//! Synthetic SDCP WebSocket printer on a test-owned loopback socket.
#![allow(dead_code)]
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message;

pub const MAINBOARD: &str = "000000000001d354";
pub const BRAND: &str = "979d4c788a4a78bc777a870f1a02867a";

pub struct Machine {
    pub status: Value,
    /// Status requests are acknowledged but no status is reported, and pushes are withheld.
    pub silent: bool,
    /// Acknowledgement code for control commands.
    pub ack: i64,
    /// Close the connection on the next control command without replying.
    pub swallow: bool,
    /// Status reported after an acknowledged Stop; defaults to STOPPED for the same task.
    pub stop_status: Option<Value>,
    pub reported_mainboard: String,
    pub requests: Vec<Value>,
    pub pings: usize,
    pub connections: usize,
}
pub struct Fake {
    pub machine: Arc<Mutex<Machine>>,
    pub port: u16,
    events: broadcast::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn resin(activity: &[i64], substate: i64, task: &str, file: &str, error: i64) -> Value {
    json!({
        "CurrentStatus": activity, "PreviousStatus": 0, "PrintScreen": 0, "ReleaseFilm": 0,
        "TempOfUVLED": 31.4, "TimeLapseStatus": 0, "TempOfBox": 25.2, "TempTargetBox": 30,
        "PrintInfo": {
            "Status": substate, "CurrentLayer": 100, "TotalLayer": 1000,
            "CurrentTicks": 65_000, "TotalTicks": 165_000, "Filename": file,
            "ErrorNumber": error, "TaskId": task
        }
    })
}
pub fn centauri(substate: i64, task: &str, file: &str) -> Value {
    json!({
        "CurrentStatus": [0], "TimeLapseStatus": 0, "PlatFormType": 0,
        "TempOfHotbed": 60.2, "TempOfNozzle": 215.4, "TempOfBox": 26.4,
        "TempTargetHotbed": 60, "TempTargetNozzle": 215, "TempTargetBox": 0,
        "PrintInfo": {
            "Status": substate, "CurrentLayer": 33, "TotalLayer": 165,
            "CurrentTicks": 1200, "TotalTicks": 9749, "Filename": file, "TaskId": task,
            "PrintSpeedPct": 100, "Progress": 20
        }
    })
}

fn status_message(machine: &Machine) -> String {
    json!({
        "Status": machine.status,
        "MainboardID": machine.reported_mainboard,
        "TimeStamp": 1_687_069_655,
        "Topic": format!("sdcp/status/{}", machine.reported_mainboard),
    })
    .to_string()
}
fn response(cmd: u64, request: &str, ack: i64) -> String {
    json!({
        "Id": BRAND,
        "Data": {
            "Cmd": cmd, "Data": {"Ack": ack}, "RequestID": request,
            "MainboardID": MAINBOARD, "TimeStamp": 1_687_069_655
        },
        "Topic": format!("sdcp/response/{MAINBOARD}"),
    })
    .to_string()
}

impl Fake {
    pub async fn new(status: Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let machine = Arc::new(Mutex::new(Machine {
            status,
            silent: false,
            ack: 0,
            swallow: false,
            stop_status: None,
            reported_mainboard: MAINBOARD.into(),
            requests: Vec::new(),
            pings: 0,
            connections: 0,
        }));
        let (events, _) = broadcast::channel(16);
        let task = tokio::spawn({
            let machine = machine.clone();
            let events = events.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let machine = machine.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        let receiver = events.subscribe();
                        if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                            machine.lock().await.connections += 1;
                            serve(socket, machine, events, receiver).await;
                        }
                    });
                }
            }
        });
        Self {
            machine,
            port,
            events,
            task,
        }
    }
    /// Replaces the status and pushes it to every connection, as the printer does on change.
    pub async fn set(&self, status: Value) {
        self.machine.lock().await.status = status;
        let _ = self.events.send(true);
    }
    pub async fn push(&self) {
        let _ = self.events.send(true);
    }
    /// Closes every open connection.
    pub fn drop_connections(&self) {
        let _ = self.events.send(false);
    }
    pub async fn commands(&self, cmd: u64) -> Vec<Value> {
        self.machine
            .lock()
            .await
            .requests
            .iter()
            .filter(|r| r.pointer("/Data/Cmd").and_then(Value::as_u64) == Some(cmd))
            .cloned()
            .collect()
    }
    pub async fn controls(&self) -> usize {
        self.machine
            .lock()
            .await
            .requests
            .iter()
            .filter(|r| r.pointer("/Data/Cmd").and_then(Value::as_u64) != Some(0))
            .count()
    }
    pub fn printer(&self, profile: &str) -> dutchman_print::config::Printer {
        dutchman_print::config::Printer {
            kind: Some("elegoo-sdcp".into()),
            host: Some("127.0.0.1".into()),
            port: Some(self.port),
            profile: Some(profile.into()),
            id: "test".into(),
            name: "Test SDCP".into(),
            model: "Synthetic SDCP".into(),
            ..Default::default()
        }
    }
    pub fn config(&self, profile: &str) -> dutchman_print::config::Config {
        dutchman_print::config::Config {
            backend: dutchman_print::config::Backend {
                kind: "printers".into(),
                slice: false,
            },
            slicer: None,
            slicers: Default::default(),
            printers: vec![self.printer(profile)],
        }
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn serve(
    mut socket: Socket,
    machine: Arc<Mutex<Machine>>,
    events: broadcast::Sender<bool>,
    mut receiver: broadcast::Receiver<bool>,
) {
    loop {
        tokio::select! {
            frame = socket.next() => {
                let Some(Ok(Message::Text(text))) = frame else {
                    if matches!(frame, Some(Ok(_))) { continue; }
                    return;
                };
                let mut replies = Vec::new();
                {
                    let mut m = machine.lock().await;
                    if text.as_str() == "ping" {
                        m.pings += 1;
                        replies.push("pong".to_owned());
                    } else {
                        let Ok(request) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        m.requests.push(request.clone());
                        let cmd = request.pointer("/Data/Cmd").and_then(Value::as_u64).unwrap_or(u64::MAX);
                        let id = request.pointer("/Data/RequestID").and_then(Value::as_str).unwrap_or_default().to_owned();
                        match cmd {
                            0 => {
                                replies.push(response(0, &id, 0));
                                if !m.silent {
                                    replies.push(status_message(&m));
                                }
                            }
                            129..=131 if m.swallow => {
                                m.swallow = false;
                                return;
                            }
                            129..=131 => {
                                replies.push(response(cmd, &id, m.ack));
                                if cmd == 130 && m.ack == 0 {
                                    let stopped = m.stop_status.take().unwrap_or_else(|| {
                                        let mut s = m.status.clone();
                                        s["CurrentStatus"] = json!([0]);
                                        s["PrintInfo"]["Status"] = json!(8);
                                        s
                                    });
                                    m.status = stopped;
                                    let _ = events.send(true);
                                }
                            }
                            _ => replies.push(response(cmd, &id, 0)),
                        }
                    }
                }
                for reply in replies {
                    if socket.send(Message::text(reply)).await.is_err() {
                        return;
                    }
                }
            }
            event = receiver.recv() => match event {
                Ok(true) => {
                    let message = {
                        let m = machine.lock().await;
                        (!m.silent).then(|| status_message(&m))
                    };
                    if let Some(message) = message {
                        if socket.send(Message::text(message)).await.is_err() {
                            return;
                        }
                    }
                }
                Ok(false) | Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
            },
        }
    }
}
