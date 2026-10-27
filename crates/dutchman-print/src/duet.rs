//! Shared Duet object model with standalone and DSF HTTP transports, handwritten from public documentation.

use crate::backend::{Error, Printer, PrinterState};
use axum::body::Bytes;
use axum::http::{Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

const STATUS_TIMEOUT: Duration = Duration::from_secs(4);
/// Conservative DSF lifetime, also used when standalone omits its documented idle timeout.
const DEFAULT_SESSION: Duration = Duration::from_secs(8);
pub const SESSIONS_FULL: &str = "Duet has no free sessions. Close another connected client or wait for its session to expire, then read status again.";
const EXPIRED: &str = "The Duet session expired before the command was accepted. It was not resent; read fresh status before another action.";
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

struct Session {
    key: Option<String>,
    connected: bool,
    last: Instant,
    timeout: Duration,
    generation: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Standalone,
    Sbc,
}
struct Http {
    uri: Uri,
    tls: Arc<rustls::ClientConfig>,
}
pub struct Client {
    config: crate::config::Printer,
    transport: Transport,
    http: Arc<Http>,
    session: tokio::sync::Mutex<Session>,
    observation: tokio::sync::Mutex<Option<Value>>,
    changed: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
}
pub struct Snapshot {
    pub printer: Printer,
    /// The raw `state.status`, when it is a known value.
    pub status: Option<String>,
    /// Full path of the live job file, such as `0:/gcodes/part.gcode`.
    pub file: Option<String>,
    pub last_file: Option<String>,
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
        .map_err(|_| "Set a Duet http or https URL.")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.host().is_none()
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.path_and_query().is_some_and(|p| p.query().is_some())
        || uri.path().contains(['%', '\\'])
        || uri.path().split('/').any(|p| matches!(p, "." | ".."))
    {
        return Err(
            "Set a Duet http or https URL without credentials, query, or escaped path segments."
                .into(),
        );
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
    if config
        .password
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.len() > 256 || p.chars().any(char::is_control))
        || config.api_key.is_some()
        || config.username.is_some()
        || config.ca_file.is_some()
        || config.cert_sha256.is_some()
    {
        return Err("Set only an optional plain password of at most 256 bytes for Duet. HTTPS requires a normally trusted certificate.".into());
    }
    if config
        .nozzle_diameter
        .is_some_and(|d| !d.is_finite() || d <= 0.0 || d > 10.0)
    {
        return Err("Set nozzle_diameter in millimetres, greater than 0 and at most 10.".into());
    }
    Ok(())
}
/// Percent-encodes a query value, keeping the drive and folder separators of a Duet path.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':' | b'/') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
pub fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
}
/// The staging folder path for a companion-built file name.
pub fn staged_path(name: &str) -> String {
    format!("0:/gcodes/{name}")
}
static CRC: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(|| {
    std::array::from_fn(|n| {
        (0..8).fold(n as u32, |c, _| {
            if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            }
        })
    })
});
/// The standard CRC-32 (IEEE 802.3) of the upload body.
pub fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, b| {
        CRC.get(usize::from((crc as u8) ^ b))
            .copied()
            .unwrap_or_default()
            ^ (crc >> 8)
    })
}
fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
impl Client {
    /// Release only the existing connection; never log in or retry during shutdown.
    pub async fn shutdown(&self) -> Result<(), Error> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut session = self.session.lock().await;
        if !session.connected {
            return Ok(());
        }
        session.connected = false;
        let key = session.key.take();
        drop(session);
        let path = if self.transport == Transport::Sbc {
            "/machine/disconnect"
        } else {
            "/rr_disconnect"
        };
        self.http
            .raw(Method::GET, path, Bytes::new(), key.as_deref(), false)
            .await
            .map(|_| ())
    }

    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        validate(&config)?;
        let uri: Uri = config
            .url
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| "Check Duet URL.")?;
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
            transport: if config.kind.as_deref() == Some("duet-sbc") {
                Transport::Sbc
            } else {
                Transport::Standalone
            },
            config,
            http: Arc::new(Http {
                uri,
                tls: Arc::new(tls),
            }),
            session: tokio::sync::Mutex::new(Session {
                key: None,
                connected: false,
                last: Instant::now(),
                timeout: DEFAULT_SESSION,
                generation: 0,
            }),
            observation: tokio::sync::Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
            changed: std::sync::atomic::AtomicBool::new(false),
        })
    }
}
impl Http {
    async fn raw(
        &self,
        method: Method,
        target: &str,
        body: Bytes,
        key: Option<&str>,
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
                    "Accept",
                    if target.starts_with("/machine/code") {
                        "text/plain"
                    } else {
                        "application/json"
                    },
                )
                .header("Content-Length", body.len());
            if target.starts_with("/machine/code") {
                request = request.header("Content-Type", "text/plain");
            }
            if upload {
                request = request.header("Content-Type", "application/octet-stream");
            }
            if let Some(key) = key {
                request = request.header("X-Session-Key", key);
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
            let bytes = Limited::new(response.into_body(), 1 << 20)
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
}
impl Client {
    /// Returns the current session key, logging in first when the session is absent or idle
    /// beyond the printer's reported timeout.
    async fn login(&self) -> Result<(Option<String>, u64), Error> {
        let mut session = self.session.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Unavailable);
        }
        if session.connected && session.last.elapsed() < session.timeout {
            return Ok((session.key.clone(), session.generation));
        }
        let mut target = String::from(if self.transport == Transport::Sbc {
            "/machine/connect?"
        } else {
            "/rr_connect?"
        });
        if let Some(password) = &self.config.password {
            target.push_str("password=");
            target.push_str(&encode(password));
            target.push('&');
        }
        if self.transport == Transport::Standalone {
            target.push_str("sessionKey=yes");
        }
        let (code, bytes) = self
            .http
            .raw(Method::GET, &target, Bytes::new(), None, false)
            .await?;
        if self.transport == Transport::Sbc && code == StatusCode::FORBIDDEN {
            return Err(Error::Authentication);
        }
        if code != StatusCode::OK {
            return Err(Error::Unavailable);
        }
        let reply: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Unavailable)?;
        if self.transport == Transport::Standalone {
            match reply.get("err").and_then(Value::as_u64) {
                Some(0) => {}
                Some(1) => return Err(Error::Authentication),
                Some(2) => return Err(refuse(SESSIONS_FULL)),
                _ => return Err(Error::Unavailable),
            }
        }
        let key = match reply.get("sessionKey") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::String(s))
                if !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_graphic()) =>
            {
                Some(s.clone())
            }
            Some(_) => return Err(Error::Unavailable),
        };
        if self.transport == Transport::Sbc && key.is_none() {
            return Err(Error::Unavailable);
        }
        session.timeout = if self.transport == Transport::Sbc {
            DEFAULT_SESSION
        } else {
            reply
                .get("sessionTimeout")
                .and_then(Value::as_u64)
                .filter(|t| *t > 0)
                .map_or(DEFAULT_SESSION, |t| Duration::from_millis(t.min(3_600_000)))
        };
        session.key = key;
        session.connected = true;
        session.last = Instant::now();
        session.generation = session.generation.wrapping_add(1);
        Ok((session.key.clone(), session.generation))
    }
    async fn expire(&self, generation: u64) {
        let mut session = self.session.lock().await;
        if session.generation == generation {
            session.connected = false;
        }
    }
    async fn touch(&self, generation: u64) {
        let mut session = self.session.lock().await;
        if session.generation == generation {
            session.last = Instant::now();
        }
    }
    /// Sends a request within a session. Reads and uploads refused with 401 (standalone)
    /// or 403 (DSF) are sent once more after logging in again; G-code is never resent.
    async fn call(
        &self,
        method: Method,
        target: &str,
        body: Bytes,
        upload: bool,
        resend: bool,
    ) -> Result<Bytes, Error> {
        for attempt in 0..2 {
            let (key, generation) = self.login().await?;
            let (code, bytes) = self
                .http
                .raw(method.clone(), target, body.clone(), key.as_deref(), upload)
                .await?;
            if code
                == if self.transport == Transport::Sbc {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::UNAUTHORIZED
                }
            {
                self.expire(generation).await;
                if !resend {
                    return Err(refuse(EXPIRED));
                }
                if attempt == 0 {
                    continue;
                }
                return Err(Error::Authentication);
            }
            let expected = if self.transport == Transport::Sbc && method == Method::PUT {
                StatusCode::CREATED
            } else if self.transport == Transport::Sbc && method == Method::DELETE {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::OK
            };
            if code != expected
                && !(self.transport == Transport::Sbc
                    && method == Method::DELETE
                    && code == StatusCode::NOT_FOUND)
            {
                return Err(Error::Unavailable);
            }
            self.touch(generation).await;
            return Ok(bytes);
        }
        Err(Error::Unavailable)
    }
    async fn json(&self, target: &str) -> Result<Value, Error> {
        let bytes = self
            .call(Method::GET, target, Bytes::new(), false, true)
            .await?;
        serde_json::from_slice(&bytes).map_err(|_| Error::Unavailable)
    }
    /// Reads a shared object model subtree through the selected transport.
    pub async fn model(&self, key: &str) -> Result<Value, Error> {
        if self.transport == Transport::Sbc {
            let model = self.json("/machine/model").await?;
            return model
                .pointer(&format!("/{}", key.replace('.', "/")))
                .cloned()
                .ok_or(Error::Unavailable);
        }
        let reply = self
            .json(&format!("/rr_model?key={}&flags=d99v", encode(key)))
            .await?;
        if reply.get("key").and_then(Value::as_str) != Some(key) {
            return Err(Error::Unavailable);
        }
        reply.get("result").cloned().ok_or(Error::Unavailable)
    }
    async fn models<const N: usize>(&self, keys: [&str; N]) -> Result<[Value; N], Error> {
        let mut values = std::array::from_fn(|_| Value::Null);
        if self.transport == Transport::Sbc {
            let model = self.json("/machine/model").await?;
            for (slot, key) in values.iter_mut().zip(keys) {
                *slot = model
                    .pointer(&format!("/{}", key.replace('.', "/")))
                    .cloned()
                    .ok_or(Error::Unavailable)?;
            }
        } else {
            for (slot, key) in values.iter_mut().zip(keys) {
                *slot = self.model(key).await?;
            }
        }
        Ok(values)
    }
    /// Sends G-code once. Acceptance only means it was queued; read status for the outcome.
    pub async fn gcode(&self, code: &str) -> Result<(), Error> {
        if self.transport == Transport::Sbc {
            let reply = self
                .call(
                    Method::POST,
                    "/machine/code?async=true",
                    Bytes::copy_from_slice(code.as_bytes()),
                    false,
                    false,
                )
                .await?;
            // The documented reply is plain text. Never expose backend output.
            let reply = std::str::from_utf8(&reply).map_err(|_| Error::Unavailable)?;
            if !reply.trim().is_empty() {
                return Err(refuse("Duet returned an unconfirmed command reply. Read fresh status to reconcile the job; the command will not be repeated."));
            }
            return Ok(());
        }
        let bytes = self
            .call(
                Method::GET,
                &format!("/rr_gcode?gcode={}", encode(code)),
                Bytes::new(),
                false,
                false,
            )
            .await?;
        let reply: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Unavailable)?;
        if reply.get("buff").and_then(Value::as_u64).is_none() {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    /// The stored size of a file in `0:/gcodes`, or None when the listing omits it.
    pub async fn stored(&self, name: &str) -> Result<Option<u64>, Error> {
        if !safe_filename(name) {
            return Err(Error::Unavailable);
        }
        if self.transport == Transport::Sbc {
            let listing = self.json("/machine/directory/0:/gcodes").await?;
            let files = listing.as_array().ok_or(Error::Unavailable)?;
            let Some(file) = files.iter().find(|f| {
                f.get("name").and_then(Value::as_str) == Some(name)
                    && f.get("type").and_then(Value::as_str) == Some("f")
            }) else {
                return Ok(None);
            };
            let size = file
                .get("size")
                .and_then(Value::as_u64)
                .ok_or(Error::Unavailable)?;
            let info = self
                .json(&format!("/machine/fileinfo/{}", encode(&staged_path(name))))
                .await?;
            if info.get("size").and_then(Value::as_u64) != Some(size) {
                return Err(Error::Unavailable);
            }
            return Ok(Some(size));
        }
        let mut first = 0u64;
        for _ in 0..64 {
            let reply = self
                .json(&format!("/rr_filelist?dir=0:/gcodes&first={first}"))
                .await?;
            if reply.get("err").and_then(Value::as_u64) != Some(0) {
                return Err(Error::Unavailable);
            }
            let files = reply
                .get("files")
                .and_then(Value::as_array)
                .ok_or(Error::Unavailable)?;
            if let Some(file) = files.iter().find(|f| {
                f.get("name").and_then(Value::as_str) == Some(name)
                    && f.get("type").and_then(Value::as_str) == Some("f")
            }) {
                return file
                    .get("size")
                    .and_then(Value::as_u64)
                    .map(Some)
                    .ok_or(Error::Unavailable);
            }
            match reply.get("next").and_then(Value::as_u64) {
                Some(0) => return Ok(None),
                Some(next) if next > first => first = next,
                _ => return Err(Error::Unavailable),
            }
        }
        Err(Error::Unavailable)
    }
    pub async fn upload(&self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        if !safe_filename(name) || bytes.len() > 200 << 20 {
            return Err(Error::Unavailable);
        }
        if self.transport == Transport::Sbc {
            self.call(
                Method::PUT,
                &format!("/machine/file/{}", encode(&staged_path(name))),
                Bytes::copy_from_slice(bytes),
                true,
                true,
            )
            .await?;
            return Ok(());
        }
        let reply = self
            .call(
                Method::POST,
                &format!(
                    "/rr_upload?name={}&crc32={:08x}",
                    encode(&staged_path(name)),
                    crc32(bytes)
                ),
                Bytes::copy_from_slice(bytes),
                true,
                true,
            )
            .await?;
        let reply: Value = serde_json::from_slice(&reply).map_err(|_| Error::Unavailable)?;
        if reply.get("err").and_then(Value::as_u64) != Some(0) {
            return Err(refuse(
                "Duet reported a failed upload. Inspect storage before preparing again.",
            ));
        }
        Ok(())
    }
    /// Deletes a staged file and confirms through the folder listing that it is gone.
    pub async fn delete(&self, name: &str) -> Result<(), Error> {
        if !safe_filename(name) {
            return Err(Error::Unavailable);
        }
        if self.transport == Transport::Sbc {
            self.call(
                Method::DELETE,
                &format!("/machine/file/{}", encode(&staged_path(name))),
                Bytes::new(),
                false,
                true,
            )
            .await?;
        } else {
            // A missing file also reports an error, so the listing decides.
            let _ = self
                .json(&format!("/rr_delete?name={}", encode(&staged_path(name))))
                .await?;
        }
        match self.stored(name).await? {
            None => Ok(()),
            Some(_) => Err(Error::Unavailable),
        }
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
        let [state, job, heat, tools] = self.models(["state", "job", "heat", "tools"]).await?;
        let raw = state.get("status").and_then(Value::as_str);
        let mut printer = self.offline(&Error::Unavailable);
        let (mapped, status) = match raw {
            Some("idle") => (PrinterState::Idle, raw),
            Some("processing" | "pausing" | "resuming" | "cancelling" | "changingTool") => {
                (PrinterState::Printing, raw)
            }
            Some("paused") => (PrinterState::Paused, raw),
            Some("busy" | "starting" | "updating" | "simulating") => (PrinterState::Busy, raw),
            Some("halted") => (PrinterState::Error, raw),
            Some("off" | "disconnected") => (PrinterState::Offline, raw),
            _ => (PrinterState::Error, None),
        };
        printer.state = mapped;
        printer.detail = Some(
            status
                .unwrap_or("Unknown Duet state. Read fresh status.")
                .into(),
        );
        let file = text(job.pointer("/file/fileName"));
        let last_file = text(job.get("lastFileName"));
        printer.job_id = file.clone();
        printer.job_name = file
            .as_deref()
            .and_then(|f| f.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let position = job.get("filePosition").and_then(Value::as_f64);
        let size = job.pointer("/file/size").and_then(Value::as_f64);
        // A byte fraction of the file, not elapsed print time.
        printer.progress = position
            .zip(size)
            .filter(|(p, s)| p.is_finite() && *s > 0.0 && *p >= 0.0)
            .map(|(p, s)| (p / s).min(1.0))
            .filter(|_| file.is_some());
        printer.seconds_left = job
            .pointer("/timesLeft/file")
            .and_then(Value::as_f64)
            .filter(|s| s.is_finite() && *s >= 0.0 && *s < 1e12)
            .map(|s| s.round() as u64)
            .filter(|_| file.is_some());
        let heaters = heat.get("heaters").and_then(Value::as_array);
        let reading = |index: Option<i64>| {
            let index = usize::try_from(index?).ok()?;
            heaters?
                .get(index)?
                .get("current")
                .and_then(Value::as_f64)
                .filter(|t| t.is_finite())
        };
        let tool = state.get("currentTool").and_then(Value::as_i64);
        printer.nozzle_temperature = tool
            .filter(|t| *t >= 0)
            .and_then(|t| {
                tools
                    .as_array()?
                    .iter()
                    .find(|v| v.get("number").and_then(Value::as_i64) == Some(t))
            })
            .and_then(|t| t.get("heaters")?.as_array()?.first()?.as_i64())
            .and_then(|i| reading(Some(i)));
        printer.bed_temperature = heat
            .get("bedHeaters")
            .and_then(Value::as_array)
            .and_then(|beds| beds.iter().filter_map(Value::as_i64).find(|i| *i >= 0))
            .and_then(|i| reading(Some(i)));
        let observed = json!({"status":raw,"file":file,"last":last_file});
        let mut previous = self.observation.lock().await;
        if previous.as_ref().is_some_and(|old| old != &observed) {
            self.changed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        *previous = Some(observed);
        printer.needs_clearing = Some(self.changed.load(std::sync::atomic::Ordering::SeqCst));
        Ok(Snapshot {
            printer,
            status: status.map(str::to_owned),
            file,
            last_file,
        })
    }
    /// Machine identity, tooling, and build volume reviewed before upload and start.
    pub async fn review(&self) -> Result<Value, Error> {
        let [boards, network, axes, tools] = self
            .models(["boards", "network", "move.axes", "tools"])
            .await?;
        let board = boards.as_array().and_then(|b| b.first());
        let tools = tools.as_array().ok_or(Error::Unavailable)?;
        let [tool] = tools.as_slice() else {
            return Err(refuse(
                "Material slot assignments are unresolved. Use a reviewed single-tool Duet printer.",
            ));
        };
        let extruders = tool.get("extruders").and_then(Value::as_array);
        let heaters = tool.get("heaters").and_then(Value::as_array);
        if extruders.is_none_or(|e| e.len() != 1) || heaters.is_none_or(|h| h.len() != 1) {
            return Err(refuse(
                "Material slot assignments are unresolved. Use a reviewed single-tool Duet printer.",
            ));
        }
        let nozzle = self.config.nozzle_diameter.ok_or_else(|| {
            refuse("Duet does not report its nozzle diameter. Check the installed nozzle and set nozzle_diameter for this printer before preparing.")
        })?;
        let volume = axes.as_array().map(|axes| {
            ["X", "Y", "Z"]
                .iter()
                .map(|letter| {
                    axes.iter()
                        .find(|a| a.get("letter").and_then(Value::as_str) == Some(letter))
                        .and_then(|a| {
                            let min = a.get("min").and_then(Value::as_f64)?;
                            let max = a.get("max").and_then(Value::as_f64)?;
                            (max > min).then(|| json!([letter, min, max]))
                        })
                })
                .collect::<Option<Vec<_>>>()
        });
        let volume = volume.flatten();
        let firmware = board.and_then(|b| text(b.get("firmwareVersion")));
        let name = board.and_then(|b| text(b.get("name")));
        if volume.is_none() || firmware.is_none() || name.is_none() {
            return Err(refuse("Machine identity, firmware, build volume, or tool information is missing. Review the printer configuration."));
        }
        let identity = serde_json::to_vec(&json!({
            "board":name,"firmware":firmware,"build":board.and_then(|b| b.get("firmwareName")),
            "unique":board.and_then(|b| b.get("uniqueId")),"machine":network.get("name"),
        }))
        .map_err(|_| Error::Unavailable)?;
        let tooling = serde_json::to_vec(&json!({
            "number":tool.get("number"),"extruders":extruders,"heaters":heaters,
        }))
        .map_err(|_| Error::Unavailable)?;
        Ok(json!({
            "endpoint":crate::safety::hash(self.config.url.as_deref().unwrap_or_default().as_bytes()),
            "identity":crate::safety::hash(&identity),
            "tooling":crate::safety::hash(&tooling),
            "info":{"nozzle_diameter":nozzle},
            "volume":volume,
        }))
    }
    pub fn offline(&self, error: &Error) -> Printer {
        Printer {
            controls: crate::backend::ALL_CONTROLS
                .iter()
                .copied()
                .map(String::from)
                .collect(),
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state: PrinterState::Offline,
            detail: Some(
                match error {
                    Error::Authentication => "Authentication failed. Check the Duet password.",
                    Error::Refused(message) if message == SESSIONS_FULL => SESSIONS_FULL,
                    _ => "Fresh Duet telemetry unavailable.",
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
    if live.printer.state != PrinterState::Idle || live.status.as_deref() != Some("idle") {
        return Err(refuse("Duet readiness is unknown or the printer is busy. Wait for fresh idle status before starting."));
    }
    Ok(())
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.transport != Transport::Sbc {
            return;
        }
        let session = self.session.get_mut();
        if !session.connected {
            return;
        }
        let key = session.key.take();
        let http = self.http.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = http
                    .raw(
                        Method::GET,
                        "/machine/disconnect",
                        Bytes::new(),
                        key.as_deref(),
                        false,
                    )
                    .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn query_values_keep_paths_and_escape_the_rest() {
        assert_eq!(
            encode("M32 \"0:/gcodes/a.gcode\""),
            "M32%20%220:/gcodes/a.gcode%22"
        );
        assert_eq!(encode("p&w=d#"), "p%26w%3Dd%23");
    }
}
