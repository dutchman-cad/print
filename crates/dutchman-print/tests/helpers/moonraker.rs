//! Synthetic Moonraker HTTP responses on a test-owned loopback socket.
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

pub struct Machine {
    pub state: String,
    pub klippy: String,
    pub webhook: String,
    pub connected: bool,
    pub name: String,
    pub id: Option<String>,
    pub history_state: String,
    pub bytes: Vec<u8>,
    pub records: Vec<(String, String, Bytes)>,
    pub auth: bool,
    pub require_key: bool,
    pub bad_size: bool,
    pub incomplete: bool,
    pub queued: bool,
    pub lost: bool,
    pub conflict: bool,
    pub missing: bool,
    pub partial: bool,
    pub slow: bool,
    pub oversized: bool,
    pub active: Option<bool>,
    pub progress: Value,
    pub settings: Value,
    pub config: Value,
    pub firmware: String,
    pub mcu_version: String,
    pub identity: String,
    pub block: Option<String>,
    pub hold_start: bool,
    pub hold_controls: bool,
    pub stale_history: bool,
    pub history_time: f64,
    pub delay_reads: usize,
    pub pending_state: Option<String>,
    pub upload_lost: bool,
    pub toolheads: usize,
    pub active_tool: String,
    pub websocket: bool,
    pub ws_connections: usize,
    pub ws_closed: usize,
    pub subscriptions: Vec<Value>,
    pub hold_baseline: bool,
    pub ws_eventtime: f64,
}
pub struct Shared {
    pub machine: Mutex<Machine>,
    pub entered: Notify,
    pub release: Notify,
    pub polled: Notify,
    pub ws_messages: tokio::sync::broadcast::Sender<Value>,
    pub baseline_release: Notify,
}
pub struct Fake {
    pub shared: Arc<Shared>,
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Broken;
impl hyper::body::Body for Broken {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Ready(Some(Err(std::io::Error::other("lost reply"))))
    }
}
async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    if request.uri().path() == "/proxy/websocket" {
        return websocket(shared, request).await;
    }
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().strip_prefix("/proxy").unwrap().to_owned();
    let query = parts.uri.query().unwrap_or_default();
    let body = axum::body::to_bytes(body, 201 << 20).await.unwrap();
    let mut m = shared.machine.lock().await;
    m.records
        .push((parts.method.to_string(), path.clone(), body.clone()));
    if m.auth || (m.require_key && parts.headers.get("x-api-key").is_none_or(|v| v != "secret")) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if m.slow && path == "/server/info" {
        drop(m);
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        return StatusCode::GATEWAY_TIMEOUT.into_response();
    }
    if m.oversized && path == "/server/info" {
        return "x".repeat((1 << 20) + 1).into_response();
    }
    if m.block.as_deref() == Some(path.as_str()) {
        m.block = None;
        drop(m);
        shared.entered.notify_one();
        shared.release.notified().await;
        m = shared.machine.lock().await;
    }
    let metadata = |m: &Machine| json!({"filename":m.name,"size":m.bytes.len()+usize::from(m.bad_size),"job_id":m.id,"print_start_time":if m.id.is_some(){json!(200)}else{Value::Null}});
    let value = match (parts.method.as_str(), path.as_str()) {
        ("GET", "/server/info") => {
            json!({"klippy_connected":m.connected,"klippy_state":m.klippy,"moonraker_version":"synthetic"})
        }
        ("GET", "/printer/info") => {
            json!({"state":m.klippy,"hostname":m.identity,"software_version":m.firmware})
        }
        ("GET", "/printer/objects/list") => {
            let mut objects = vec![
                "webhooks",
                "print_stats",
                "virtual_sdcard",
                "display_status",
                "extruder",
                "heater_bed",
                "toolhead",
                "configfile",
                "pause_resume",
                "mcu",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
            objects.extend((1..m.toolheads).map(|i| format!("extruder{i}")));
            json!({"objects":objects})
        }
        ("GET", "/server/history/list") => {
            json!({"jobs":if m.id.is_some(){vec![json!({"job_id":m.id,"start_time":m.history_time})]}else{vec![]}})
        }
        ("GET", "/printer/objects/query") => {
            assert!(query.contains("print_stats") && query.contains("webhooks"));
            status(&mut m, &shared)
        }

        ("GET", "/server/files/metadata") => {
            if query.strip_prefix("filename=") != Some(m.name.as_str()) || m.name.is_empty() {
                return StatusCode::NOT_FOUND.into_response();
            }
            metadata(&m)
        }
        ("GET", "/server/history/job") => {
            json!({"job":{"job_id":m.id,"filename":m.name,"status":m.history_state,"exists":true,"metadata":{"size":m.bytes.len()},"start_time":200,"end_time":if m.history_state=="in_progress" {Value::Null}else{json!(300)}}})
        }
        ("DELETE", p) if p.starts_with("/server/files/gcodes/") => {
            let name = m.name.clone();
            m.name.clear();
            m.bytes.clear();
            json!({"item":{"root":"gcodes","path":name,"size":0}})
        }
        ("POST", "/server/files/upload") => {
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.contains("name=\"print\"\r\n\r\nfalse"));
            assert!(text.contains("name=\"root\"\r\n\r\ngcodes"));
            let name = text
                .split("filename=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            let data = text
                .split("Content-Type: application/octet-stream\r\n\r\n")
                .nth(1)
                .unwrap()
                .split("\r\n--")
                .next()
                .unwrap();
            assert!(text.contains(&dutchman_print::safety::hash(data.as_bytes())));
            m.name = name.into();
            m.bytes = data.as_bytes().to_vec();
            let v = json!({"result":{"item":{"path":m.name,"root":"gcodes","size":m.bytes.len()},"print_started":m.incomplete,"print_queued":m.queued,"action":"create_file"}});
            if m.upload_lost {
                return Response::new(Body::new(Broken));
            }
            return (StatusCode::CREATED, axum::Json(v)).into_response();
        }
        ("POST", "/printer/print/start") => {
            assert_eq!(query.strip_prefix("filename="), Some(m.name.as_str()));
            if m.conflict {
                return StatusCode::CONFLICT.into_response();
            }
            if m.delay_reads > 0 {
                m.pending_state = Some("printing".into());
            } else {
                m.state = if m.hold_start { "paused" } else { "printing" }.into();
            }
            if !m.stale_history {
                let next =
                    m.id.as_deref()
                        .and_then(|i| i.parse::<u32>().ok())
                        .unwrap_or(0)
                        + 1;
                m.id = Some(format!("{next:06}"));
                m.history_state = "in_progress".into();
            }
            if m.lost {
                return Response::new(Body::new(Broken));
            }
            json!("ok")
        }
        ("POST", "/printer/print/pause") => {
            if m.hold_controls {
                shared.polled.notify_one();
                return axum::Json(json!({"result":"ok"})).into_response();
            }
            if m.delay_reads > 0 {
                m.pending_state = Some("paused".into());
            } else {
                m.state = "paused".into();
            }
            json!("ok")
        }
        ("POST", "/printer/print/resume") => {
            if m.hold_controls {
                shared.polled.notify_one();
                return axum::Json(json!({"result":"ok"})).into_response();
            }
            if m.delay_reads > 0 {
                m.pending_state = Some("printing".into());
            } else {
                m.state = "printing".into();
            }
            json!("ok")
        }
        ("POST", "/printer/print/cancel") => {
            if m.delay_reads > 0 {
                m.pending_state = Some("cancelled".into());
            } else {
                m.state = "cancelled".into();
            }
            m.history_state = "cancelled".into();
            json!("ok")
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if m.websocket
        && parts.method == axum::http::Method::POST
        && path.starts_with("/printer/print/")
    {
        m.ws_eventtime += 1.0;
        let eventtime = m.ws_eventtime;
        let update = status(&mut m, &shared);
        let _ = shared.ws_messages.send(json!({"jsonrpc":"2.0","method":"notify_status_update","params":[update["status"],eventtime]}));
    }
    axum::Json(json!({"result":value})).into_response()
}
fn status(m: &mut Machine, shared: &Shared) -> Value {
    if m.hold_start && m.state == "paused" {
        shared.polled.notify_one();
    }
    if m.delay_reads > 0 {
        m.delay_reads -= 1;
    } else if let Some(state) = m.pending_state.take() {
        m.state = state;
    }
    if m.missing {
        json!({"eventtime":1,"status":{}})
    } else if m.partial {
        json!({"eventtime":1,"status":{"heater_bed":{"temperature":59}}})
    } else {
        let mut status = json!({"webhooks":{"state":m.webhook,"state_message":"secret http://private.invalid"},
                "print_stats":{"state":m.state,"filename":m.name,"print_duration":20,"message":"secret http://private.invalid"},
                "virtual_sdcard":{"is_active":m.active.unwrap_or(m.state=="printing"),"progress":m.progress},
                "display_status":{"progress":m.progress},"extruder":{"temperature":201.5},"heater_bed":{"temperature":59},
                "toolhead":{"extruder":"extruder","axis_minimum":[0,0,0,0],"axis_maximum":[180,180,180,0]},
                "mcu":{"mcu_version":m.mcu_version},"configfile":{"settings":m.settings,"config":m.config},"pause_resume":{"is_paused":m.state=="paused"}});
        status["toolhead"]["extruder"] = json!(m.active_tool);
        for i in 1..m.toolheads {
            status[format!("extruder{i}")] = json!({"temperature":200.0 + i as f64});
        }
        json!({"eventtime":1,"status":status})
    }
}
async fn websocket(shared: Arc<Shared>, mut request: Request) -> Response {
    use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
    let mut m = shared.machine.lock().await;
    if !m.websocket {
        return StatusCode::NOT_FOUND.into_response();
    }
    if m.auth
        || (m.require_key
            && request
                .headers()
                .get("x-api-key")
                .is_none_or(|v| v != "secret"))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let key = request
        .headers()
        .get("sec-websocket-key")
        .unwrap()
        .as_bytes();
    let accept = derive_accept_key(key);
    m.ws_connections += 1;
    drop(m);
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::{protocol::Role, Message};
        let stream = hyper_util::rt::TokioIo::new(upgrade.await.unwrap());
        let mut socket =
            tokio_tungstenite::WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
        let mut messages = shared.ws_messages.subscribe();
        let request = socket.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
        let mut m = shared.machine.lock().await;
        m.subscriptions.push(request.clone());
        let hold = m.hold_baseline;
        drop(m);
        if hold {
            shared.baseline_release.notified().await;
        }
        let baseline = {
            let mut m = shared.machine.lock().await;
            status(&mut m, &shared)
        };
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":request["id"],"result":baseline})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        loop {
            tokio::select! {
                message = messages.recv() => {
                    let value = message.unwrap();
                    if value == json!("drop") {break;}
                    if socket.send(Message::Text(value.to_string().into())).await.is_err() {break;}
                }
                frame = socket.next() => match frame {
                    Some(Ok(Message::Ping(bytes))) => { if socket.send(Message::Pong(bytes)).await.is_err() {break;} }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
        shared.machine.lock().await.ws_closed += 1;
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", accept)
        .body(Body::empty())
        .unwrap()
}
impl Fake {
    pub async fn new() -> Self {
        let shared = Arc::new(Shared {
            machine: Mutex::new(Machine {
                state: "standby".into(),
                klippy: "ready".into(),
                webhook: "ready".into(),
                connected: true,
                name: String::new(),
                id: None,
                history_state: "in_progress".into(),
                bytes: vec![],
                records: vec![],
                auth: false,
                require_key: true,
                bad_size: false,
                incomplete: false,
                queued: false,
                lost: false,
                conflict: false,
                missing: false,
                partial: false,
                slow: false,
                oversized: false,
                active: None,
                progress: json!(0.25),
                settings: json!({"mcu":{"serial":"synthetic"},"extruder":{"nozzle_diameter":0.4}}),
                config: json!({"synthetic":"configuration"}),
                firmware: "synthetic".into(),
                mcu_version: "synthetic".into(),
                identity: "synthetic".into(),
                block: None,
                hold_start: false,
                hold_controls: false,
                stale_history: false,
                history_time: 200.25,
                delay_reads: 0,
                pending_state: None,
                upload_lost: false,
                toolheads: 1,
                active_tool: "extruder".into(),
                websocket: false,
                ws_connections: 0,
                ws_closed: 0,
                subscriptions: vec![],
                hold_baseline: false,
                ws_eventtime: 1000.0,
            }),
            entered: Notify::new(),
            release: Notify::new(),
            polled: Notify::new(),
            ws_messages: tokio::sync::broadcast::channel(32).0,
            baseline_release: Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/proxy/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { shared, url, task }
    }
    pub fn config(&self) -> dutchman_print::config::Config {
        let mut c:dutchman_print::config::Config=toml::from_str("[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='moonraker'\nid='test'\nname='Test printer'\nmodel='Test model'\nresume_commissioned=true\nprinter_profile='Machine'\nprint_profile='Draft'\nfilament_profile='PLA'").unwrap();
        c.printers[0].url = Some(self.url.clone());
        c.printers[0].api_key = Some("secret".into());
        c.slicer.as_mut().unwrap().path = Some(env!("CARGO_BIN_EXE_dutchman-fake-slicer").into());
        c
    }
    pub async fn commands(&self) -> Vec<String> {
        self.shared
            .machine
            .lock()
            .await
            .records
            .iter()
            .filter(|(method, path, _)| method == "POST" && path.starts_with("/printer/print/"))
            .map(|(_, p, _)| p.clone())
            .collect()
    }
}
