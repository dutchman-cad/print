//! Synthetic Repetier-Server REST responses on a test-owned loopback socket.
#![allow(dead_code)]
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    response::{IntoResponse, Response},
    Router,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub const KEY: &str = "synthetic-repetier-key";
pub const SLUG: &str = "bench_1";

#[derive(Clone)]
pub struct Item {
    pub id: u64,
    pub name: String,
    pub bytes: Vec<u8>,
}
/// One request as the server saw it.
#[derive(Clone, Debug)]
pub struct Record {
    pub method: String,
    pub path: String,
    pub query: String,
    pub key: Option<String>,
    pub action: Option<String>,
    pub data: Value,
}
pub struct Machine {
    pub key: String,
    pub online: Value,
    pub active: Value,
    pub job: Value,
    pub running: Option<u64>,
    pub listed: bool,
    pub extruders: Value,
    pub active_extruder: u64,
    pub bed: Value,
    pub config_extruders: usize,
    pub firmware: String,
    pub models: Vec<Item>,
    pub jobs: Vec<Item>,
    pub next_id: u64,
    pub records: Vec<Record>,
    /// Commands that change something, such as `startJob {"id":3}` or `upload name`.
    pub commands: Vec<String>,
    pub no_space: bool,
    pub upload_error: bool,
    pub upload_lost: bool,
    pub bad_length: bool,
    pub copy_lost: bool,
    pub autostart: bool,
    pub start_lost: bool,
    pub start_refused: bool,
    pub stop_lost: bool,
    pub delay_reads: usize,
    pub pending: Option<(Value, Option<u64>)>,
}
pub struct Shared {
    pub machine: Mutex<Machine>,
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
fn lost() -> Response {
    Response::new(Body::new(Broken))
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
/// The bytes of the multipart part named `field`.
fn part(content_type: &str, body: &[u8], field: &str) -> Option<Vec<u8>> {
    let boundary = format!("--{}", content_type.split("boundary=").nth(1)?);
    let text = body;
    let marker = format!("name=\"{field}\"");
    let start = text
        .windows(marker.len())
        .position(|w| w == marker.as_bytes())?;
    let header_end = start + text[start..].windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let end_marker = format!("\r\n{boundary}");
    let end = header_end
        + text[header_end..]
            .windows(end_marker.len())
            .position(|w| w == end_marker.as_bytes())?;
    Some(text[header_end..end].to_vec())
}
fn entry(item: &Item) -> Value {
    json!({"analysed":1,"created":1_700_000_000_000u64,"id":item.id,"name":item.name,
        "length":item.bytes.len(),"lines":10,"printTime":123.4,"printed":0,"printedTimeComp":0,
        "state":"stored","extruderUsage":[1.0],"filamentTotal":1.0,"volumeTotal":0,"volumeUsage":[0]})
}
/// Applies a job change now, or after the configured number of `listPrinter` reads.
fn apply(m: &mut Machine, job: Value, running: Option<u64>) {
    if m.delay_reads > 0 {
        m.pending = Some((job, running));
    } else {
        m.job = job;
        m.running = running;
    }
}
async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path()
        .strip_prefix("/repetier")
        .unwrap()
        .to_owned();
    let raw_query = parts.uri.query().unwrap_or_default().to_owned();
    let query = params(&raw_query);
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(body, 201 << 20).await.unwrap();
    let key = parts
        .headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut m = shared.machine.lock().await;
    let data: Value = query
        .get("data")
        .map(|d| serde_json::from_str(d).unwrap())
        .unwrap_or(Value::Null);
    m.records.push(Record {
        method: parts.method.to_string(),
        path: path.clone(),
        query: raw_query,
        key: key.clone(),
        action: query.get("a").cloned(),
        data: data.clone(),
    });
    if key.as_deref() != Some(m.key.as_str()) {
        return axum::Json(json!({"error":"Authorization required"})).into_response();
    }
    if path == format!("/printer/model/{SLUG}") {
        assert_eq!(parts.method, "POST");
        assert_eq!(query.get("a").map(String::as_str), Some("upload"));
        let name = query.get("name").unwrap().clone();
        let mut bytes = part(&content_type, &body, "filename").unwrap();
        m.commands.push(format!("upload {name}"));
        if m.bad_length {
            bytes.push(b'\n');
        }
        if m.upload_error {
            bytes.truncate(bytes.len() / 2);
        }
        m.next_id += 1;
        let id = m.next_id;
        m.models.push(Item { id, name, bytes });
        if m.upload_lost {
            return lost();
        }
        if m.upload_error {
            return axum::Json(json!({"error":"Upload failed"})).into_response();
        }
        return axum::Json(json!({})).into_response();
    }
    assert_eq!(path, format!("/printer/api/{SLUG}"), "printer slug");
    let action = query.get("a").cloned().unwrap_or_default();
    let changes = matches!(
        action.as_str(),
        "copyModel" | "startJob" | "stopJob" | "continueJob" | "removeJob" | "removeModel" | "send"
    );
    if changes {
        assert_eq!(parts.method, "POST", "{action}");
        m.commands.push(format!("{action} {data}"));
    }
    let id = data.get("id").and_then(Value::as_u64);
    let value = match action.as_str() {
        "listPrinter" => {
            // A pending change becomes visible after `delay_reads` further reads.
            if m.pending.is_some() {
                if m.delay_reads > 0 {
                    m.delay_reads -= 1;
                } else if let Some((job, running)) = m.pending.take() {
                    m.job = job;
                    m.running = running;
                }
            }
            let mut list =
                vec![json!({"active":true,"job":"none","name":"Other","online":1,"slug":"other"})];
            if m.listed {
                list.push(json!({"active":m.active,"job":m.job,"name":"Bench","online":m.online,"slug":SLUG}));
            }
            json!(list)
        }
        "stateList" => {
            json!({SLUG:{"activeExtruder":m.active_extruder,"extruder":m.extruders,"heatedBed":m.bed,
                "numExtruder":m.config_extruders,"firmware":m.firmware,"layer":0,"x":0,"y":0,"z":0},
                "other":{"activeExtruder":0,"extruder":[{"tempRead":99.0,"tempSet":0}],"heatedBed":{"tempRead":99.0,"tempSet":0}}})
        }
        "listJobs" => json!({"data":m.jobs.iter().map(entry).collect::<Vec<_>>()}),
        "listModels" => json!({"data":m.models.iter().map(entry).collect::<Vec<_>>()}),
        "canUploadGCodeWithSize" => {
            assert!(data.get("size").and_then(Value::as_u64).is_some());
            if m.no_space {
                json!({"ok":false,"error":"Not enough free disk space."})
            } else {
                json!({"ok":true})
            }
        }
        "version" => {
            json!({"build":1,"major":2,"minor":0,"name":"Repetier-Server 2.0.0","patch":0})
        }
        "getPrinterConfig" => {
            assert_eq!(data.get("printer").and_then(Value::as_str), Some(SLUG));
            let extruder = json!({"filamentDiameter":1.75,"maxTemp":260,"lastTemp":200});
            json!({"general":{"slug":SLUG,"name":"Bench","firmwareName":m.firmware,"printerVariant":"cartesian","heatedBed":true},
                "extruders":vec![extruder; m.config_extruders],
                "heatedBed":{"installed":true,"lastTemp":60},
                "movement":{"xMin":0,"xMax":200,"yMin":0,"yMax":200,"zMin":0,"zMax":180},
                "webcam":{"dynamicUrl":"http://camera.invalid/"}})
        }
        "copyModel" => {
            assert_eq!(data.get("autostart"), Some(&json!(false)));
            if let Some(model) = m.models.iter().find(|i| Some(i.id) == id).cloned() {
                m.next_id += 1;
                let job = Item {
                    id: m.next_id,
                    ..model
                };
                if m.autostart && m.running.is_none() {
                    apply(&mut m, json!(job.name), Some(job.id));
                }
                m.jobs.push(job);
            }
            if m.copy_lost {
                return lost();
            }
            json!({})
        }
        "startJob" => {
            if m.start_refused {
                return axum::Json(json!({"error":"Printer not ready"})).into_response();
            }
            let job = m.jobs.iter().find(|i| Some(i.id) == id).cloned();
            if let Some(job) = job {
                if m.running.is_none() && m.online == json!(1) {
                    apply(&mut m, json!(job.name), Some(job.id));
                }
            }
            if m.start_lost {
                return lost();
            }
            json!({})
        }
        "stopJob" => {
            if let Some(running) = m.running {
                m.jobs.retain(|j| j.id != running);
                apply(&mut m, json!("none"), None);
            }
            if m.stop_lost {
                return lost();
            }
            json!({})
        }
        "removeJob" => {
            if m.running == id {
                return axum::Json(json!({"error":"Job is printing"})).into_response();
            }
            m.jobs.retain(|j| Some(j.id) != id);
            json!({})
        }
        "removeModel" => {
            m.models.retain(|j| Some(j.id) != id);
            json!({})
        }
        _ => json!({"error":"Unknown command"}),
    };
    axum::Json(value).into_response()
}
impl Fake {
    pub async fn new() -> Self {
        let shared = Arc::new(Shared {
            machine: Mutex::new(Machine {
                key: KEY.into(),
                online: json!(1),
                active: json!(true),
                job: json!("none"),
                running: None,
                listed: true,
                extruders: json!([{"tempRead":201.5,"tempSet":205,"output":0}]),
                active_extruder: 0,
                bed: json!({"tempRead":59.0,"tempSet":60,"output":0}),
                config_extruders: 1,
                firmware: "Marlin".into(),
                models: vec![],
                jobs: vec![],
                next_id: 10,
                records: vec![],
                commands: vec![],
                no_space: false,
                upload_error: false,
                upload_lost: false,
                bad_length: false,
                copy_lost: false,
                autostart: false,
                start_lost: false,
                start_refused: false,
                stop_lost: false,
                delay_reads: 0,
                pending: None,
            }),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/repetier/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { shared, url, task }
    }
    pub fn config(&self) -> dutchman_print::config::Config {
        let mut c: dutchman_print::config::Config = toml::from_str("[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='repetier'\nid='test'\nname='Test printer'\nmodel='Test model'\nnozzle_diameter=0.4\nprinter_profile='Machine'\nprint_profile='Draft'\nfilament_profile='PLA'").unwrap();
        c.printers[0].url = Some(self.url.clone());
        c.printers[0].api_key = Some(KEY.into());
        c.printers[0].slug = Some(SLUG.into());
        c.slicer.as_mut().unwrap().path = Some(env!("CARGO_BIN_EXE_dutchman-fake-slicer").into());
        c
    }
    pub async fn commands(&self) -> Vec<String> {
        self.shared.machine.lock().await.commands.clone()
    }
    /// Commands by name only, such as `startJob`.
    pub async fn actions(&self) -> Vec<String> {
        self.commands()
            .await
            .iter()
            .map(|c| c.split(' ').next().unwrap().to_owned())
            .collect()
    }
    /// Runs a job that this companion did not stage.
    pub async fn external(&self) {
        let mut m = self.shared.machine.lock().await;
        m.next_id += 1;
        let id = m.next_id;
        m.jobs.push(Item {
            id,
            name: "external".into(),
            bytes: vec![b'G'; 4000],
        });
        m.job = json!("external");
        m.running = Some(id);
    }
    /// Ends the running job the way a finished print leaves the queue.
    pub async fn finish(&self) {
        let mut m = self.shared.machine.lock().await;
        if let Some(running) = m.running.take() {
            m.jobs.retain(|j| j.id != running);
        }
        m.job = json!("none");
    }
}
