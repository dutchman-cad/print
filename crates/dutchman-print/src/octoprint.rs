//! OctoPrint REST transport, handwritten from its public documentation.

use crate::backend::{Error, Printer, PrinterState};
use axum::body::Bytes;
use axum::http::{Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

const STATUS_TIMEOUT: Duration = Duration::from_secs(4);
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

pub struct Client {
    config: crate::config::Printer,
    uri: Uri,
    tls: Arc<rustls::ClientConfig>,
}
pub struct Snapshot {
    pub printer: Printer,
    pub connection: Value,
    pub job: Value,
}
fn refuse(message: &str) -> Error {
    Error::Refused(message.into())
}
pub fn validate(config: &crate::config::Printer) -> Result<(), String> {
    let uri: Uri = config
        .url
        .as_deref()
        .unwrap_or_default()
        .parse()
        .map_err(|_| "Set an OctoPrint http or https URL.")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.host().is_none()
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.path_and_query().is_some_and(|p| p.query().is_some())
        || uri.path().contains(['%', '\\'])
        || uri.path().split('/').any(|p| matches!(p, "." | ".."))
    {
        return Err("Set an OctoPrint http or https URL without credentials, query, or escaped path segments.".into());
    }
    if let Some(authority) = uri.authority() {
        let suffix = authority
            .as_str()
            .strip_prefix(authority.host())
            .unwrap_or_default();
        if !suffix.is_empty()
            && suffix
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .is_none_or(|p| p == 0)
        {
            return Err("Set a printer port between 1 and 65535.".into());
        }
    }
    if config.api_key.as_ref().is_none_or(|k| {
        k.is_empty() || k.len() > 1024 || !k.is_ascii() || k.chars().any(char::is_control)
    }) || config.username.is_some()
        || config.password.is_some()
        || config.ca_file.is_some()
        || config.cert_sha256.is_some()
    {
        return Err("Set only api_key for OctoPrint authentication. HTTPS requires a normally trusted certificate.".into());
    }
    Ok(())
}
impl Client {
    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        validate(&config)?;
        let uri: Uri = config
            .url
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| "Check OctoPrint URL.")?;
        let mut roots = rustls::RootCertStore::empty();
        if uri.scheme_str() == Some("https") {
            for cert in rustls_native_certs::load_native_certs().certs {
                roots
                    .add(cert)
                    .map_err(|_| "Cannot load system certificate trust.")?;
            }
            if roots.is_empty() {
                return Err("No system certificate trust is available for HTTPS.".into());
            }
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot configure HTTPS.")?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            config,
            uri,
            tls: Arc::new(tls),
        })
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        content_type: &str,
        upload: bool,
    ) -> Result<(StatusCode, Bytes), Error> {
        let deadline = if upload {
            Duration::from_secs(30 + body.len() as u64 / (32 * 1024))
        } else {
            STATUS_TIMEOUT
        };
        tokio::time::timeout(deadline, async {
            let host = self
                .uri
                .host()
                .ok_or(Error::Unavailable)?
                .trim_matches(['[', ']']);
            let https = self.uri.scheme_str() == Some("https");
            let socket = tokio::net::TcpStream::connect((
                host,
                self.uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
            ))
            .await
            .map_err(|_| Error::Unavailable)?;
            let stream: Box<dyn Stream> = if https {
                let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                    .map_err(|_| Error::Unavailable)?;
                Box::new(
                    tokio_rustls::TlsConnector::from(self.tls.clone())
                        .connect(name, socket)
                        .await
                        .map_err(|_| Error::Unavailable)?,
                )
            } else {
                Box::new(socket)
            };
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .map_err(|_| Error::Unavailable)?;
            struct Driver(tokio::task::JoinHandle<()>);
            impl Drop for Driver {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _driver = Driver(tokio::spawn(async move {
                let _ = connection.await;
            }));
            let request = Request::builder()
                .method(method)
                .uri(format!("{}{}", self.uri.path().trim_end_matches('/'), path))
                .header(
                    "Host",
                    self.uri.authority().ok_or(Error::Unavailable)?.as_str(),
                )
                .header(
                    "X-Api-Key",
                    self.config.api_key.as_deref().unwrap_or_default(),
                )
                .header("Accept", "application/json")
                .header("Content-Type", content_type)
                .header("Content-Length", body.len())
                .body(Full::new(body))
                .map_err(|_| Error::Unavailable)?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|_| Error::Unavailable)?;
            let code = response.status();
            // Backend bodies and redirects are never forwarded to callers or followed.
            if matches!(code, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                return Err(Error::Authentication);
            }
            let bytes = Limited::new(response.into_body(), 1 << 20)
                .collect()
                .await
                .map_err(|_| Error::Unavailable)?
                .to_bytes();
            Ok((code, bytes))
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
    pub async fn json(&self, path: &str) -> Result<Value, Error> {
        let (code, bytes) = self
            .request(Method::GET, path, Bytes::new(), "application/json", false)
            .await?;
        if code != StatusCode::OK {
            return Err(Error::Unavailable);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Unavailable)
    }
    pub async fn command(&self, path: &str, value: Value) -> Result<(), Error> {
        let body = serde_json::to_vec(&value).map_err(|_| Error::Unavailable)?;
        let (code, _) = self
            .request(Method::POST, path, body.into(), "application/json", false)
            .await?;
        if code != StatusCode::NO_CONTENT {
            return Err(refuse(
                "OctoPrint refused the command. Read fresh status before another action.",
            ));
        }
        Ok(())
    }
    pub async fn upload(&self, filename: &str, bytes: &[u8]) -> Result<(), Error> {
        if !filename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        {
            return Err(Error::Unavailable);
        }
        let boundary = format!("dutchman{}", crate::new_job_id().map_err(Error::Failed)?);
        if bytes
            .windows(boundary.len())
            .any(|b| b == boundary.as_bytes())
        {
            return Err(Error::Unavailable);
        }
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"select\"\r\n\r\nfalse\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"print\"\r\n\r\nfalse\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (code, reply) = self
            .request(
                Method::POST,
                "/api/files/local",
                body.into(),
                &format!("multipart/form-data; boundary={boundary}"),
                true,
            )
            .await?;
        let reply: Value = serde_json::from_slice(&reply).map_err(|_| {
            refuse("OctoPrint upload validation failed. Read status and inspect storage.")
        })?;
        if code != StatusCode::CREATED
            || reply.get("done").and_then(Value::as_bool) != Some(true)
            || reply.get("effectiveSelect").and_then(Value::as_bool) == Some(true)
            || reply.get("effectivePrint").and_then(Value::as_bool) == Some(true)
        {
            return Err(refuse(
                "OctoPrint upload validation failed. Read status and inspect storage.",
            ));
        }
        Ok(())
    }
    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        tokio::time::timeout(STATUS_TIMEOUT, self.snapshot_in())
            .await
            .map_err(|_| Error::Unavailable)?
    }
    async fn snapshot_in(&self) -> Result<Snapshot, Error> {
        let connection = self.json("/api/connection").await?;
        let job = self.json("/api/job").await?;
        let connection_state = connection
            .pointer("/current/state")
            .and_then(Value::as_str)
            .ok_or(Error::Unavailable)?;
        let job_state = job
            .get("state")
            .and_then(Value::as_str)
            .ok_or(Error::Unavailable)?;
        let mut printer = self.offline(&Error::Unavailable);
        printer.detail = Some(
            match connection_state {
                "Offline" => "Offline: printer serial connection is disconnected.",
                "Offline after error" => {
                    "Offline after error: check the printer serial connection."
                }
                "Operational" | "Printing" | "Pausing" | "Cancelling" | "Paused" | "Error" => {
                    connection_state
                }
                _ => "Unknown or opening serial connection. Read fresh status.",
            }
            .into(),
        );
        if matches!(connection_state, "Offline" | "Offline after error") {
            return Ok(Snapshot {
                printer,
                connection,
                job,
            });
        }
        let data = self.json("/api/printer").await?;
        let flags = data.pointer("/state/flags").ok_or(Error::Unavailable)?;
        let flag = |key: &str| flags.get(key).and_then(Value::as_bool);
        let complete = [
            "operational",
            "paused",
            "printing",
            "pausing",
            "cancelling",
            "error",
            "ready",
            "closedOrError",
        ]
        .iter()
        .all(|key| flag(key).is_some());
        let text = data.pointer("/state/text").and_then(Value::as_str);
        printer.state = if !complete
            || connection_state != job_state
            || text != Some(job_state)
            || flag("error") == Some(true)
        {
            PrinterState::Error
        } else if flag("closedOrError") == Some(true) {
            PrinterState::Offline
        } else if flag("operational") != Some(true) {
            PrinterState::Error
        } else if matches!(job_state, "Pausing" | "Cancelling")
            && flag(if job_state == "Pausing" {
                "pausing"
            } else {
                "cancelling"
            }) == Some(true)
            && flag("paused") == Some(false)
            && flag(if job_state == "Pausing" {
                "cancelling"
            } else {
                "pausing"
            }) == Some(false)
        {
            PrinterState::Printing
        } else if job_state == "Paused"
            && flag("paused") == Some(true)
            && flag("printing") == Some(false)
            && flag("pausing") == Some(false)
            && flag("cancelling") == Some(false)
        {
            PrinterState::Paused
        } else if job_state == "Printing"
            && flag("printing") == Some(true)
            && flag("paused") == Some(false)
            && flag("pausing") == Some(false)
            && flag("cancelling") == Some(false)
        {
            PrinterState::Printing
        } else if job_state == "Operational"
            && flag("ready") == Some(true)
            && ["printing", "paused", "pausing", "cancelling"]
                .iter()
                .all(|k| flag(k) == Some(false))
        {
            PrinterState::Idle
        } else {
            PrinterState::Error
        };
        printer.job_name = job
            .pointer("/job/file/display")
            .and_then(Value::as_str)
            .or_else(|| job.pointer("/job/file/name").and_then(Value::as_str))
            .filter(|s| s.len() <= 256 && !s.chars().any(char::is_control))
            .map(str::to_owned);
        printer.job_id = file_identity(&job);
        printer.progress = job
            .pointer("/progress/completion")
            .and_then(Value::as_f64)
            .filter(|v| (0.0..=100.0).contains(v))
            .map(|v| v / 100.0);
        printer.seconds_left = job
            .pointer("/progress/printTimeLeft")
            .and_then(Value::as_u64);
        printer.nozzle_temperature = data
            .pointer("/temperature/tool0/actual")
            .and_then(Value::as_f64);
        printer.bed_temperature = data
            .pointer("/temperature/bed/actual")
            .and_then(Value::as_f64);
        Ok(Snapshot {
            printer,
            connection,
            job,
        })
    }
    pub fn offline(&self, error: &Error) -> Printer {
        Printer {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state: PrinterState::Offline,
            detail: Some(
                if matches!(error, Error::Authentication) {
                    "Authentication failed. Check OctoPrint API key permissions."
                } else {
                    "Fresh OctoPrint telemetry unavailable."
                }
                .into(),
            ),
            needs_clearing: None,
            job_id: None,
            job_name: None,
            progress: None,
            seconds_left: None,
            nozzle_temperature: None,
            bed_temperature: None,
        }
    }
    pub async fn review(&self, snapshot: &Snapshot) -> Result<Value, Error> {
        let profiles = self.json("/api/printerprofiles").await?;
        let id = snapshot
            .connection
            .pointer("/current/printerProfile")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                refuse("OctoPrint printer identity is missing. Review its connection and profile.")
            })?;
        let profile = profiles
            .get("profiles")
            .and_then(|p| p.get(id))
            .ok_or_else(|| {
                refuse("OctoPrint printer profile is missing. Review its configuration.")
            })?;
        let version = self.json("/api/version").await?;
        if profile.get("id").and_then(Value::as_str) != Some(id)
            || ["width", "depth", "height"].iter().any(|key| {
                profile
                    .get("volume")
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_f64)
                    .is_none_or(|v| v <= 0.0)
            })
            || profile
                .pointer("/extruder/nozzleDiameter")
                .and_then(Value::as_f64)
                .is_none_or(|v| v <= 0.0)
            || snapshot
                .connection
                .pointer("/current/port")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || version
                .get("server")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(refuse("OctoPrint connection, build volume, nozzle, or server version is missing. Review its configuration."));
        }
        if profile.pointer("/extruder/count").and_then(Value::as_u64) != Some(1) {
            return Err(refuse(
                "Material slot assignments are unresolved. Use a reviewed single-extruder printer.",
            ));
        }
        Ok(
            json!({"endpoint":crate::safety::hash(self.config.url.as_deref().unwrap_or_default().as_bytes()), "connection":snapshot.connection.get("current"), "version":version,
            "profile":{"id":profile.get("id"),"model":profile.get("model"),"volume":profile.get("volume"),"extruder":profile.get("extruder"),"heatedBed":profile.get("heatedBed")}}),
        )
    }
}
pub fn file_identity(job: &Value) -> Option<String> {
    let file = job.pointer("/job/file")?;
    let name = file.get("name")?.as_str()?;
    let origin = file.get("origin")?.as_str()?;
    if name.is_empty()
        || name.len() > 256
        || name.chars().any(char::is_control)
        || !matches!(origin, "local" | "sdcard")
    {
        return None;
    }
    Some(format!("{origin}/{name}"))
}
pub fn ready(snapshot: &Snapshot) -> Result<(), Error> {
    if snapshot.printer.state != PrinterState::Idle
        || snapshot.printer.detail.as_deref() != Some("Operational")
    {
        return Err(refuse("OctoPrint readiness is unknown, busy, or the serial connection is disconnected. Read fresh status and resolve the printer problem."));
    }
    Ok(())
}
pub fn history(file: &Value) -> Value {
    file.pointer("/prints/last")
        .or_else(|| file.pointer("/print/last"))
        .cloned()
        .unwrap_or(Value::Null)
}
