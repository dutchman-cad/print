//! PrusaLink transport, written from Prusa's public protocol documentation.

use crate::backend::{Error, JobState, Printer, PrinterState};
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use md5::{Digest as _, Md5};
use serde_json::Value;
use std::time::Duration;

const STATUS_TIMEOUT: Duration = Duration::from_secs(4);
pub struct Client {
    config: crate::config::Printer,
    uri: Uri,
    connection: tokio::sync::Mutex<Option<Connection>>,
    authentication: tokio::sync::Mutex<Option<(String, u32)>>,
    terminal: std::sync::Mutex<std::collections::VecDeque<(String, String, JobState)>>,
}
struct Connection {
    sender: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    driver: tokio::task::JoinHandle<()>,
    progress: tokio::sync::watch::Sender<crate::progress::Progress>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}
pub struct Snapshot {
    pub printer: Printer,
    pub status: Value,
    pub job: Option<Value>,
}
fn refused(message: &str) -> Error {
    Error::Refused(message.into())
}

pub fn validate(config: &crate::config::Printer) -> Result<(), String> {
    let uri: Uri = config
        .url
        .as_deref()
        .unwrap_or_default()
        .parse()
        .map_err(|_| "Set a printer URL as http://host[:port].")?;
    if uri.scheme_str() != Some("http")
        || uri.host().is_none()
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || !matches!(uri.path_and_query().map(|p| p.as_str()), None | Some("/"))
    {
        return Err(
            "Set a printer URL as http://host[:port], without credentials or a path.".into(),
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
    let credentials = match (&config.api_key, &config.username, &config.password) {
        (Some(key), None, None) => !key.is_empty(),
        (None, Some(user), Some(password)) => !user.is_empty() && !password.is_empty(),
        _ => false,
    };
    if !credentials {
        return Err("Set either api_key or username and password for each printer.".into());
    }
    for value in [&config.api_key, &config.username, &config.password]
        .into_iter()
        .flatten()
    {
        if value.chars().any(char::is_control) || value.len() > 1024 {
            return Err("Use credentials of at most 1024 bytes without control characters.".into());
        }
    }
    Ok(())
}
impl Client {
    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        validate(&config)?;
        let uri = config
            .url
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| "Check printer URL.")?;
        Ok(Self {
            config,
            uri,
            connection: tokio::sync::Mutex::new(None),
            authentication: tokio::sync::Mutex::new(None),
            terminal: std::sync::Mutex::new(std::collections::VecDeque::new()),
        })
    }
    async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        auth: Option<String>,
        upload: bool,
        deadline: Duration,
    ) -> Result<(StatusCode, HeaderMap, Bytes), Error> {
        let mut cached = self.connection.lock().await;
        let mut active = cached.take();
        if active.as_ref().is_some_and(|c| c.sender.is_closed()) {
            active = None;
        }
        let (progress, _) = crate::progress::channel();
        let progress = active
            .as_ref()
            .map(|c| c.progress.clone())
            .unwrap_or(progress);
        crate::progress::touch(&progress);
        let updates = progress.subscribe();
        let size = body.len() as u64;
        let operation = async {
            if active.as_ref().is_some_and(|c| c.sender.is_closed()) {
                active = None;
            }
            if active.is_none() {
                let host = self.uri.host().ok_or(Error::Unavailable)?;
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let port = self.uri.port_u16().unwrap_or(80);
                let socket = tokio::net::TcpStream::connect((host, port))
                    .await
                    .map_err(|_| Error::Unavailable)?;
                let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(
                    crate::progress::Stream::new(socket, progress.clone()),
                ))
                .await
                .map_err(|_| Error::Unavailable)?;
                let task = tokio::spawn(async move {
                    let _ = connection.await;
                });
                active = Some(Connection {
                    sender,
                    driver: task,
                    progress,
                });
            }
            let connection = active.as_mut().ok_or(Error::Unavailable)?;
            crate::progress::touch(&connection.progress);
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header(
                    "Host",
                    self.uri.authority().ok_or(Error::Unavailable)?.as_str(),
                )
                .header("Accept", "application/json")
                .header("Content-Length", body.len());
            if let Some(key) = &self.config.api_key {
                request = request.header("X-Api-Key", key);
            }
            if let Some(auth) = auth {
                request = request.header("Authorization", auth);
            }
            if upload {
                request = request
                    .header("Content-Type", "application/octet-stream")
                    .header("Print-After-Upload", "?0")
                    .header("Overwrite", "?0");
            }
            let request = request
                .body(Full::new(body))
                .map_err(|_| refused("Check printer credentials."))?;
            let response = connection
                .sender
                .send_request(request)
                .await
                .map_err(|_| Error::Unavailable)?;
            let (parts, body) = response.into_parts();
            let bytes = Limited::new(body, 1 << 20)
                .collect()
                .await
                .map_err(|_| Error::Unavailable)?
                .to_bytes();
            Ok((parts.status, parts.headers, bytes))
        };
        let result = if upload {
            // Reused sockets retain their own progress channel.
            crate::progress::timeout(deadline, crate::progress::REPLY, updates, size, operation)
                .await
                .map_err(|_| Error::Unavailable)
        } else {
            tokio::time::timeout(deadline, operation)
                .await
                .map_err(|_| Error::Unavailable)
        };
        match result {
            Ok(Ok(value)) => {
                *cached = active;
                Ok(value)
            }
            Ok(Err(error)) | Err(error) => Err(error),
        }
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        upload: bool,
    ) -> Result<(StatusCode, Bytes), Error> {
        let deadline = if upload {
            Duration::from_secs(30)
        } else if method == Method::GET {
            STATUS_TIMEOUT
        } else {
            Duration::from_secs(15)
        };
        let mut cache = self.authentication.lock().await;
        let credentials = self
            .config
            .username
            .as_ref()
            .zip(self.config.password.as_ref());
        if credentials.is_some() && cache.is_none() {
            let (code, headers, _) = self
                .raw(
                    Method::GET,
                    "/api/v1/status",
                    Bytes::new(),
                    None,
                    false,
                    STATUS_TIMEOUT,
                )
                .await?;
            if code != StatusCode::UNAUTHORIZED {
                return Err(Error::Authentication);
            }
            let challenge = headers
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .ok_or(Error::Authentication)?;
            *cache = Some((challenge.to_owned(), 0));
        }
        for attempt in 0..2 {
            let auth = if let (Some((user, password)), Some((challenge, count))) =
                (credentials, cache.as_mut())
            {
                *count = count.checked_add(1).ok_or(Error::Authentication)?;
                Some(digest(
                    challenge,
                    user,
                    password,
                    method.as_str(),
                    path,
                    *count,
                )?)
            } else {
                None
            };
            let (code, headers, bytes) = self
                .raw(method.clone(), path, body.clone(), auth, upload, deadline)
                .await?;
            if code == StatusCode::UNAUTHORIZED {
                let challenge = headers
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok());
                if attempt == 0
                    && method == Method::GET
                    && credentials.is_some()
                    && challenge.is_some_and(|c| {
                        c.split(',')
                            .any(|f| f.trim() == "stale=true" || f.trim() == "stale=\"true\"")
                    })
                {
                    *cache = challenge.map(|c| (c.to_owned(), 0));
                    continue;
                }
                *cache = None;
                return Err(Error::Authentication);
            }
            if code == StatusCode::FORBIDDEN {
                return Err(Error::Authentication);
            }
            return Ok((code, bytes));
        }
        Err(Error::Authentication)
    }
    pub async fn json(&self, path: &str) -> Result<Value, Error> {
        let (code, body) = self.request(Method::GET, path, Bytes::new(), false).await?;
        if code != StatusCode::OK {
            return Err(Error::Unavailable);
        }
        serde_json::from_slice(&body).map_err(|_| Error::Unavailable)
    }
    pub(crate) fn terminal(&self, id: &str, name: &str) -> Option<JobState> {
        self.terminal
            .lock()
            .ok()?
            .iter()
            .find(|(seen_id, seen_name, _)| seen_id == id && seen_name == name)
            .map(|(_, _, state)| *state)
    }
    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        tokio::time::timeout(STATUS_TIMEOUT, self.snapshot_in())
            .await
            .map_err(|_| Error::Unavailable)?
    }
    async fn snapshot_in(&self) -> Result<Snapshot, Error> {
        let status = self.json("/api/v1/status").await?;
        let detail = status
            .pointer("/printer/state")
            .and_then(Value::as_str)
            .ok_or(Error::Unavailable)?;
        let state = match detail {
            "IDLE" | "READY" | "FINISHED" | "STOPPED" => PrinterState::Idle,
            "PRINTING" => PrinterState::Printing,
            "BUSY" => PrinterState::Busy,
            "PAUSED" => PrinterState::Paused,
            "ATTENTION" => PrinterState::Attention,
            _ => PrinterState::Error,
        };
        let job = if status.get("job").is_some_and(|j| !j.is_null()) {
            let value = self.json("/api/v1/job").await?;
            if value.get("id") != status.pointer("/job/id") {
                return Err(refused(
                    "Printer job changed while reading status. Read status again.",
                ));
            }
            Some(value)
        } else {
            None
        };
        let printer = Printer {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state,
            detail: Some(detail.into()),
            needs_clearing: matches!(detail, "FINISHED" | "STOPPED" | "ERROR").then_some(true),
            job_id: job
                .as_ref()
                .and_then(|j| j.get("id"))
                .and_then(Value::as_u64)
                .map(|id| id.to_string()),
            job_name: job
                .as_ref()
                .and_then(|j| {
                    j.pointer("/file/display_name")
                        .or_else(|| j.pointer("/file/name"))
                })
                .and_then(Value::as_str)
                .map(str::to_owned),
            progress: status
                .pointer("/job/progress")
                .and_then(Value::as_f64)
                .filter(|v| (0.0..=100.0).contains(v))
                .map(|v| v / 100.0),
            seconds_left: status
                .pointer("/job/time_remaining")
                .and_then(Value::as_u64),
            nozzle_temperature: status
                .pointer("/printer/temp_nozzle")
                .and_then(Value::as_f64),
            bed_temperature: status.pointer("/printer/temp_bed").and_then(Value::as_f64),
        };
        if let (Some(id), Some(name), Some(outcome)) = (
            &printer.job_id,
            printer.job_name.as_ref().filter(|name| name.len() <= 256),
            match detail {
                "FINISHED" => Some(JobState::Done),
                "STOPPED" => Some(JobState::Cancelled),
                _ => None,
            },
        ) {
            if let Ok(mut terminal) = self.terminal.lock() {
                if !terminal
                    .iter()
                    .any(|(seen, seen_name, _)| seen == id && seen_name == name)
                {
                    if terminal.len() >= 64 {
                        terminal.pop_front();
                    }
                    terminal.push_back((id.clone(), name.clone(), outcome));
                }
            }
        }
        Ok(Snapshot {
            printer,
            status,
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
                match error {
                    Error::Authentication => "Authentication failed. Check printer credentials.",
                    Error::Refused(_) => "Telemetry could not be reconciled. Read fresh status.",
                    _ => "Fresh telemetry unavailable.",
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
fn md5(text: &str) -> String {
    Md5::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn quoted(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}
fn digest(
    challenge: &str,
    user: &str,
    password: &str,
    method: &str,
    path: &str,
    count: u32,
) -> Result<String, Error> {
    let text = challenge
        .strip_prefix("Digest ")
        .ok_or_else(|| refused("Printer needs Digest authentication."))?;
    let mut fields = std::collections::HashMap::new();
    let mut chars = text.chars().peekable();
    while chars.peek().is_some() {
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    break;
                }
                if c == '\\' {
                    if let Some(c) = chars.next() {
                        value.push(c);
                    }
                } else {
                    value.push(c);
                }
            }
            while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
                chars.next();
            }
        } else {
            value = chars
                .by_ref()
                .take_while(|c| *c != ',')
                .collect::<String>()
                .trim()
                .into();
        }
        fields.insert(key.trim().to_ascii_lowercase(), value);
    }
    let realm = fields
        .get("realm")
        .ok_or_else(|| refused("Digest realm is missing."))?;
    let nonce = fields
        .get("nonce")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| refused("Digest nonce is missing."))?;
    let algorithm = fields.get("algorithm").map(String::as_str).unwrap_or("MD5");
    if !matches!(algorithm, "MD5" | "MD5-sess") {
        return Err(refused("Printer Digest algorithm is not supported yet."));
    }
    let qop = fields.get("qop");
    if qop.is_some_and(|v| !v.split(',').any(|v| v.trim() == "auth")) {
        return Err(refused("Printer Digest protection is not supported yet."));
    }
    let cnonce = crate::new_job_id().map_err(Error::Failed)?;
    let mut ha1 = md5(&format!("{user}:{realm}:{password}"));
    if algorithm == "MD5-sess" {
        ha1 = md5(&format!("{ha1}:{nonce}:{cnonce}"));
    }
    let ha2 = md5(&format!("{method}:{path}"));
    let nc = format!("{count:08x}");
    let response = if qop.is_some() {
        md5(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
    } else {
        md5(&format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut auth = format!("Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{response}\", algorithm={algorithm}", quoted(user), quoted(realm), quoted(nonce), quoted(path));
    if qop.is_some() {
        auth.push_str(&format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\""));
    } else if algorithm == "MD5-sess" {
        auth.push_str(&format!(", cnonce=\"{cnonce}\""));
    }
    if let Some(opaque) = fields.get("opaque") {
        auth.push_str(&format!(", opaque=\"{}\"", quoted(opaque)));
    }
    Ok(auth)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn terminal_observations_survive_later_idle_reads() {
        for (wire, outcome) in [
            ("FINISHED", JobState::Done),
            ("STOPPED", JobState::Cancelled),
        ] {
            let state = std::sync::Arc::new(tokio::sync::Mutex::new(wire));
            let observed = state.clone();
            let app = axum::Router::new().fallback(move |uri: Uri| {
                let observed = observed.clone();
                async move {
                    let wire = *observed.lock().await;
                    axum::Json(if uri.path() == "/api/v1/status" {
                        if wire == "IDLE" {
                            serde_json::json!({"printer":{"state":"IDLE"}})
                        } else {
                            serde_json::json!({"printer":{"state":wire},"job":{"id":41}})
                        }
                    } else {
                        serde_json::json!({"id":41,"file":{"name":"test.gcode"}})
                    })
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = Client::new(crate::config::Printer {
                url: Some(format!("http://{}", listener.local_addr().unwrap())),
                api_key: Some("synthetic".into()),
                ..Default::default()
            })
            .unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            client.snapshot().await.unwrap();
            *state.lock().await = "IDLE";
            assert_eq!(
                client.snapshot().await.unwrap().printer.state,
                PrinterState::Idle
            );
            assert_eq!(client.terminal("41", "test.gcode"), Some(outcome));
            assert_eq!(client.terminal("41", "other.gcode"), None);
            server.abort();
        }
    }

    #[tokio::test]
    async fn upload_progress_outlasts_idle_limit_and_stalled_socket_is_discarded() {
        for stalled in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                if stalled {
                    std::future::pending::<()>().await;
                }
                let mut left = 16 << 20;
                let mut bytes = [0; 65536];
                while left > 0 {
                    let n = socket.read(&mut bytes).await.unwrap();
                    assert!(n > 0);
                    left -= n;
                    // Simulate a slow link, independent of command/message ordering.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                socket
                    .get_mut()
                    .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            });
            let client = Client::new(crate::config::Printer {
                url: Some(url),
                api_key: Some("synthetic".into()),
                ..Default::default()
            })
            .unwrap();
            let result = client
                .raw(
                    Method::PUT,
                    "/api/v1/files/usb/test.gcode",
                    Bytes::from(vec![1; 16 << 20]),
                    None,
                    true,
                    Duration::from_secs(2),
                )
                .await;
            if stalled {
                assert!(result.is_err());
                assert!(client.connection.lock().await.is_none());
                server.abort();
            } else {
                assert_eq!(result.unwrap().0, StatusCode::CREATED);
                server.await.unwrap();
            }
        }
    }
}
