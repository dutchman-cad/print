//! Snapmaker 2.0 over its legacy HTTP API, from the description Snapmaker published on its
//! forum: `POST /api/v1/connect` (confirmed on the touchscreen), `GET /api/v1/status`, and
//! `POST /api/v1/disconnect`, the last two with the `token` parameter. Nothing else is sent.

use super::{offline, plain, report};
use crate::backend::{Error, Printer, PrinterState};
use axum::body::Bytes;
use axum::http::{Method, Request, StatusCode};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::time::Instant;

pub const PORT: u16 = 8080;
const DEADLINE: Duration = Duration::from_secs(4);
const MAX_REPLY: usize = 1 << 16;

const WAITING: &str = "Connecting to the Snapmaker printer.";
pub const APPROVE: &str =
    "Waiting for approval. Tap Confirm on the Snapmaker touchscreen to allow this connection.";
pub const TIMED_OUT: &str = "Snapmaker did not confirm the connection in time, so Print kept no connection. Approve the request on the touchscreen when it appears, then retry by reading status again.";
const UNAVAILABLE: &str = "Snapmaker connection unavailable. Check the printer's host, port, and network, then read status again.";
const REFUSED: &str = "The Snapmaker printer refused the connection request. Check the touchscreen, then read status again.";
const UNREADABLE: &str =
    "The Snapmaker connect reply carried no usable token. Read status again to retry.";
pub const ENDED: &str = "The Snapmaker printer no longer accepts this connection. Read status again to request a new one, then approve it on the touchscreen.";
const STALE: &str = "Snapmaker status is stale. Wait for fresh status.";
const UNRECOGNISED: &str = "Snapmaker has published only IDLE, so Print does not recognise this status. Check the printer.";
pub const NOT_A_PRINTER: &str =
    "The installed head is not a printing head, so this machine is not a printer right now.";
const WITHHELD: &str = "(withheld)";

/// Pairing, polling, and freshness timing.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How long a connection request may wait for touchscreen approval.
    pub pair: Duration,
    /// How long a connection request runs before status reads as awaiting approval.
    pub settle: Duration,
    /// Interval between status requests once approved.
    pub poll: Duration,
    /// A status reply older than this is stale.
    pub fresh: Duration,
    /// Deadline for each status request.
    pub request: Duration,
    /// After a connection ends, the next status read requests a new one no sooner than this.
    pub retry: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            pair: Duration::from_secs(60),
            settle: Duration::from_secs(2),
            poll: Duration::from_secs(10),
            fresh: Duration::from_secs(30),
            request: Duration::from_secs(4),
            retry: Duration::from_secs(30),
        }
    }
}

/// What the connect reply says about the machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Machine {
    pub series: Option<String>,
    /// `headType`, shown as reported; its numbering is not published.
    pub head: Option<String>,
    /// A textual `headType` that names a laser or CNC head.
    pub not_printing: bool,
    pub enclosure: Option<bool>,
}
impl Machine {
    /// Reads the documented connect fields, withholding any value that repeats the token.
    pub fn from_connect(reply: &Value, token: &str) -> Self {
        let shown = |text: String| {
            if text.contains(token) {
                WITHHELD.to_owned()
            } else {
                text
            }
        };
        let series = reply
            .get("series")
            .and_then(Value::as_str)
            .map(|s| shown(plain(s, 64)))
            .filter(|s| !s.trim().is_empty());
        let (head, not_printing) = match reply.get("headType") {
            Some(Value::Number(n)) => (Some(shown(plain(&n.to_string(), 16))), false),
            Some(Value::String(s)) if !s.trim().is_empty() => {
                let lower = s.to_ascii_lowercase();
                (
                    Some(shown(plain(s, 32))),
                    lower.contains("laser") || lower.contains("cnc"),
                )
            }
            _ => (None, false),
        };
        Self {
            series,
            head,
            not_printing,
            enclosure: reply.get("hasEnclosure").and_then(Value::as_bool),
        }
    }
}

/// Maps one status reply to printer JSON. Only the published `IDLE` is idle; any other value
/// is `error` with the value shown, and no running, paused, or progress state is invented.
pub fn status(config: &crate::config::Printer, reply: &Value, machine: &Machine) -> Printer {
    let (mut state, mut detail) = match reply.get("status") {
        Some(Value::String(s)) if s == "IDLE" => (PrinterState::Idle, "IDLE".to_owned()),
        Some(Value::String(s)) if !s.trim().is_empty() => (
            PrinterState::Error,
            format!("{}; {UNRECOGNISED}", plain(s, 32)),
        ),
        Some(Value::Number(n)) => (
            PrinterState::Error,
            format!("{}; {UNRECOGNISED}", plain(&n.to_string(), 16)),
        ),
        _ => (
            PrinterState::Error,
            format!("UNKNOWN STATUS; {UNRECOGNISED}"),
        ),
    };
    if let Some(series) = &machine.series {
        detail.push_str(&format!("; {series}"));
    }
    if let Some(head) = &machine.head {
        detail.push_str(&format!("; head type {head}"));
    }
    if machine.enclosure == Some(true) {
        detail.push_str("; enclosure");
    }
    if machine.not_printing {
        if state == PrinterState::Idle {
            state = PrinterState::Attention;
        }
        detail.push_str(&format!("; {NOT_A_PRINTER}"));
    }
    report(config, state, detail)
}

