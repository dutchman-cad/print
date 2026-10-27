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
    pub stale_history: bool,
}
pub struct Shared {
    pub machine: Mutex<Machine>,
    pub entered: Notify,
    pub release: Notify,
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
            json!({"objects":["webhooks","print_stats","virtual_sdcard","display_status","extruder","heater_bed","toolhead","configfile","pause_resume","mcu"]})
        }
        ("GET", "/printer/objects/query") => {
            assert!(query.contains("print_stats") && query.contains("webhooks"));
            if m.missing {
                json!({"eventtime":1,"status":{}})
            } else if m.partial {
                json!({"eventtime":1,"status":{"heater_bed":{"temperature":59}}})
            } else {
                json!({"eventtime":1,"status":{"webhooks":{"state":m.webhook,"state_message":"secret http://private.invalid"},
                "print_stats":{"state":m.state,"filename":m.name,"print_duration":20,"message":"secret http://private.invalid"},
                "virtual_sdcard":{"is_active":m.active.unwrap_or(m.state=="printing"),"progress":m.progress},
                "display_status":{"progress":m.progress},"extruder":{"temperature":201.5},"heater_bed":{"temperature":59},
                "toolhead":{"extruder":"extruder","axis_minimum":[0,0,0,0],"axis_maximum":[180,180,180,0]},
                "mcu":{"mcu_version":m.mcu_version},"configfile":{"settings":m.settings,"config":m.config},"pause_resume":{"is_paused":m.state=="paused"}}})
            }
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
            return (StatusCode::CREATED, axum::Json(v)).into_response();
        }
        ("POST", "/printer/print/start") => {
            assert_eq!(query.strip_prefix("filename="), Some(m.name.as_str()));
            if m.conflict {
                return StatusCode::CONFLICT.into_response();
            }
            m.state = "printing".into();
            if !m.stale_history {
                m.id = Some("000001".into());
                m.history_state = "in_progress".into();
            }
            if m.lost {
                return Response::new(Body::new(Broken));
            }
            json!("ok")
        }
        ("POST", "/printer/print/pause") => {
            m.state = "paused".into();
            json!("ok")
        }
        ("POST", "/printer/print/resume") => {
            m.state = "printing".into();
            json!("ok")
        }
        ("POST", "/printer/print/cancel") => {
            m.state = "cancelled".into();
            m.history_state = "cancelled".into();
            json!("ok")
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(json!({"result":value})).into_response()
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
                stale_history: false,
            }),
            entered: Notify::new(),
            release: Notify::new(),
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
