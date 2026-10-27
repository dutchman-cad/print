//! Synthetic RepRapFirmware HTTP responses on a test-owned loopback socket.
#![allow(dead_code)]
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

pub struct Machine {
    pub password: Option<String>,
    pub sessions_full: bool,
    pub session_timeout: u64,
    pub keys: BTreeSet<String>,
    pub next_key: u64,
    pub connects: usize,
    pub status: String,
    pub file: Option<String>,
    pub last_file: Option<String>,
    pub position: u64,
    pub times_left: Value,
    pub current_tool: i64,
    pub tools: Value,
    pub heat: Value,
    pub firmware: String,
    pub files: BTreeMap<String, Vec<u8>>,
    /// Every authorized request as (path, decoded query).
    pub records: Vec<(String, String)>,
    pub commands: Vec<String>,
    pub upload_err: bool,
    pub upload_lost: bool,
    pub bad_size: bool,
    pub lost: bool,
    pub block: Option<String>,
    pub hold_controls: bool,
    pub expire_on_gcode: bool,
    pub delay_reads: usize,
    pub pending: Option<(String, Option<String>, Option<String>)>,
}
pub struct Shared {
    pub machine: Mutex<Machine>,
    pub entered: Notify,
    pub release: Notify,
    pub polled: Notify,
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
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}
fn params(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_owned(), decode(v))
        })
        .collect()
}
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in bytes {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
fn apply(m: &mut Machine, status: &str, file: Option<String>, last: Option<String>) {
    if m.delay_reads > 0 {
        m.pending = Some((status.into(), file, last));
    } else {
        m.status = status.into();
        m.file = file;
        m.last_file = last;
    }
}
async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().strip_prefix("/duet").unwrap().to_owned();
    let query = params(parts.uri.query().unwrap_or_default());
    let body = axum::body::to_bytes(body, 201 << 20).await.unwrap();
    let mut m = shared.machine.lock().await;
    if path == "/rr_connect" {
        m.connects += 1;
        assert_eq!(query.get("sessionKey").map(String::as_str), Some("yes"));
        if m.sessions_full {
            return axum::Json(json!({"err":2})).into_response();
        }
        if m.password.is_some() && query.get("password") != m.password.as_ref() {
            return axum::Json(json!({"err":1})).into_response();
        }
        m.next_key += 1;
        let key = m.next_key;
        m.keys.insert(key.to_string());
        return axum::Json(json!({"err":0,"sessionTimeout":m.session_timeout,"boardType":"synthetic","sessionKey":key}))
            .into_response();
    }
    if path == "/rr_gcode" && m.expire_on_gcode {
        m.expire_on_gcode = false;
        m.keys.clear();
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let key = parts
        .headers
        .get("x-session-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !m.keys.contains(key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let rendered = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    m.records.push((path.clone(), rendered));
    if m.block.as_deref() == Some(path.as_str()) {
        m.block = None;
        drop(m);
        shared.entered.notify_one();
        shared.release.notified().await;
        m = shared.machine.lock().await;
    }
    let value = match path.as_str() {
        "/rr_disconnect" => {
            m.keys.remove(key);
            json!({"err":0})
        }
        "/rr_model" => {
            let key = query.get("key").cloned().unwrap_or_default();
            let result = match key.as_str() {
                "state" => {
                    if m.status == "pausing" && m.hold_controls {
                        shared.polled.notify_one();
                    }
                    if m.delay_reads > 0 {
                        m.delay_reads -= 1;
                    } else if let Some((status, file, last)) = m.pending.take() {
                        m.status = status;
                        m.file = file;
                        m.last_file = last;
                    }
                    json!({"status":m.status,"currentTool":m.current_tool,"upTime":1})
                }
                "job" => {
                    let size = m
                        .file
                        .as_deref()
                        .and_then(|f| f.strip_prefix("0:/gcodes/"))
                        .and_then(|n| m.files.get(n))
                        .map_or(1000, Vec::len);
                    json!({"file":{"fileName":m.file,"size":if m.file.is_some(){json!(size)}else{Value::Null}},
                        "filePosition":if m.file.is_some(){json!(m.position)}else{Value::Null},
                        "timesLeft":{"file":m.times_left,"filament":null,"slicer":null},
                        "lastFileName":m.last_file})
                }
                "heat" => m.heat.clone(),
                "tools" => m.tools.clone(),
                "boards" => {
                    json!([{"name":"Synthetic board","firmwareName":"RepRapFirmware","firmwareVersion":m.firmware}])
                }
                "network" => json!({"name":"synthetic"}),
                "move.axes" => {
                    json!([{"letter":"X","min":0,"max":200},{"letter":"Y","min":0,"max":200},{"letter":"Z","min":0,"max":180}])
                }
                _ => return StatusCode::NOT_FOUND.into_response(),
            };
            json!({"key":key,"flags":query.get("flags"),"result":result})
        }
        "/rr_filelist" => {
            assert_eq!(query.get("dir").map(String::as_str), Some("0:/gcodes"));
            let first: usize = query.get("first").unwrap().parse().unwrap();
            let names: Vec<_> = m.files.iter().collect();
            let page: Vec<_> = names
                .iter()
                .skip(first)
                .take(2)
                .map(|(name, bytes)| json!({"type":"f","name":name,"size":bytes.len(),"date":"2026-01-01T00:00:00"}))
                .collect();
            let next = if first + 2 < names.len() {
                first + 2
            } else {
                0
            };
            json!({"dir":"0:/gcodes","first":first,"files":page,"next":next,"err":0})
        }
        "/rr_delete" => {
            let name = query.get("name").unwrap();
            let existed = name
                .strip_prefix("0:/gcodes/")
                .and_then(|n| m.files.remove(n))
                .is_some();
            json!({"err":if existed {0} else {1}})
        }
        "/rr_upload" => {
            let name = query.get("name").unwrap();
            let name = name.strip_prefix("0:/gcodes/").unwrap().to_owned();
            assert_eq!(
                query.get("crc32").unwrap(),
                &format!("{:08x}", crc32(&body))
            );
            let mut stored = body.to_vec();
            if m.bad_size {
                stored.push(b'\n');
            }
            if m.upload_err {
                stored.truncate(stored.len() / 2);
            }
            m.files.insert(name, stored);
            if m.upload_lost {
                return Response::new(Body::new(Broken));
            }
            json!({"err":if m.upload_err {1} else {0}})
        }
        "/rr_gcode" => {
            let code = query.get("gcode").cloned().unwrap_or_default();
            m.commands.push(code.clone());
            if let Some(file) = code
                .strip_prefix("M32 \"")
                .and_then(|c| c.strip_suffix('"'))
            {
                let name = file.strip_prefix("0:/gcodes/").unwrap();
                if m.files.contains_key(name) && m.status == "idle" {
                    let last = m.last_file.clone();
                    m.position = m.files[name].len() as u64 / 4;
                    apply(&mut m, "processing", Some(file.into()), last);
                }
                if m.lost {
                    return Response::new(Body::new(Broken));
                }
            } else if code == "M25" && m.status == "processing" {
                let (file, last) = (m.file.clone(), m.last_file.clone());
                if m.hold_controls {
                    m.status = "pausing".into();
                    shared.polled.notify_one();
                } else {
                    apply(&mut m, "paused", file, last);
                }
            } else if code == "M24" && m.status == "paused" {
                let (file, last) = (m.file.clone(), m.last_file.clone());
                apply(&mut m, "processing", file, last);
            } else if code == "M0" && m.status == "paused" {
                let file = m.file.clone();
                apply(&mut m, "idle", None, file);
            }
            json!({"buff":250})
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(value).into_response()
}
impl Fake {
    pub async fn new() -> Self {
        let shared = Arc::new(Shared {
            machine: Mutex::new(Machine {
                password: Some("secret".into()),
                sessions_full: false,
                session_timeout: 60_000,
                keys: BTreeSet::new(),
                next_key: 41,
                connects: 0,
                status: "idle".into(),
                file: None,
                last_file: None,
                position: 0,
                times_left: json!(321.4),
                current_tool: 0,
                tools: json!([{"number":0,"name":"Synthetic","heaters":[1],"extruders":[0],"state":"active"}]),
                heat: json!({"heaters":[{"current":59.0,"active":60,"standby":0,"state":"active"},{"current":201.5,"active":205,"standby":0,"state":"active"}],"bedHeaters":[0,-1,-1],"chamberHeaters":[-1]}),
                firmware: "3.5.0".into(),
                files: BTreeMap::new(),
                records: vec![],
                commands: vec![],
                upload_err: false,
                upload_lost: false,
                bad_size: false,
                lost: false,
                block: None,
                hold_controls: false,
                expire_on_gcode: false,
                delay_reads: 0,
                pending: None,
            }),
            entered: Notify::new(),
            release: Notify::new(),
            polled: Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/duet/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { shared, url, task }
    }
    pub fn config(&self) -> dutchman_print::config::Config {
        let mut c: dutchman_print::config::Config = toml::from_str("[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='duet'\nid='test'\nname='Test printer'\nmodel='Test model'\nnozzle_diameter=0.4\nprinter_profile='Machine'\nprint_profile='Draft'\nfilament_profile='PLA'").unwrap();
        c.printers[0].url = Some(self.url.clone());
        c.printers[0].password = Some("secret".into());
        c.slicer.as_mut().unwrap().path = Some(env!("CARGO_BIN_EXE_dutchman-fake-slicer").into());
        c
    }
    pub async fn commands(&self) -> Vec<String> {
        self.shared.machine.lock().await.commands.clone()
    }
    /// Starts an external job that this companion did not stage.
    pub async fn external(&self, status: &str) {
        let mut m = self.shared.machine.lock().await;
        m.files.insert("external.gcode".into(), vec![b'G'; 4000]);
        m.status = status.into();
        m.file = Some("0:/gcodes/external.gcode".into());
        m.position = 1000;
    }
}
