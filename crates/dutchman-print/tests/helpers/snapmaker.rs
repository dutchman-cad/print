//! Synthetic Snapmaker 2.0 (HTTP) and J1 (SACP over TCP) printers on test-owned loopback
//! sockets. Each records every request it receives.
#![allow(dead_code)]
use axum::{body::Bytes, http::Method, http::StatusCode, http::Uri, Router};
use dutchman_print::snapmaker::sacp::{self, Decoder, Packet};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{watch, Mutex};

/// The example token from Snapmaker's published connect reply.
pub const TOKEN: &str = "9b114b52-22f7-4384-b5d1-2de897474722";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// The connect reply arrives at once and the token is accepted.
    Immediate,
    /// The connect request is held open until `approve`.
    Hold,
    /// The connect reply carries a token at once, but status is refused until `approve`.
    Pending,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub body: String,
}

pub struct Machine2 {
    pub approval: Approval,
    pub connect: Value,
    pub status: Value,
    /// Status replies fail with 500 while set.
    pub failing: bool,
    /// The token is refused with 401 while set.
    pub revoked: bool,
    pub requests: Vec<Request>,
}
pub struct Fake2 {
    pub machine: Arc<Mutex<Machine2>>,
    pub port: u16,
    approved: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake2 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn connect_reply() -> Value {
    json!({"token": TOKEN, "series": "Snapmaker 2.0 A150", "headType": 3, "hasEnclosure": false})
}
pub fn idle() -> Value {
    json!({"status": "IDLE", "x": 0.775, "y": 0.752, "z": 0.443, "homed": true, "offsetX": 0, "offsetY": 0})
}

impl Fake2 {
    pub async fn new(approval: Approval) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let machine = Arc::new(Mutex::new(Machine2 {
            approval,
            connect: connect_reply(),
            status: idle(),
            failing: false,
            revoked: false,
            requests: Vec::new(),
        }));
        let (approved, _) = watch::channel(false);
        let app = Router::new().fallback({
            let machine = machine.clone();
            let approved = approved.clone();
            move |method: Method, uri: Uri, body: Bytes| {
                let machine = machine.clone();
                let approved = approved.subscribe();
                async move { handle(machine, approved, method, uri, body).await }
            }
        });
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            machine,
            port,
            approved,
            task,
        }
    }
    pub fn approve(&self) {
        self.approved.send_replace(true);
    }
    pub async fn requests(&self) -> Vec<Request> {
        self.machine.lock().await.requests.clone()
    }
    pub async fn count(&self, path: &str) -> usize {
        self.requests()
            .await
            .iter()
            .filter(|r| r.path == path)
            .count()
    }
    pub fn printer(&self) -> dutchman_print::config::Printer {
        dutchman_print::config::Printer {
            kind: Some("snapmaker-2".into()),
            host: Some("127.0.0.1".into()),
            port: Some(self.port),
            id: "test".into(),
            name: "Test Snapmaker 2.0".into(),
            model: "Synthetic Snapmaker 2.0".into(),
            ..Default::default()
        }
    }
    pub fn config(&self) -> dutchman_print::config::Config {
        config(self.printer())
    }
}

pub fn config(printer: dutchman_print::config::Printer) -> dutchman_print::config::Config {
    dutchman_print::config::Config {
        backend: dutchman_print::config::Backend {
            kind: "printers".into(),
            slice: false,
        },
        slicer: None,
        slicers: Default::default(),
        printers: vec![printer],
    }
}

