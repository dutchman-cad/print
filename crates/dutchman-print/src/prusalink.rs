//! PrusaLink transport, written from Prusa's public protocol documentation.

use crate::backend::{Error, Printer, PrinterState};
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
        Ok(Self { config, uri })
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
        let operation = async {
            let host = self.uri.host().ok_or(Error::Unavailable)?;
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let port = self.uri.port_u16().unwrap_or(80);
            let socket = tokio::net::TcpStream::connect((host, port))
                .await
                .map_err(|_| Error::Unavailable)?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(socket))
                    .await
                    .map_err(|_| Error::Unavailable)?;
            let task = tokio::spawn(async move {
                let _ = connection.await;
            });
            // Aborting the driver also bounds connections on timeout or cancellation.
            struct Driver(tokio::task::JoinHandle<()>);
            impl Drop for Driver {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _driver = Driver(task);
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
            let response = sender
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
        tokio::time::timeout(deadline, operation)
            .await
            .map_err(|_| Error::Unavailable)?
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        upload: bool,
    ) -> Result<(StatusCode, Bytes), Error> {
        let deadline = if upload {
            Duration::from_secs(30 + body.len() as u64 / (32 * 1024))
        } else {
            STATUS_TIMEOUT
        };
        // Challenge only a read. A machine request is sent exactly once.
        let auth =
            if let (Some(user), Some(password)) = (&self.config.username, &self.config.password) {
                let (status, headers, _) = self
                    .raw(
                        Method::GET,
                        "/api/v1/status",
                        Bytes::new(),
                        None,
                        false,
                        STATUS_TIMEOUT,
                    )
                    .await?;
                if status != StatusCode::UNAUTHORIZED {
                    return Err(refused(
                        "Printer did not provide Digest authentication. Check its settings.",
                    ));
                }
                let challenge = headers
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| refused("Printer authentication challenge is missing."))?;
                Some(digest(challenge, user, password, method.as_str(), path)?)
            } else {
                None
            };
        let (status, _, bytes) = self.raw(method, path, body, auth, upload, deadline).await?;
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(Error::Authentication);
        }
        Ok((status, bytes))
    }
    pub async fn json(&self, path: &str) -> Result<Value, Error> {
        let (code, body) = self.request(Method::GET, path, Bytes::new(), false).await?;
        if code != StatusCode::OK {
            return Err(Error::Unavailable);
        }
        serde_json::from_slice(&body).map_err(|_| Error::Unavailable)
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
            "PRINTING" | "BUSY" => PrinterState::Printing,
            "PAUSED" | "ATTENTION" => PrinterState::Paused,
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
    let response = if qop.is_some() {
        md5(&format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"))
    } else {
        md5(&format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut auth = format!("Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{response}\", algorithm={algorithm}", quoted(user), quoted(realm), quoted(nonce), quoted(path));
    if qop.is_some() {
        auth.push_str(&format!(", qop=auth, nc=00000001, cnonce=\"{cnonce}\""));
    } else if algorithm == "MD5-sess" {
        auth.push_str(&format!(", cnonce=\"{cnonce}\""));
    }
    if let Some(opaque) = fields.get("opaque") {
        auth.push_str(&format!(", opaque=\"{}\"", quoted(opaque)));
    }
    Ok(auth)
}
