//! Moonraker REST transport, handwritten from its public documentation.

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
    observation: tokio::sync::Mutex<Option<Value>>,
    changed: std::sync::atomic::AtomicBool,
}
pub struct Snapshot {
    pub printer: Printer,
    pub objects: Value,
    pub history: Value,
    pub server: Value,
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
        .map_err(|_| "Set a Moonraker http or https URL.")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.host().is_none()
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.path_and_query().is_some_and(|p| p.query().is_some())
        || uri.path().contains(['%', '\\'])
        || uri.path().split('/').any(|p| matches!(p, "." | ".."))
    {
        return Err("Set a Moonraker http or https URL without credentials, query, or escaped path segments.".into());
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
    if config.api_key.as_ref().is_some_and(|k| {
        k.is_empty() || k.len() > 1024 || !k.is_ascii() || k.chars().any(char::is_control)
    }) || config.username.is_some()
        || config.password.is_some()
        || config.ca_file.is_some()
        || config.cert_sha256.is_some()
    {
        return Err("Set only api_key for Moonraker authentication. HTTPS requires a normally trusted certificate.".into());
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
            .map_err(|_| "Check Moonraker URL.")?;
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
            observation: tokio::sync::Mutex::new(None),
            changed: std::sync::atomic::AtomicBool::new(false),
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
            let mut request = Request::builder()
                .method(method)
                .uri(format!("{}{}", self.uri.path().trim_end_matches('/'), path))
                .header(
                    "Host",
                    self.uri.authority().ok_or(Error::Unavailable)?.as_str(),
                )
                .header("Accept", "application/json")
                .header("Content-Type", content_type)
                .header("Content-Length", body.len());
            if let Some(key) = &self.config.api_key {
                request = request.header("X-Api-Key", key);
            }
            let request = request
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
        result(&bytes)
    }
    pub async fn command(&self, path: &str) -> Result<(), Error> {
        let (code, body) = self
            .request(Method::POST, path, Bytes::new(), "application/json", false)
            .await?;
        if code != StatusCode::OK || result(&body)? != json!("ok") {
            return Err(refuse(
                "Moonraker refused the command. Read fresh status before another action.",
            ));
        }
        Ok(())
    }
    pub async fn upload(&self, filename: &str, bytes: &[u8]) -> Result<(), Error> {
        if !safe_filename(filename) || bytes.len() > 200 << 20 {
            return Err(Error::Unavailable);
        }
        let boundary = format!("dutchman{}", crate::new_job_id().map_err(Error::Failed)?);
        if bytes
            .windows(boundary.len())
            .any(|b| b == boundary.as_bytes())
        {
            return Err(Error::Unavailable);
        }
        let checksum = crate::safety::hash(bytes);
        let mut body = Vec::new();
        for (key, value) in [
            ("root", "gcodes"),
            ("print", "false"),
            ("checksum", checksum.as_str()),
        ] {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n").as_bytes());
        }
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (code, body) = self
            .request(
                Method::POST,
                "/server/files/upload",
                body.into(),
                &format!("multipart/form-data; boundary={boundary}"),
                true,
            )
            .await?;
        let reply = result(&body)?;
        if code != StatusCode::CREATED
            || reply.get("print_started").and_then(Value::as_bool) != Some(false)
            || reply.get("print_queued").and_then(Value::as_bool) != Some(false)
            || reply.pointer("/item/root").and_then(Value::as_str) != Some("gcodes")
            || reply.pointer("/item/path").and_then(Value::as_str) != Some(filename)
            || reply.pointer("/item/size").and_then(Value::as_u64) != Some(bytes.len() as u64)
        {
            return Err(refuse(
                "Moonraker upload validation failed. Inspect storage before starting.",
            ));
        }
        Ok(())
    }
    pub fn acknowledge(&self) {
        self.changed
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    pub async fn delete(&self, filename: &str) -> Result<(), Error> {
        let (code, body) = self
            .request(
                Method::DELETE,
                &format!("/server/files/gcodes/{}", encode(filename)),
                Bytes::new(),
                "application/json",
                false,
            )
            .await?;
        if code == StatusCode::NOT_FOUND {
            return Ok(());
        }
        let reply = result(&body)?;
        if code != StatusCode::OK
            || reply.pointer("/item/path").and_then(Value::as_str) != Some(filename)
            || reply.pointer("/item/root").and_then(Value::as_str) != Some("gcodes")
        {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    pub async fn metadata(&self, filename: &str) -> Result<Value, Error> {
        self.json(&format!(
            "/server/files/metadata?filename={}",
            encode(filename)
        ))
        .await
    }
    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        tokio::time::timeout(STATUS_TIMEOUT, self.snapshot_in())
            .await
            .map_err(|_| Error::Unavailable)?
    }
    async fn snapshot_in(&self) -> Result<Snapshot, Error> {
        let server = self.json("/server/info").await?;
        let mut printer = self.offline(&Error::Unavailable);
        let connected = server.get("klippy_connected").and_then(Value::as_bool);
        let klippy = server.get("klippy_state").and_then(Value::as_str);
        if connected != Some(true) || klippy != Some("ready") {
            printer.state =
                if connected == Some(false) || matches!(klippy, Some("disconnected" | "startup")) {
                    PrinterState::Offline
                } else {
                    PrinterState::Error
                };
            printer.detail = Some(
                match klippy {
                    Some("disconnected") => "Klippy disconnected. Restore the printer connection.",
                    Some("startup") => "Klippy startup. Wait for fresh ready status.",
                    Some("shutdown") => "Klippy shutdown. Resolve the printer fault.",
                    Some("error") => "Klippy error. Resolve the printer fault.",
                    _ => "Klippy readiness is unknown. Read fresh status.",
                }
                .into(),
            );
            return Ok(Snapshot {
                printer,
                server,
                objects: Value::Null,
                history: Value::Null,
            });
        }
        let available = self.json("/printer/objects/list").await?;
        let available = available
            .get("objects")
            .and_then(Value::as_array)
            .ok_or(Error::Unavailable)?;
        if available.len() > 1024 {
            return Err(Error::Unavailable);
        }
        let names = [
            "webhooks",
            "print_stats",
            "virtual_sdcard",
            "display_status",
            "extruder",
            "heater_bed",
            "toolhead",
            "configfile",
            "pause_resume",
        ];
        let mut query = names
            .iter()
            .filter(|name| available.iter().any(|v| v.as_str() == Some(**name)))
            .copied()
            .collect::<Vec<_>>()
            .join("&");
        for name in available.iter().filter_map(Value::as_str).filter(|s| {
            *s == "mcu"
                || s.starts_with("mcu ")
                || s.strip_prefix("extruder")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }) {
            if name.len() > 128 || query.len() > 8192 {
                return Err(Error::Unavailable);
            }
            query.push('&');
            query.push_str(&encode(name));
        }
        let data = self
            .json(&format!("/printer/objects/query?{query}"))
            .await?;
        if data
            .get("eventtime")
            .and_then(Value::as_f64)
            .is_none_or(|t| t < 0.0)
        {
            return Err(Error::Unavailable);
        }
        let objects = data.get("status").cloned().ok_or(Error::Unavailable)?;
        let state = objects
            .pointer("/print_stats/state")
            .and_then(Value::as_str);
        let webhook = objects.pointer("/webhooks/state").and_then(Value::as_str);
        let active = objects
            .pointer("/virtual_sdcard/is_active")
            .and_then(Value::as_bool);
        printer.state = if webhook == Some("startup") {
            PrinterState::Offline
        } else if webhook != Some("ready") {
            PrinterState::Error
        } else {
            match (state, active) {
                (Some("standby" | "complete" | "cancelled"), Some(false)) => PrinterState::Idle,
                (Some("printing"), Some(true)) => PrinterState::Printing,
                (Some("paused"), Some(false)) => PrinterState::Paused,
                _ => PrinterState::Error,
            }
        };
        printer.detail = Some(
            if webhook != Some("ready") {
                match webhook {
                    Some("shutdown") => "Klippy shutdown. Resolve the printer fault.",
                    Some("startup") => "Klippy startup. Wait for fresh ready status.",
                    _ => "Klippy readiness is unknown. Read fresh status.",
                }
            } else if printer.state == PrinterState::Error && state != Some("error") {
                "Unknown or contradictory printer state. Read fresh status."
            } else {
                match state {
                    Some("standby") => "standby",
                    Some("complete") => "complete",
                    Some("cancelled") => "cancelled",
                    Some("printing") => "printing",
                    Some("paused") => "paused",
                    _ => "error",
                }
            }
            .into(),
        );
        printer.job_name = objects
            .pointer("/print_stats/filename")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .map(str::to_owned);
        printer.progress = objects
            .pointer("/display_status/progress")
            .and_then(Value::as_f64)
            .filter(|p| (0.0..=1.0).contains(p))
            .or_else(|| {
                objects
                    .pointer("/virtual_sdcard/progress")
                    .and_then(Value::as_f64)
                    .filter(|p| (0.0..=1.0).contains(p))
            });
        let extruder = objects
            .pointer("/toolhead/extruder")
            .and_then(Value::as_str)
            .unwrap_or("extruder");
        printer.nozzle_temperature = objects
            .get(extruder)
            .and_then(|v| v.get("temperature"))
            .and_then(Value::as_f64);
        printer.bed_temperature = objects
            .pointer("/heater_bed/temperature")
            .and_then(Value::as_f64);
        let jobs = self.json("/server/history/list?limit=1&order=desc").await?;
        let jobs = jobs
            .get("jobs")
            .and_then(Value::as_array)
            .filter(|j| j.len() <= 1)
            .ok_or(Error::Unavailable)?;
        let latest = jobs
            .first()
            .map(|job| {
                job.get("start_time")
                    .and_then(Value::as_f64)
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .map(|time| json!({"id":job.get("job_id"),"start":time,"end":job.get("end_time"),"status":job.get("status")}))
                    .ok_or(Error::Unavailable)
            })
            .transpose()?;
        let observed = json!({"state":state,"filename":printer.job_name,"latest":latest});
        let mut previous = self.observation.lock().await;
        if previous.as_ref().is_some_and(|old| old != &observed) {
            self.changed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        *previous = Some(observed);
        printer.needs_clearing = Some(self.changed.load(std::sync::atomic::Ordering::SeqCst));
        // Metadata links a filename to an execution; history distinguishes repeated prints of that file.
        let mut history = Value::Null;
        if let Some(name) = &printer.job_name {
            let metadata = self.metadata(name).await?;
            if metadata.get("filename").and_then(Value::as_str) != Some(name.as_str()) {
                return Err(Error::Unavailable);
            }
            if let Some(id) = metadata.get("job_id").and_then(Value::as_str).filter(|s| {
                !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric())
            }) {
                let record = self.json(&format!("/server/history/job?uid={id}")).await?;
                history = record.get("job").cloned().ok_or(Error::Unavailable)?;
                if history.get("job_id").and_then(Value::as_str) != Some(id)
                    || history.get("filename").and_then(Value::as_str) != Some(name.as_str())
                {
                    return Err(Error::Unavailable);
                }
                printer.job_id = Some(id.into());
            }
        }
        Ok(Snapshot {
            printer,
            server,
            objects,
            history,
        })
    }
    pub async fn review(&self, live: &Snapshot) -> Result<Value, Error> {
        let host = self.json("/printer/info").await?;
        let settings = live
            .objects
            .pointer("/configfile/settings")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                refuse("Machine configuration is missing. Review the printer before preparing.")
            })?;
        if live
            .objects
            .pointer("/toolhead/extruder")
            .and_then(Value::as_str)
            != Some("extruder")
            || settings
                .keys()
                .any(|k| k.starts_with("extruder") && k != "extruder")
        {
            return Err(refuse(
                "Material slot assignments are unresolved. Use a reviewed single-extruder printer.",
            ));
        }
        let nozzle = settings
            .get("extruder")
            .and_then(|v| v.get("nozzle_diameter"))
            .and_then(Value::as_f64);
        let mut controllers = std::collections::BTreeMap::new();
        for name in settings
            .keys()
            .filter(|s| *s == "mcu" || s.starts_with("mcu "))
        {
            let controller = live.objects.get(name).ok_or_else(|| refuse("Controller firmware information is missing. Read fresh status and review the printer."))?;
            if controller
                .get("mcu_version")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(refuse("Controller firmware information is missing. Read fresh status and review the printer."));
            }
            controllers.insert(name, json!({"version":controller.get("mcu_version"),"build":controller.get("mcu_build_versions"),"constants":controller.get("mcu_constants")}));
        }
        let bounds = |key: &str| {
            live.objects
                .get("toolhead")
                .and_then(|v| v.get(key))
                .and_then(Value::as_array)
        };
        let volume = bounds("axis_minimum")
            .zip(bounds("axis_maximum"))
            .is_some_and(|(min, max)| {
                min.len() >= 3
                    && max.len() >= 3
                    && min
                        .iter()
                        .zip(max)
                        .take(3)
                        .all(|(a, b)| a.as_f64().zip(b.as_f64()).is_some_and(|(a, b)| b > a))
            });
        if !volume
            || nozzle.is_none_or(|n| n <= 0.0)
            || settings.get("mcu").is_none()
            || live
                .server
                .get("moonraker_version")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || live
                .objects
                .pointer("/configfile/config")
                .and_then(Value::as_object)
                .is_none()
            || host.get("state").and_then(Value::as_str) != Some("ready")
            || ["hostname", "software_version"].iter().any(|k| {
                host.get(k)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            })
        {
            return Err(refuse("Machine identity, firmware, build volume, or nozzle information is missing. Review the printer configuration."));
        }
        let config = serde_json::to_vec(
            &json!({"settings":settings,"config":live.objects.pointer("/configfile/config"),"controllers":controllers}),
        )
        .map_err(|_| Error::Unavailable)?;
        Ok(
            json!({"endpoint":crate::safety::hash(self.config.url.as_deref().unwrap_or_default().as_bytes()),
            "identity":crate::safety::hash(&serde_json::to_vec(&json!({"hostname":host.get("hostname"),"version":host.get("software_version"),"server":live.server.get("moonraker_version")})).map_err(|_| Error::Unavailable)?),
            "info":{"nozzle_diameter":nozzle},"configuration":crate::safety::hash(&config),"min":bounds("axis_minimum"),"max":bounds("axis_maximum")}),
        )
    }
    pub fn resume_commissioned(&self) -> bool {
        self.config.resume_commissioned
    }
    pub fn offline(&self, error: &Error) -> Printer {
        Printer {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state: PrinterState::Offline,
            detail: Some(
                if matches!(error, Error::Authentication) {
                    "Authentication failed. Check the Moonraker API key."
                } else {
                    "Fresh Moonraker telemetry unavailable."
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
}
fn result(bytes: &[u8]) -> Result<Value, Error> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Error::Unavailable)?;
    value.get("result").cloned().ok_or(Error::Unavailable)
}
fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
}
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
pub fn ready(live: &Snapshot) -> Result<(), Error> {
    if live.printer.state != PrinterState::Idle {
        return Err(refuse("Moonraker readiness is unknown or the printer is busy. Restore Klippy ready status before starting."));
    }
    Ok(())
}