async fn handle(
    machine: Arc<Mutex<Machine2>>,
    mut approved: watch::Receiver<bool>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> (StatusCode, String) {
    let approval = {
        let mut m = machine.lock().await;
        m.requests.push(Request {
            method: method.to_string(),
            path: uri.path().to_owned(),
            query: uri.query().map(str::to_owned),
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        m.approval
    };
    match (method.as_str(), uri.path()) {
        ("POST", "/api/v1/connect") => {
            if approval == Approval::Hold {
                let _ = approved.wait_for(|a| *a).await;
            }
            (StatusCode::OK, machine.lock().await.connect.to_string())
        }
        ("GET", "/api/v1/status") => {
            let m = machine.lock().await;
            let token = uri.query() == Some(format!("token={TOKEN}").as_str());
            let accepted = approval != Approval::Pending || *approved.borrow();
            if !token || !accepted || m.revoked {
                (StatusCode::UNAUTHORIZED, "{}".into())
            } else if m.failing {
                (StatusCode::INTERNAL_SERVER_ERROR, "{}".into())
            } else {
                (StatusCode::OK, m.status.to_string())
            }
        }
        ("POST", "/api/v1/disconnect") => (StatusCode::OK, "{}".into()),
        _ => (StatusCode::NOT_FOUND, "{}".into()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Acknowledge each request with its own sequence.
    Answer,
    /// Never reply.
    Silent,
    /// Acknowledge with a sequence the client did not use.
    WrongSequence,
    /// Send acknowledgements whose payload checksum or header CRC is wrong.
    Corrupt,
    /// Acknowledge correctly, one byte per write.
    Fragmented,
    /// Acknowledge module info with this nonzero result.
    Result(u8),
}

pub struct MachineJ1 {
    pub mode: Mode,
    /// Module ID and firmware version of each installed module.
    pub modules: Vec<(u16, String)>,
    pub model: u8,
    pub requests: Vec<Packet>,
    pub connections: usize,
    pub closed: usize,
}
pub struct FakeJ1 {
    pub machine: Arc<Mutex<MachineJ1>>,
    pub port: u16,
    close: watch::Sender<u64>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeJ1 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A module-info acknowledgement payload with a UInt8 count.
pub fn modules_payload(modules: &[(u16, String)]) -> Vec<u8> {
    let mut payload = vec![0, u8::try_from(modules.len()).unwrap()];
    for (index, (id, firmware)) in modules.iter().enumerate() {
        payload.push(u8::try_from(index).unwrap() + 1);
        payload.extend_from_slice(&id.to_le_bytes());
        payload.push(0);
        payload.push(1);
        payload.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        payload.push(2);
        payload.extend_from_slice(&u16::try_from(firmware.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(firmware.as_bytes());
    }
    payload
}
pub fn machine_payload(model: u8, firmware: &str) -> Vec<u8> {
    let mut payload = vec![0, model, 3];
    payload.extend_from_slice(&0xCAFEu32.to_le_bytes());
    for text in [firmware, "SN-PRIVATE"] {
        payload.extend_from_slice(&u16::try_from(text.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(text.as_bytes());
    }
    payload
}

impl FakeJ1 {
    pub async fn new(modules: &[u16]) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let machine = Arc::new(Mutex::new(MachineJ1 {
            mode: Mode::Answer,
            modules: modules
                .iter()
                .map(|id| (*id, "v1.0.2".to_owned()))
                .collect(),
            model: 4,
            requests: Vec::new(),
            connections: 0,
            closed: 0,
        }));
        let (close, _) = watch::channel(0);
        let task = tokio::spawn({
            let machine = machine.clone();
            let close = close.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    machine.lock().await.connections += 1;
                    let machine = machine.clone();
                    let receiver = close.subscribe();
                    tokio::spawn(async move {
                        serve(stream, machine.clone(), receiver).await;
                        machine.lock().await.closed += 1;
                    });
                }
            }
        });
        Self {
            machine,
            port,
            close,
            task,
        }
    }
    /// Closes every open connection.
    pub fn drop_connections(&self) {
        self.close.send_modify(|n| *n += 1);
    }
    pub async fn set_mode(&self, mode: Mode) {
        self.machine.lock().await.mode = mode;
    }
    pub async fn requests(&self) -> Vec<Packet> {
        self.machine.lock().await.requests.clone()
    }
    pub fn printer(&self) -> dutchman_print::config::Printer {
        dutchman_print::config::Printer {
            kind: Some("snapmaker-j1".into()),
            host: Some("127.0.0.1".into()),
            port: Some(self.port),
            id: "test".into(),
            name: "Test Snapmaker J1".into(),
            model: "Synthetic Snapmaker J1".into(),
            ..Default::default()
        }
    }
    pub fn config(&self) -> dutchman_print::config::Config {
        config(self.printer())
    }
}

fn ack(request: &Packet, sequence: u16, payload: Vec<u8>) -> Vec<u8> {
    Packet {
        receiver: sacp::SOFTWARE,
        sender: sacp::CONTROLLER,
        attribute: sacp::ACK,
        sequence,
        set: request.set,
        id: request.id,
        payload,
    }
    .encode()
    .unwrap()
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    machine: Arc<Mutex<MachineJ1>>,
    mut close: watch::Receiver<u64>,
) {
    let mut decoder = Decoder::default();
    let mut buffer = [0u8; 1024];
    loop {
        let read = tokio::select! {
            read = stream.read(&mut buffer) => read,
            _ = close.changed() => return,
        };
        let Ok(count) = read else {
            return;
        };
        if count == 0 {
            return;
        }
        decoder.push(&buffer[..count]);
        while let Some(request) = decoder.next_packet() {
            let (mode, reply) = {
                let mut m = machine.lock().await;
                m.requests.push(request.clone());
                let payload = match (request.id, m.mode) {
                    (sacp::MODULE_INFO, Mode::Result(code)) => vec![code],
                    (sacp::MODULE_INFO, _) => modules_payload(&m.modules),
                    (sacp::MACHINE_INFO, _) => machine_payload(m.model, "V1.2.3"),
                    _ => continue,
                };
                let sequence = match m.mode {
                    Mode::WrongSequence => request.sequence.wrapping_add(1000),
                    _ => request.sequence,
                };
                (m.mode, ack(&request, sequence, payload))
            };
            let written = match mode {
                Mode::Silent => Ok(()),
                Mode::Corrupt => {
                    let mut bad_sum = reply.clone();
                    let last = bad_sum.len() - 3;
                    bad_sum[last] ^= 0xFF;
                    let mut bad_crc = reply;
                    bad_crc[6] ^= 0xFF;
                    bad_sum.extend_from_slice(&bad_crc);
                    stream.write_all(&bad_sum).await
                }
                Mode::Fragmented => {
                    let mut result = Ok(());
                    for byte in reply {
                        result = stream.write_all(&[byte]).await;
                        if result.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                        tokio::task::yield_now().await;
                    }
                    result
                }
                _ => stream.write_all(&reply).await,
            };
            if written.is_err() {
                return;
            }
        }
    }
}
