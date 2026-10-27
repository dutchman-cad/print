//! Repetier-Server HTTP transport, handwritten from its public API manual.
//!
//! Websocket commands are sent through the documented REST form,
//! `/printer/api/<slug>?a=<command>&data=<json>`, with the key in the `x-api-key` header.

use crate::backend::{Error, Printer, PrinterState};
use axum::body::Bytes;
use axum::http::{Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub(crate) fn controls() -> Vec<&'static str> {
    vec!["start", "cancel", "clear"]
}

const STATUS_TIMEOUT: Duration = Duration::from_secs(4);
/// Model and job lists carry per-file statistics, so they get more room than status.
const MAX_REPLY: usize = 4 << 20;
const MAX_ENTRIES: usize = 5000;
pub const IDLE: &str = "online, no job";
pub const ACTIVE: &str = "online, job active";
pub const PAUSE: &str = "Repetier-Server documents no pause command and reports no paused state. Pause the job in Repetier-Server.";
pub const RESUME: &str = "Repetier-Server reports no paused state, so a paused job cannot be verified. Resume the job in Repetier-Server.";
pub const STOP: &str = "Repetier-Server's stopJob cannot name the job it stops, so Stop is available only for a job this companion started. Stop other jobs in Repetier-Server.";
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

pub struct Client {
    config: crate::config::Printer,
    slug: String,
    uri: Uri,
    tls: Arc<rustls::ClientConfig>,
    observation: tokio::sync::Mutex<Option<Value>>,
    changed: std::sync::atomic::AtomicBool,
}
/// A stored model or a queued job, as `listModels` and `listJobs` list them.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub name: String,
    pub length: Option<u64>,
}
pub struct Snapshot {
    pub printer: Printer,
    /// The `listPrinter` `job` value while the printer is online and runs a job.
    pub job: Option<String>,
    /// The print queue from `listJobs`.
    pub jobs: Vec<Entry>,
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
        .map_err(|_| "Set a Repetier-Server http or https URL.")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.host().is_none()
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.path_and_query().is_some_and(|p| p.query().is_some())
        || uri.path().contains(['%', '\\'])
        || uri.path().split('/').any(|p| matches!(p, "." | ".."))
    {
        return Err("Set a Repetier-Server http or https URL without credentials, query, or escaped path segments.".into());
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
        return Err("Set only api_key for Repetier-Server authentication. HTTPS requires a normally trusted certificate.".into());
    }
    if config.slug.as_ref().is_none_or(|s| {
        s.is_empty()
            || s.len() > 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    }) {
        return Err("Set slug to the Repetier-Server printer slug: 1–64 letters, digits, hyphens, or underscores.".into());
    }
    if config
        .nozzle_diameter
        .is_some_and(|d| !d.is_finite() || d <= 0.0 || d > 10.0)
    {
        return Err("Set nozzle_diameter in millimetres, greater than 0 and at most 10.".into());
    }
    Ok(())
}
/// Percent-encodes a query value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
/// A raw value for `detail`, shortened and limited to printable ASCII.
fn shown(value: Option<&Value>) -> String {
    match value {
        None => "missing".into(),
        Some(Value::String(s)) => format!("'{}'", crate::snapmaker::plain(s, 32)),
        Some(other) => crate::snapmaker::plain(&other.to_string(), 32),
    }
}
/// The staged model's name: the companion-built file name without its extension. A server
/// that keeps the extension is matched too.
pub fn stem(filename: &str) -> &str {
    filename.strip_suffix(".gcode").unwrap_or(filename)
}
pub fn named(name: &str, filename: &str) -> bool {
    name == stem(filename) || name == filename
}
fn entries(reply: &Value) -> Result<Vec<Entry>, Error> {
    let list = reply
        .get("data")
        .and_then(Value::as_array)
        .ok_or(Error::Unavailable)?;
    if list.len() > MAX_ENTRIES {
        return Err(Error::Unavailable);
    }
    list.iter()
        .map(|e| {
            Ok(Entry {
                id: e
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or(Error::Unavailable)?,
                name: e
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(Error::Unavailable)?
                    .chars()
                    .take(256)
                    .collect(),
                length: e.get("length").and_then(Value::as_u64),
            })
        })
        .collect()
}
impl Client {
    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        validate(&config)?;
        let uri: Uri = config
            .url
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| "Check Repetier-Server URL.")?;
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
            slug: config.slug.clone().unwrap_or_default(),
            config,
            uri,
            tls: Arc::new(tls),
            observation: tokio::sync::Mutex::new(None),
            changed: std::sync::atomic::AtomicBool::new(false),
        })
    }
    /// Sends one request. The key goes only in the `x-api-key` header, never in a URL.
    async fn request(
        &self,
        method: Method,
        target: &str,
        body: Bytes,
        content_type: Option<&str>,
        upload: bool,
    ) -> Result<(StatusCode, Bytes), Error> {
        let (progress, updates) = crate::progress::channel();
        let size = body.len() as u64;
        let operation = async {
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
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(
                crate::progress::Stream::new(stream, progress),
            ))
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
                .uri(format!(
                    "{}{}",
                    self.uri.path().trim_end_matches('/'),
                    target
                ))
                .header(
                    "Host",
                    self.uri.authority().ok_or(Error::Unavailable)?.as_str(),
                )
                .header(
                    "x-api-key",
                    self.config.api_key.as_deref().unwrap_or_default(),
                )
                .header("Accept", "application/json")
                .header("Content-Length", body.len());
            if let Some(content_type) = content_type {
                request = request.header("Content-Type", content_type);
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
            let bytes = Limited::new(response.into_body(), MAX_REPLY)
                .collect()
                .await
                .map_err(|_| Error::Unavailable)?
                .to_bytes();
            Ok((code, bytes))
        };
        if upload {
            crate::progress::timeout(
                Duration::from_secs(30),
                crate::progress::REPLY,
                updates,
                size,
                operation,
            )
            .await
            .map_err(|_| Error::Unavailable)?
        } else {
            tokio::time::timeout(STATUS_TIMEOUT, operation)
                .await
                .map_err(|_| Error::Unavailable)?
        }
    }
    /// The manual's error reply is `{"error":"..."}`; its text is never passed on.
    fn checked(code: StatusCode, bytes: &[u8]) -> Result<Value, Error> {
        if code != StatusCode::OK {
            return Err(Error::Unavailable);
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| Error::Unavailable)?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return Err(if error.to_ascii_lowercase().contains("authoriz") {
                Error::Authentication
            } else {
                refuse(
                    "Repetier-Server refused the request. Read fresh status before another action.",
                )
            });
        }
        Ok(value)
    }
    /// Sends one websocket command through the REST form. Reads use GET; commands that
    /// change anything use POST, which the manual allows equally.
    pub async fn command(&self, action: &str, data: Value, change: bool) -> Result<Value, Error> {
        let data = serde_json::to_string(&data).map_err(|_| Error::Unavailable)?;
        let (code, bytes) = self
            .request(
                if change { Method::POST } else { Method::GET },
                &format!(
                    "/printer/api/{}?a={}&data={}",
                    self.slug,
                    encode(action),
                    encode(&data)
                ),
                Bytes::new(),
                None,
                false,
            )
            .await?;
        Self::checked(code, &bytes)
    }
    pub async fn models(&self) -> Result<Vec<Entry>, Error> {
        entries(&self.command("listModels", json!({}), false).await?)
    }
    pub async fn jobs(&self) -> Result<Vec<Entry>, Error> {
        entries(&self.command("listJobs", json!({}), false).await?)
    }
    /// Asks the documented free-space check before an upload.
    pub async fn can_upload(&self, size: usize) -> Result<(), Error> {
        let reply = self
            .command("canUploadGCodeWithSize", json!({"size":size}), false)
            .await?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(refuse(
                "Repetier-Server reports too little free disk space for this G-code. Free space on the server, then start again.",
            ));
        }
        Ok(())
    }
    /// Uploads G-code into the printer's model list, in the multipart field `filename`.
    pub async fn upload(&self, filename: &str, bytes: &[u8]) -> Result<(), Error> {
        if !crate::duet::safe_filename(filename) || bytes.len() > 200 << 20 {
            return Err(Error::Unavailable);
        }
        let boundary = format!("dutchman{}", crate::new_job_id().map_err(Error::Failed)?);
        if bytes
            .windows(boundary.len())
            .any(|b| b == boundary.as_bytes())
        {
            return Err(Error::Unavailable);
        }
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"filename\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (code, reply) = self
            .request(
                Method::POST,
                &format!(
                    "/printer/model/{}?a=upload&name={}",
                    self.slug,
                    encode(stem(filename))
                ),
                body.into(),
                Some(&format!("multipart/form-data; boundary={boundary}")),
                true,
            )
            .await?;
        // The manual does not describe the upload reply; only an error reply is recognised.
        if code != StatusCode::OK {
            return Err(refuse(
                "Repetier-Server refused the upload. Inspect its model list before preparing again.",
            ));
        }
        if serde_json::from_slice::<Value>(&reply)
            .ok()
            .is_some_and(|v| v.get("error").is_some())
        {
            return Err(refuse(
                "Repetier-Server reported a failed upload. Inspect its model list before preparing again.",
            ));
        }
        Ok(())
    }
    /// Removes every stored model and queued job carrying the staged name, then confirms
    /// through both lists that none remain. Call it only while the printer is idle.
    pub async fn delete(&self, filename: &str) -> Result<(), Error> {
        for job in self
            .jobs()
            .await?
            .iter()
            .filter(|j| named(&j.name, filename))
        {
            let _ = self
                .command("removeJob", json!({"id":job.id}), true)
                .await?;
        }
        for model in self
            .models()
            .await?
            .iter()
            .filter(|m| named(&m.name, filename))
        {
            let _ = self
                .command("removeModel", json!({"id":model.id}), true)
                .await?;
        }
        if self.jobs().await?.iter().any(|j| named(&j.name, filename))
            || self
                .models()
                .await?
                .iter()
                .any(|m| named(&m.name, filename))
        {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    pub fn acknowledge(&self) {
        self.changed
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        tokio::time::timeout(STATUS_TIMEOUT, self.snapshot_in())
            .await
            .map_err(|_| Error::Unavailable)?
    }
    async fn snapshot_in(&self) -> Result<Snapshot, Error> {
        let list = self.command("listPrinter", json!({}), false).await?;
        let list = list.as_array().ok_or(Error::Unavailable)?;
        if list.len() > MAX_ENTRIES {
            return Err(Error::Unavailable);
        }
        let mut printer = self.offline(&Error::Unavailable);
        let Some(entry) = list
            .iter()
            .find(|p| p.get("slug").and_then(Value::as_str) == Some(self.slug.as_str()))
        else {
            printer.state = PrinterState::Error;
            printer.detail = Some(
                "Repetier-Server lists no printer with the configured slug. Check slug.".into(),
            );
            return Ok(Snapshot {
                printer,
                job: None,
                jobs: Vec::new(),
            });
        };
        let online = entry.get("online");
        let active = entry.get("active").and_then(Value::as_bool);
        let job = entry.get("job");
        let unknown = || {
            format!(
                "Unknown Repetier-Server state: online {}, active {}, job {}. Read fresh status.",
                shown(online),
                shown(entry.get("active")),
                shown(job)
            )
        };
        let (state, detail, live) = match (online.and_then(Value::as_i64), active) {
            (Some(0), Some(false)) => (PrinterState::Offline, "deactivated".to_owned(), None),
            (Some(0), _) => (PrinterState::Offline, "offline".to_owned(), None),
            (Some(1), Some(true)) => match job.and_then(Value::as_str) {
                Some("none") => (PrinterState::Idle, IDLE.to_owned(), None),
                Some(_) => match text(job) {
                    Some(name) => (PrinterState::Printing, ACTIVE.to_owned(), Some(name)),
                    None => (PrinterState::Error, unknown(), None),
                },
                None => (PrinterState::Error, unknown(), None),
            },
            _ => (PrinterState::Error, unknown(), None),
        };
        printer.state = state;
        printer.detail = Some(detail);
        let jobs = self.jobs().await?;
        if state != PrinterState::Offline {
            let states = self
                .command("stateList", json!({"includeHistory":false}), false)
                .await?;
            let own = states.get(&self.slug).ok_or(Error::Unavailable)?;
            let reading = |v: Option<&Value>| {
                v.and_then(|v| v.get("tempRead"))
                    .and_then(Value::as_f64)
                    .filter(|t| t.is_finite())
            };
            let extruders = own.get("extruder").and_then(Value::as_array);
            let active = own
                .get("activeExtruder")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .unwrap_or(0);
            printer.nozzle_temperature = reading(extruders.and_then(|e| e.get(active)));
            printer.bed_temperature = reading(own.get("heatedBed"));
        }
        printer.job_name = live.clone();
        // A job has an id only when exactly one queued job carries the live name.
        printer.job_id = live.as_deref().and_then(|name| {
            let mut matching = jobs.iter().filter(|j| j.name == name);
            match (matching.next(), matching.next()) {
                (Some(job), None) => Some(job.id.to_string()),
                _ => None,
            }
        });
        let observed =
            json!({"state":printer.state,"detail":printer.detail,"job":live,"id":printer.job_id});
        let mut previous = self.observation.lock().await;
        if previous.as_ref().is_some_and(|old| old != &observed) {
            self.changed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        *previous = Some(observed);
        printer.needs_clearing = Some(self.changed.load(std::sync::atomic::Ordering::SeqCst));
        Ok(Snapshot {
            printer,
            job: live,
            jobs,
        })
    }
    /// Server identity, machine configuration, and tooling reviewed before upload and start.
    pub async fn review(&self) -> Result<Value, Error> {
        let version = self.command("version", json!({}), false).await?;
        let config = self
            .command("getPrinterConfig", json!({"printer":self.slug}), false)
            .await?;
        if config.pointer("/general/slug").and_then(Value::as_str) != Some(self.slug.as_str()) {
            return Err(refuse(
                "Repetier-Server returned another printer's configuration. Check slug.",
            ));
        }
        let extruders = config
            .get("extruders")
            .and_then(Value::as_array)
            .ok_or_else(|| refuse("Repetier-Server printer configuration lists no extruders. Review the printer configuration."))?;
        if extruders.len() != 1 {
            return Err(refuse(
                "Material slot assignments are unresolved. Use a reviewed single-extruder printer.",
            ));
        }
        let nozzle = self.config.nozzle_diameter.ok_or_else(|| {
            refuse("Repetier-Server does not report a nozzle diameter. Check the installed nozzle and set nozzle_diameter for this printer before preparing.")
        })?;
        let movement = config.get("movement");
        let volume = ["x", "y", "z"]
            .iter()
            .map(|axis| {
                let min = movement?
                    .get(format!("{axis}Min"))
                    .and_then(Value::as_f64)?;
                let max = movement?
                    .get(format!("{axis}Max"))
                    .and_then(Value::as_f64)?;
                (max > min).then(|| json!([axis, min, max]))
            })
            .collect::<Option<Vec<_>>>();
        let server = text(version.get("name"));
        let firmware = text(config.pointer("/general/firmwareName"));
        if volume.is_none() || server.is_none() || firmware.is_none() {
            return Err(refuse("Server version, firmware, or build volume is missing. Review the Repetier-Server printer configuration."));
        }
        let identity = serde_json::to_vec(&json!({
            "server":server,"build":version.get("build"),"firmware":firmware,
            "name":config.pointer("/general/name"),"variant":config.pointer("/general/printerVariant"),
            "bed":config.pointer("/heatedBed/installed"),
        }))
        .map_err(|_| Error::Unavailable)?;
        let tooling = serde_json::to_vec(
            &json!({"count":extruders.len(),"filament":extruders.first().and_then(|e| e.get("filamentDiameter"))}),
        )
        .map_err(|_| Error::Unavailable)?;
        Ok(json!({
            "endpoint":crate::safety::hash(format!("{}#{}", self.config.url.as_deref().unwrap_or_default(), self.slug).as_bytes()),
            "identity":crate::safety::hash(&identity),
            "tooling":crate::safety::hash(&tooling),
            "info":{"nozzle_diameter":nozzle},
            "volume":volume,
        }))
    }
    pub fn offline(&self, error: &Error) -> Printer {
        Printer {
            controls: controls().into_iter().map(String::from).collect(),
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state: PrinterState::Offline,
            detail: Some(
                if matches!(error, Error::Authentication) {
                    "Authentication failed. Check the Repetier-Server API key."
                } else {
                    "Fresh Repetier-Server telemetry unavailable."
                }
                .into(),
            ),
            needs_clearing: None,
            job_path: None,
            job_id: None,
            job_name: None,
            progress: None,
            seconds_left: None,
            nozzle_temperature: None,
            bed_temperature: None,
        }
    }
}
pub fn ready(live: &Snapshot) -> Result<(), Error> {
    if live.printer.state != PrinterState::Idle
        || live.printer.detail.as_deref() != Some(IDLE)
        || live.job.is_some()
    {
        return Err(refuse("Repetier-Server readiness is unknown or the printer is busy. Wait for fresh idle status before starting."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(encode("{\"id\":3}"), "%7B%22id%22%3A3%7D");
        assert_eq!(encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn staged_names_match_with_or_without_extension() {
        assert!(named("dutchman-ab", "dutchman-ab.gcode"));
        assert!(named("dutchman-ab.gcode", "dutchman-ab.gcode"));
        assert!(!named("dutchman-a", "dutchman-ab.gcode"));
    }
}