/// A token is kept only if it is plain enough to send as a query parameter unchanged.
fn usable_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Clone)]
struct Telemetry {
    running: bool,
    ended: Option<Instant>,
    status: Option<Value>,
    received: Option<Instant>,
    machine: Machine,
    reason: &'static str,
}
struct Link {
    token: std::sync::Arc<Mutex<Option<String>>>,
    host: String,
    port: u16,
    timing: Timing,
    state: watch::Sender<Telemetry>,
}

pub struct Client {
    config: crate::config::Printer,
    token: std::sync::Arc<Mutex<Option<String>>>,
    closed: std::sync::atomic::AtomicBool,
    timing: Timing,
    state: watch::Sender<Telemetry>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = self.worker.get_mut().take() {
            task.abort();
        }
    }
}
impl Client {
    pub async fn shutdown(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut worker = self.worker.lock().await;
        if let Some(task) = worker.take() {
            task.abort();
            let _ = task.await;
        }
        self.state.send_modify(|t| {
            t.running = false;
            t.received = None;
            t.reason = ENDED;
        });
        let token = self.token.lock().await.take();
        if let Some(token) = token {
            let link = Link {
                token: self.token.clone(),
                host: self.config.host.clone().unwrap_or_default(),
                port: self.config.port.unwrap_or(PORT),
                timing: self.timing,
                state: self.state.clone(),
            };
            link.disconnect(&token).await;
        }
    }

    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        super::validate(&config)?;
        let (state, _) = watch::channel(Telemetry {
            running: false,
            ended: None,
            status: None,
            received: None,
            machine: Machine::default(),
            reason: WAITING,
        });
        Ok(Self {
            config,
            token: Default::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
            timing: Timing::default(),
            state,
            worker: Mutex::new(None),
        })
    }
    /// Replaces the timing before the first status read, for simulated printers.
    pub fn with_timing(mut self, timing: Timing) -> Self {
        let bound = |d: Duration| d.clamp(Duration::from_millis(10), Duration::from_secs(3600));
        self.timing = Timing {
            pair: bound(timing.pair),
            settle: bound(timing.settle),
            poll: bound(timing.poll),
            fresh: bound(timing.fresh),
            request: bound(timing.request),
            retry: bound(timing.retry),
        };
        self
    }
    /// Starts a connection unless one is open or pending, or the last ended too recently.
    async fn begin(&self) {
        let mut worker = self.worker.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let due = {
            let t = self.state.borrow();
            !t.running && t.ended.is_none_or(|e| e.elapsed() >= self.timing.retry)
        };
        if due {
            self.state.send_modify(|t| {
                t.running = true;
                t.reason = WAITING;
            });
            let link = Link {
                token: self.token.clone(),
                host: self.config.host.clone().unwrap_or_default(),
                port: self.config.port.unwrap_or(PORT),
                timing: self.timing,
                state: self.state.clone(),
            };
            if let Some(old) = worker.replace(tokio::spawn(run(link))) {
                old.abort();
            }
        }
    }
    fn live(&self, t: &Telemetry) -> bool {
        t.running
            && t.status.is_some()
            && t.received.is_some_and(|r| r.elapsed() < self.timing.fresh)
    }
    fn printer(&self, t: &Telemetry) -> Printer {
        match t.status.as_ref().filter(|_| self.live(t)) {
            Some(reply) => status(&self.config, reply, &t.machine),
            None if t.running && t.received.is_some() => offline(&self.config, STALE),
            None => offline(&self.config, t.reason),
        }
    }
    /// Fresh status, waiting briefly while a new connection request is answered.
    pub async fn snapshot(&self) -> Printer {
        self.begin().await;
        let mut rx = self.state.subscribe();
        let _ = tokio::time::timeout(DEADLINE, async {
            loop {
                {
                    let t = rx.borrow_and_update();
                    if self.live(&t) || t.reason != WAITING {
                        break;
                    }
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        let t = rx.borrow().clone();
        self.printer(&t)
    }
}

impl Link {
    /// One HTTP/1.1 request. Bodies are never shown to callers and the path, which may hold
    /// the token, is never logged or returned.
    async fn request(
        &self,
        method: Method,
        path: &str,
        form: Option<String>,
        deadline: Instant,
        reached: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<(StatusCode, Bytes), Error> {
        let operation = async {
            let socket = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|_| Error::Unavailable)?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(socket))
                    .await
                    .map_err(|_| Error::Unavailable)?;
            if let Some(reached) = reached {
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            struct Driver(tokio::task::JoinHandle<()>);
            impl Drop for Driver {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _driver = Driver(tokio::spawn(async move {
                let _ = connection.await;
            }));
            let host = if self.host.contains(':') {
                format!("[{}]:{}", self.host, self.port)
            } else {
                format!("{}:{}", self.host, self.port)
            };
            let body = Bytes::from(form.unwrap_or_default());
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("Host", host)
                .header("Accept", "application/json")
                .header("Content-Length", body.len());
            if !body.is_empty() {
                request = request.header("Content-Type", "application/x-www-form-urlencoded");
            }
            let request = request
                .body(Full::new(body))
                .map_err(|_| Error::Unavailable)?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|_| Error::Unavailable)?;
            let code = response.status();
            let bytes = Limited::new(response.into_body(), MAX_REPLY)
                .collect()
                .await
                .map_err(|_| Error::Unavailable)?
                .to_bytes();
            Ok((code, bytes))
        };
        tokio::time::timeout_at(deadline, operation)
            .await
            .map_err(|_| Error::Unavailable)?
    }
    /// Releases an approved or pending token; the reply does not matter.
    async fn disconnect(&self, token: &str) {
        let _ = self
            .request(
                Method::POST,
                "/api/v1/disconnect",
                Some(format!("token={token}")),
                Instant::now() + self.timing.request,
                None,
            )
            .await;
    }
}

async fn run(link: Link) {
    let outcome = connection(&link).await;
    link.state.send_modify(|t| {
        t.running = false;
        t.ended = Some(Instant::now());
        t.status = None;
        t.received = None;
        t.machine = Machine::default();
        t.reason = outcome;
    });
}

/// Requests a connection, waits a bounded time for approval, then polls status until the
/// printer stops accepting the token. Returns why the connection ended.
async fn connection(link: &Link) -> &'static str {
    let deadline = Instant::now() + link.timing.pair;
    let reached = std::sync::atomic::AtomicBool::new(false);
    let connect = link.request(
        Method::POST,
        "/api/v1/connect",
        None,
        deadline,
        Some(&reached),
    );
    tokio::pin!(connect);
    let settle = tokio::time::sleep(link.timing.settle);
    tokio::pin!(settle);
    let mut settled = false;
    let reply = loop {
        tokio::select! {
            reply = &mut connect => break reply,
            () = &mut settle, if !settled => {
                // Ask for the touchscreen only once the request has reached the printer.
                if reached.load(std::sync::atomic::Ordering::SeqCst) {
                    settled = true;
                    link.state.send_modify(|t| t.reason = APPROVE);
                } else {
                    settle.as_mut().reset(tokio::time::Instant::now() + link.timing.settle);
                }
            }
        }
    };
    let (code, body) = match reply {
        Ok(reply) => reply,
        // Only a request the printer received can be waiting on the touchscreen.
        Err(_)
            if reached.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() >= deadline =>
        {
            return TIMED_OUT
        }
        Err(_) => return UNAVAILABLE,
    };
    if code != StatusCode::OK {
        return REFUSED;
    }
    let Ok(reply) = serde_json::from_slice::<Value>(&body) else {
        return UNREADABLE;
    };
    let Some(token) = reply
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| usable_token(t))
        .map(str::to_owned)
    else {
        return UNREADABLE;
    };
    *link.token.lock().await = Some(token.clone());
    let machine = Machine::from_connect(&reply, &token);
    link.state.send_modify(|t| t.machine = machine);
    let path = format!("/api/v1/status?token={token}");
    let mut approved = false;
    loop {
        let reply = link
            .request(
                Method::GET,
                &path,
                None,
                Instant::now() + link.timing.request,
                None,
            )
            .await;
        let body = match reply {
            Ok((StatusCode::OK, body)) => serde_json::from_slice::<Value>(&body)
                .ok()
                .filter(Value::is_object),
            Ok((code, _)) if code.is_client_error() && approved => return ENDED,
            _ => None,
        };
        if let Some(mut body) = body {
            if let Some(raw) = body
                .get_mut("status")
                .filter(|s| s.as_str().is_some_and(|s| s.contains(token.as_str())))
            {
                *raw = Value::from(WITHHELD);
            }
            approved = true;
            link.state.send_modify(|t| {
                t.status = Some(body);
                t.received = Some(Instant::now());
            });
        } else if !approved && Instant::now() >= deadline {
            link.token.lock().await.take();
            link.disconnect(&token).await;
            return TIMED_OUT;
        } else if !approved {
            // Status is refused until the touchscreen request is confirmed.
            link.state.send_modify(|t| t.reason = APPROVE);
        }
        let pause = if approved {
            link.timing.poll
        } else {
            link.timing.settle.min(link.timing.poll)
        };
        tokio::time::sleep(pause).await;
    }
}
