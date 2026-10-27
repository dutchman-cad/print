//! Synthetic OctoPrint REST fixtures on test-owned loopback sockets.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
#[path = "helpers/slicer.rs"]
mod slicer_fixture;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use dutchman_print::{
    backend::{Backend, JobState, Options, PrinterState},
    config::Config,
    octoprint::Client,
    printers::Printers,
    safety::Safety,
};
use serde_json::{json, Value};
use std::{
    future::Future as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, Notify};

struct Machine {
    state: String,
    connection: Option<String>,
    flags: Option<Value>,
    name: Option<String>,
    origin: String,
    bytes: Vec<u8>,
    records: Vec<(Method, String, Bytes)>,
    auth: bool,
    bad_size: bool,
    selected_wrong: bool,
    lost: bool,
    conflict: bool,
    incomplete: bool,
    history: Value,
    profile: Value,
    block: bool,
    block_upload: bool,
    start_conflict: bool,
    missing: bool,
    progress: Value,
    wrong_path: bool,
    slow: bool,
    oversized: bool,
    delay: bool,
    transition: Option<(String, usize)>,
    external_history: Value,
    upload_lost: bool,
    delete_fails: bool,
}
struct Shared {
    machine: Mutex<Machine>,
    entered: Notify,
    release: Notify,
}
struct Fake {
    shared: Arc<Shared>,
    url: String,
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
    let path = parts
        .uri
        .path()
        .strip_prefix("/proxy")
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(body, 201 << 20).await.unwrap();
    let mut m = shared.machine.lock().await;
    m.records
        .push((parts.method.clone(), path.clone(), body.clone()));
    if m.auth || parts.headers.get("x-api-key").is_none_or(|v| v != "secret") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if m.slow && path == "/api/job" {
        drop(m);
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        return StatusCode::GATEWAY_TIMEOUT.into_response();
    }
    if m.oversized && path == "/api/job" {
        return "x".repeat((1 << 20) + 1).into_response();
    }
    if m.missing && path == "/api/job" {
        return axum::Json(json!({})).into_response();
    }
    if parts.method == Method::POST
        && ((m.block && path == "/api/job") || (m.block_upload && path == "/api/files/local"))
    {
        m.block = false;
        m.block_upload = false;
        drop(m);
        shared.entered.notify_one();
        shared.release.notified().await;
        m = shared.machine.lock().await;
    }
    let file = |m: &Machine| json!({"name":m.name,"path":if m.wrong_path {Some(format!("other/{}",m.name.as_deref().unwrap_or_default()))}else{m.name.clone()},"origin":m.origin,"size":m.bytes.len() + usize::from(m.bad_size),"type":"machinecode","prints":{"last":m.history}});
    match (parts.method.as_str(), path.as_str()) {
        ("GET", "/api/connection") => axum::Json(json!({"current":{"state":m.connection.as_ref().unwrap_or(&m.state),"port":"SYNTHETIC","baudrate":115200,"printerProfile":"test"}})).into_response(),
        ("GET", "/api/printer") => {
            let flags = m.flags.clone().unwrap_or_else(|| json!({"operational":!matches!(m.state.as_str(),"Offline"|"Offline after error"|"Opening serial connection"),"printing":matches!(m.state.as_str(),"Printing"|"Printing from SD"),"paused":m.state=="Paused","pausing":m.state=="Pausing","cancelling":m.state=="Cancelling","error":m.state=="Error","ready":m.state=="Operational","closedOrError":matches!(m.state.as_str(),"Offline"|"Offline after error")}));
            axum::Json(json!({"state":{"text":m.state,"flags":flags},"temperature":{"tool0":{"actual":201.5},"bed":{"actual":59.0}}})).into_response()
        }
        ("GET", "/api/job") => {
            if let Some((state, remaining)) = m.transition.take() {
                if remaining == 0 {
                    m.state = state;
                    if m.state == "Operational" { m.history = json!({"date":1793059200.875,"success":false}); }
                } else { m.transition = Some((state, remaining - 1)); }
            }
            axum::Json(json!({"state":m.state,"job":{"file":file(&m)},"progress":m.progress})).into_response()
        },
        ("GET", "/api/files") => {
            let mut files = vec![json!({"name":"external.gcode","path":"folder/external.gcode","origin":"local","prints":{"last":m.external_history}})];
            if m.name.is_some() { files.push(file(&m)); }
            axum::Json(json!({"files":files})).into_response()
        },
        ("DELETE", p) if p.starts_with("/api/files/local/") => {
            if m.delete_fails { return StatusCode::CONFLICT.into_response(); }
            m.name = None;
            m.bytes.clear();
            StatusCode::NO_CONTENT.into_response()
        },
        ("GET", "/api/version") => axum::Json(json!({"api":"0.1","server":"synthetic"})).into_response(),
        ("GET", "/api/printerprofiles") => axum::Json(json!({"profiles":{"test":m.profile}})).into_response(),
        ("POST", "/api/files/local") => {
            let text = std::str::from_utf8(&body).unwrap();
            let name = text.split("filename=\"").nth(1).unwrap().split('"').next().unwrap();
            m.name = Some(name.into());
            let data = text.split("Content-Type: application/octet-stream\r\n\r\n").nth(1).unwrap();
            m.bytes = data.split("\r\n--").next().unwrap().as_bytes().to_vec();
            if m.upload_lost { return Response::new(Body::new(Broken)); }
            axum::response::Response::builder().status(201).header("Content-Type","application/json")
                .body(Body::from(json!({"done":!m.incomplete,"effectiveSelect":false,"effectivePrint":false}).to_string())).unwrap()
        }
        ("GET", p) if p.starts_with("/api/files/local/") => {
            if m.name.as_deref() != p.strip_prefix("/api/files/local/") {StatusCode::NOT_FOUND.into_response()}
            else {axum::Json(file(&m)).into_response()}
        }
        ("POST", p) if p.starts_with("/api/files/local/") => {
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v, json!({"command":"select","print":false}));
            if m.conflict {return StatusCode::CONFLICT.into_response();}
            if m.selected_wrong {m.name = Some("external.gcode".into());}
            StatusCode::NO_CONTENT.into_response()
        }
        ("POST", "/api/job") => {
            if m.conflict || m.start_conflict {return StatusCode::CONFLICT.into_response();}
            let value: Value = serde_json::from_slice(&body).unwrap();
            if m.delay {
                let (transition, final_state) = match value["command"].as_str().unwrap() {
                    "start" => ("Starting", "Printing"),
                    "cancel" => ("Cancelling", "Operational"),
                    "pause" if value["action"] == "pause" => ("Pausing", "Paused"),
                    "pause" => ("Resuming", "Printing"),
                    _ => unreachable!(),
                };
                m.state = transition.into();
                m.transition = Some((final_state.into(), 1));
                return StatusCode::NO_CONTENT.into_response();
            }
            match value["command"].as_str().unwrap() {
                "start" => {m.state = "Printing".into(); if m.lost {return Response::new(Body::new(Broken));}},
                "pause" => match value["action"].as_str().unwrap() {"pause" => m.state="Paused".into(), "resume" => m.state="Printing".into(), _ => unreachable!()},
                "cancel" => {m.state="Operational".into(); m.history=json!({"date":1793059200.875,"success":false});},
                _ => unreachable!(),
            }
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
impl Fake {
    async fn new() -> Self {
        let shared = Arc::new(Shared {
            machine: Mutex::new(Machine {
                state: "Operational".into(),
                connection: None,
                flags: None,
                name: None,
                origin: "local".into(),
                bytes: vec![],
                records: vec![],
                auth: false,
                bad_size: false,
                selected_wrong: false,
                lost: false,
                conflict: false,
                incomplete: false,
                history: Value::Null,
                profile: json!({"id":"test","model":"Test model","volume":{"width":180,"depth":180,"height":180},"extruder":{"count":1,"nozzleDiameter":0.4},"heatedBed":true}),
                block: false,
                block_upload: false,
                start_conflict: false,
                missing: false,
                progress: json!({"completion":25.0,"printTimeLeft":60}),
                wrong_path: false,
                slow: false,
                oversized: false,
                delay: false,
                transition: None,
                external_history: Value::Null,
                upload_lost: false,
                delete_fails: false,
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
    fn config(&self) -> Config {
        let mut c:Config=toml::from_str("[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='octoprint'\nid='test'\nname='Test printer'\nmodel='Test model'\nprinter_profile='Machine'\nprint_profile='Draft'\nfilament_profile='PLA'").unwrap();
        c.printers[0].url = Some(self.url.clone());
        c.printers[0].api_key = Some("secret".into());
        c.slicer.as_mut().unwrap().path = Some(env!("CARGO_BIN_EXE_dutchman-fake-slicer").into());
        c
    }
    async fn backend(&self, dir: &Path) -> Arc<Safety> {
        let input = input(dir);
        let b = Arc::new(Safety::new(Arc::new(Printers::new(self.config()).unwrap())));
        b.prepare("one", &input, "test", Options::default())
            .await
            .unwrap();
        b
    }
    async fn commands(&self) -> Vec<Value> {
        self.shared
            .machine
            .lock()
            .await
            .records
            .iter()
            .filter(|(method, path, _)| *method == Method::POST && path == "/api/job")
            .map(|(_, _, body)| serde_json::from_slice(body).unwrap())
            .collect()
    }
}
fn input(dir: &Path) -> PathBuf {
    slicer_fixture::input(dir, |input| {
        vec![
            "--export-gcode".into(),
            "--no-binary-gcode".into(),
            format!(
                "--output={}",
                input.parent().unwrap().join("sliced.gcode").display()
            ),
            "--printer-profile=Machine".into(),
            "--print-profile=Draft".into(),
            "--material-profile=PLA".into(),
            input.display().to_string(),
        ]
    })
}
#[tokio::test]
async fn states_serial_connection_authentication_and_optional_values() {
    let f = Fake::new().await;
    let client = Client::new(f.config().printers.remove(0)).unwrap();
    for (state, expected) in [
        ("Operational", PrinterState::Idle),
        ("Printing", PrinterState::Printing),
        ("Pausing", PrinterState::Printing),
        ("Cancelling", PrinterState::Printing),
        ("Starting", PrinterState::Printing),
        ("Starting print from SD", PrinterState::Printing),
        ("Resuming", PrinterState::Printing),
        ("Finishing", PrinterState::Printing),
        ("Printing from SD", PrinterState::Printing),
        ("Sending file to SD", PrinterState::Offline),
        ("Starting to send file to SD", PrinterState::Offline),
        ("Transferring file to SD", PrinterState::Offline),
        ("Transferring file to printer", PrinterState::Offline),
        ("Connecting", PrinterState::Offline),
        ("Detecting", PrinterState::Offline),
        ("Detecting serial port", PrinterState::Offline),
        ("Detecting baudrate", PrinterState::Offline),
        ("Paused", PrinterState::Paused),
        ("Error", PrinterState::Error),
        ("Offline", PrinterState::Offline),
        ("Offline after error", PrinterState::Error),
        ("Opening serial connection", PrinterState::Offline),
        ("Future", PrinterState::Offline),
    ] {
        f.shared.machine.lock().await.state = state.into();
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.printer.state, expected, "{state}");
        if expected != PrinterState::Idle {
            assert!(
                dutchman_print::octoprint::ready(&snapshot).is_err(),
                "{state}"
            );
        }
    }
    {
        let mut m = f.shared.machine.lock().await;
        m.state = "Printing".into();
        m.name = Some("synthetic.gcode".into());
    }
    let p = client.snapshot().await.unwrap().printer;
    assert_eq!(p.progress, Some(0.25));
    assert_eq!(p.seconds_left, Some(60));
    assert_eq!(p.nozzle_temperature, Some(201.5));
    assert_eq!(p.job_name.as_deref(), Some("synthetic.gcode"));
    f.shared.machine.lock().await.progress = json!({"completion":null,"printTimeLeft":null});
    assert_eq!(client.snapshot().await.unwrap().printer.progress, None);
    f.shared.machine.lock().await.auth = true;
    assert!(matches!(
        client.snapshot().await,
        Err(dutchman_print::backend::Error::Authentication)
    ));
    let raw = Printers::new(f.config()).unwrap();
    let report = serde_json::to_string(&raw.printers().await.unwrap()).unwrap();
    assert!(!report.contains("secret"));
    assert!(!report.contains(&f.url));
    assert!(report.contains("Authentication failed"));
}
#[tokio::test]
async fn start_stages_without_print_then_explicit_controls_and_completion_need_clearance() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    assert!(b
        .start("one")
        .await
        .unwrap_err()
        .to_string()
        .contains("clear bed"));
    b.clear("test").await.unwrap();
    assert_eq!(b.start("one").await.unwrap().state, JobState::Printing);
    assert_eq!(b.pause("test").await.unwrap().state, JobState::Paused);
    assert!(b.pause("test").await.is_err());
    assert_eq!(b.resume("test").await.unwrap().state, JobState::Printing);
    let commands = f.commands().await;
    assert_eq!(
        commands,
        vec![
            json!({"command":"start"}),
            json!({"command":"pause","action":"pause"}),
            json!({"command":"pause","action":"resume"})
        ]
    );
    let m = f.shared.machine.lock().await;
    let upload = m
        .records
        .iter()
        .find(|(method, path, _)| *method == Method::POST && path == "/api/files/local")
        .unwrap();
    let body = std::str::from_utf8(&upload.2).unwrap();
    assert!(body.contains("name=\"print\"\r\n\r\nfalse"));
    assert!(body.contains("name=\"select\"\r\n\r\nfalse"));
    drop(m);
    f.shared.machine.lock().await.state = "Finishing".into();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    assert_eq!(
        b.printers().await.unwrap()[0].detail.as_deref(),
        Some("Finishing")
    );
    f.shared.machine.lock().await.state = "Operational".into();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    f.shared.machine.lock().await.history = json!({"date":1793059200.875,"success":true});
    assert_eq!(b.job("one").await.unwrap().state, JobState::Done);
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    b.clear("test").await.unwrap();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
}
#[tokio::test]
async fn validation_conflicts_selection_and_changed_review_never_start() {
    for mode in [
        "size",
        "incomplete",
        "selection",
        "conflict",
        "profile",
        "bytes",
        "serial",
        "missing",
        "unknown",
        "slot",
    ] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = f.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "size" => m.bad_size = true,
                "incomplete" => m.incomplete = true,
                "selection" => m.selected_wrong = true,
                "conflict" => m.conflict = true,
                "profile" => m.profile["volume"]["width"] = json!(200),
                "bytes" => std::fs::write(dir.path().join("sliced.gcode"), b"G28\n").unwrap(),
                "serial" => m.state = "Offline".into(),
                "missing" => m.missing = true,
                "unknown" => m.state = "Future".into(),
                "slot" => m.profile["extruder"]["count"] = json!(2),
                _ => unreachable!(),
            }
        }
        assert!(b.start("one").await.is_err(), "{mode}");
        assert!(f.commands().await.is_empty(), "{mode}");
    }
}
#[tokio::test]
async fn lost_start_is_reconciled_once_and_other_live_jobs_are_never_controlled() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.lost = true;
    assert!(b
        .start("one")
        .await
        .unwrap_err()
        .to_string()
        .contains("reply was lost"));
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    assert!(b.start("one").await.is_err());
    for origin in ["sdcard", "local"] {
        {
            let mut m = f.shared.machine.lock().await;
            m.origin = origin.into();
            if origin == "local" {
                m.name = Some("external.gcode".into());
            }
        }
        assert!(b.pause("test").await.is_err());
        assert!(b.cancel("one").await.is_err());
    }
    assert_eq!(f.commands().await.len(), 1);
}
#[tokio::test]
async fn stop_during_pending_start_cancels_only_the_confirmed_job() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.block = true;
    let start = tokio::spawn({
        let b = b.clone();
        async move { b.start("one").await }
    });
    f.shared.entered.notified().await;
    let cancel = b.cancel("one");
    tokio::pin!(cancel);
    // Poll until cancellation has registered the stop and is waiting on Start's lock.
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(cancel.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    f.shared.release.notify_one();
    let _ = start.await.unwrap();
    assert_eq!(cancel.await.unwrap().state, JobState::Cancelled);
    assert_eq!(
        f.commands().await,
        vec![json!({"command":"start"}), json!({"command":"cancel"})]
    );
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
}
#[tokio::test]
async fn stale_history_and_failed_outcomes_remain_distinct() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    f.shared.machine.lock().await.history = json!({"date":1793059200.125,"success":true});
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    f.shared.machine.lock().await.state = "Operational".into();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    f.shared.machine.lock().await.history = json!({"date":1793059200.875,"success":false});
    assert_eq!(b.job("one").await.unwrap().state, JobState::Failed);
    assert!(b.resume("test").await.is_err());
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
}

#[test]
fn config_accepts_https_and_base_paths_and_refuses_secret_or_ambiguous_urls() {
    let mut p = dutchman_print::config::Printer {
        api_key: Some("secret".into()),
        ..Default::default()
    };
    for url in ["http://127.0.0.1:5000", "https://127.0.0.1/proxy/path/"] {
        p.url = Some(url.into());
        assert!(dutchman_print::octoprint::validate(&p).is_ok());
    }
    for url in [
        "file:///secret",
        "http://maker:secret@127.0.0.1",
        "http://127.0.0.1/?secret",
        "https://127.0.0.1/../secret",
        "https://127.0.0.1/%2fsecret",
        "http://127.0.0.1:0",
    ] {
        p.url = Some(url.into());
        let error = dutchman_print::octoprint::validate(&p).unwrap_err();
        assert!(!error.contains("secret"));
    }
    p.url = Some("https://127.0.0.1".into());
    p.username = Some("maker".into());
    assert!(dutchman_print::octoprint::validate(&p).is_err());
    p.username = None;
    p.cert_sha256 = Some("0".repeat(64));
    assert!(dutchman_print::octoprint::validate(&p).is_err());
}

#[tokio::test]
async fn untrusted_tls_identity_never_receives_credentials() {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    for _ in 0..2 {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "https://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(socket)
                .await;
            assert!(tls.is_err());
        });
        let p = dutchman_print::config::Printer {
            url: Some(url),
            api_key: Some("secret".into()),
            ..Default::default()
        };
        assert!(Client::new(p).unwrap().snapshot().await.is_err());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn lost_start_checkpoint_survives_restart_without_repeating_start() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = input(dir.path());
    let path = dir.path().join("jobs.json");
    let raw = Printers::new(f.config()).unwrap().with_store(path.clone());
    raw.prepare("one", &input, "test", Options::default())
        .await
        .unwrap();
    f.shared.machine.lock().await.lost = true;
    assert!(raw.start("one").await.is_err());
    let restored = Printers::new(f.config()).unwrap();
    restored.restore_jobs(&path).await.unwrap();
    assert_eq!(restored.job("one").await.unwrap().state, JobState::Printing);
    assert!(restored.start("one").await.is_err());
    assert_eq!(f.commands().await.len(), 1);
    let state = std::fs::read_to_string(path).unwrap();
    assert!(!state.contains("secret"));
    assert!(!state.contains(&f.url));
}

#[tokio::test]
async fn unknown_or_contradictory_resume_telemetry_never_moves_the_printer() {
    for mode in [
        "unknown",
        "missing",
        "flags",
        "connection",
        "not_operational",
        "contradictory_flags",
    ] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = f.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        b.pause("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "unknown" => m.state = "Future".into(),
                "missing" => m.missing = true,
                "flags" => m.flags = Some(json!({"operational":true})),
                "connection" => m.connection = Some("Printing".into()),
                "not_operational" | "contradictory_flags" => {
                    m.flags = Some(json!({
                        "operational":mode != "not_operational", "paused":true,
                        "printing":mode == "contradictory_flags", "pausing":false,
                        "cancelling":false,"error":false,"closedOrError":false,"ready":true
                    }))
                }
                _ => unreachable!(),
            }
        }
        assert!(b.resume("test").await.is_err());
        assert_eq!(f.commands().await.len(), 2);
    }
}

#[tokio::test]
async fn api_machine_routes_require_fresh_confirmation_before_backend_commands() {
    use tower::ServiceExt as _;
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let raw = Arc::new(Printers::new(f.config()).unwrap());
    let safety = Safety::new(raw.clone());
    safety
        .prepare("one", &input(dir.path()), "test", Options::default())
        .await
        .unwrap();
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.path().join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    safety
        .save(&settings.storage.data_dir.join("safety.json"))
        .await
        .unwrap();
    let app = dutchman_print::router(raw, settings);
    for path in [
        "/api/v1/jobs/one/start",
        "/api/v1/jobs/one/cancel",
        "/api/v1/printers/test/pause",
        "/api/v1/printers/test/resume",
        "/api/v1/printers/test/clear",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let issue_start = || {
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/confirmations")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({"action":"start","printer":"test","job":"one"}).to_string(),
            ))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(issue_start()).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    api_action(&app, "clear", None, 1).await;
    f.shared.machine.lock().await.profile["extruder"]["nozzleDiameter"] = json!(0.6);
    let response = app.clone().oneshot(issue_start()).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("0.4") && text.contains("0.6"));
    f.shared.machine.lock().await.profile["extruder"]["nozzleDiameter"] = json!(0.4);
    f.shared.machine.lock().await.flags = Some(json!({"operational":true}));
    assert_eq!(
        app.clone().oneshot(issue_start()).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    f.shared.machine.lock().await.flags = None;
    api_action(&app, "clear", None, 6).await;
    api_action(&app, "start", Some("one"), 2).await;
    api_action(&app, "pause", None, 3).await;
    api_action(&app, "resume", None, 4).await;
    api_action(&app, "cancel", Some("one"), 5).await;
    assert_eq!(f.commands().await.len(), 4);
}

#[tokio::test]
async fn job_conflict_consumes_the_attempt_and_is_not_retried() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.start_conflict = true;
    assert!(b.start("one").await.is_err());
    f.shared.machine.lock().await.start_conflict = false;
    b.clear("test").await.unwrap();
    assert!(b
        .start("one")
        .await
        .unwrap_err()
        .to_string()
        .contains("already attempted"));
    assert_eq!(f.commands().await.len(), 1);
}

#[tokio::test]
async fn stop_during_upload_is_applied_after_the_matching_start() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.block_upload = true;
    let start = tokio::spawn({
        let b = b.clone();
        async move { b.start("one").await }
    });
    f.shared.entered.notified().await;
    let cancel = b.cancel("one");
    tokio::pin!(cancel);
    // Poll until cancellation has registered the stop and is waiting on Start's lock.
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(cancel.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    f.shared.release.notify_one();
    let _ = start.await.unwrap();
    assert_eq!(cancel.await.unwrap().state, JobState::Cancelled);
    assert_eq!(
        f.commands().await,
        vec![json!({"command":"start"}), json!({"command":"cancel"})]
    );
}

#[tokio::test]
async fn orcaslicer_selection_exports_text_gcode_for_octoprint() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let presets = root.join("presets");
    for (category, name) in [
        ("machine", "Machine"),
        ("process", "Draft"),
        ("filament", "PLA"),
    ] {
        let folder = presets.join("user/default").join(category);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("preset.json"), json!({"name":name}).to_string()).unwrap();
    }
    let input = slicer_fixture::input(&root, |input| {
        vec![
            format!("--datadir={}", presets.display()),
            format!(
                "--load-filaments={}",
                presets.join("user/default/filament/preset.json").display()
            ),
            format!(
                "--load-settings={};{}",
                presets.join("user/default/machine/preset.json").display(),
                presets.join("user/default/process/preset.json").display()
            ),
            "--slice=0".into(),
            "--export-3mf=sliced.3mf".into(),
            format!("--outputdir={}", root.display()),
            input.display().to_string(),
        ]
    });
    let mut config = f.config();
    let slicer = config.slicer.as_mut().unwrap();
    slicer.kind = "orcaslicer".into();
    slicer.datadir = Some(presets);
    let b = Safety::new(Arc::new(Printers::new(config).unwrap()));
    assert_eq!(
        b.prepare("one", &input, "test", Options::default())
            .await
            .unwrap()
            .seconds,
        Some(123)
    );
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    let m = f.shared.machine.lock().await;
    assert!(m.bytes.starts_with(b"; nozzle_diameter"));
    assert!(!m.bytes.contains(&0));
    assert!(m.name.as_ref().unwrap().ends_with(".gcode"));
}

#[tokio::test]
async fn input_and_profile_mismatches_name_the_refusal() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    std::fs::write(dir.path().join("input.3mf"), b"changed").unwrap();
    assert!(b
        .start("one")
        .await
        .unwrap_err()
        .to_string()
        .contains("Prepared file or printer changed"));
    for (field, label) in [
        ("printer", "printer"),
        ("print", "print"),
        ("material", "material"),
    ] {
        let mut options = Options::default();
        match field {
            "printer" => options.printer_profile = Some("Wrong model or nozzle".into()),
            "print" => options.print_profile = Some("Wrong plate".into()),
            _ => options.filament_profile = Some("Wrong material".into()),
        }
        let error = b
            .prepare("two", &dir.path().join("input.3mf"), "test", options)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(label));
        assert!(error.contains("profile differs"));
    }
    assert!(f.commands().await.is_empty());
}

#[tokio::test]
async fn identical_basename_in_another_folder_is_not_the_tracked_job() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    f.shared.machine.lock().await.wrong_path = true;
    assert!(b.pause("test").await.is_err());
    assert!(b.cancel("one").await.is_err());
    assert_eq!(f.commands().await.len(), 1);
}

#[tokio::test]
async fn stale_and_oversized_telemetry_blocks_readiness() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.oversized = true;
    assert!(b.start("one").await.is_err());
    assert!(f.commands().await.is_empty());
    {
        let mut m = f.shared.machine.lock().await;
        m.oversized = false;
        m.slow = true;
    }
    let client = Client::new(f.config().printers.remove(0)).unwrap();
    let began = std::time::Instant::now();
    assert!(client.snapshot().await.is_err());
    assert!(began.elapsed() < std::time::Duration::from_secs(6));
}

#[tokio::test]
async fn delayed_transitions_are_polled_without_repeating_commands() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.delay = true;
    assert_eq!(b.start("one").await.unwrap().state, JobState::Printing);
    assert_eq!(b.pause("test").await.unwrap().state, JobState::Paused);
    assert_eq!(b.resume("test").await.unwrap().state, JobState::Printing);
    assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(f.commands().await.len(), 4);
}

#[tokio::test]
async fn fractional_history_orders_results_within_the_same_second() {
    for success in [true, false] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = f.backend(dir.path()).await;
        f.shared.machine.lock().await.history = json!({"date":1793059200.5,"success":true});
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        f.shared.machine.lock().await.state = "Operational".into();
        for date in [
            json!(1793059200.25),
            json!(1793059200.5),
            Value::Null,
            json!("1793059200.75"),
        ] {
            f.shared.machine.lock().await.history = json!({"date":date,"success":success});
            assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
        }
        let history = json!({"date":1793059200.75,"success":success});
        assert!(history["date"].as_u64().is_none());
        f.shared.machine.lock().await.history = history;
        assert_eq!(
            b.job("one").await.unwrap().state,
            if success {
                JobState::Done
            } else {
                JobState::Failed
            }
        );
    }
}

#[tokio::test]
async fn flags_precede_text_but_inconsistent_text_cannot_authorize_start() {
    let f = Fake::new().await;
    let client = Client::new(f.config().printers.remove(0)).unwrap();
    for (key, expected) in [
        ("printing", PrinterState::Printing),
        ("pausing", PrinterState::Printing),
        ("cancelling", PrinterState::Printing),
        ("paused", PrinterState::Paused),
        ("error", PrinterState::Error),
        ("closedOrError", PrinterState::Offline),
    ] {
        let mut flags = json!({"operational":true,"printing":false,"pausing":false,"cancelling":false,"paused":false,"error":false,"closedOrError":false,"ready":true});
        flags[key] = json!(true);
        f.shared.machine.lock().await.flags = Some(flags);
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.printer.state, expected, "{key}");
        assert!(dutchman_print::octoprint::ready(&snapshot).is_err());
    }
}

async fn api_action(app: &Router, action: &str, job: Option<&str>, token: u64) {
    use tower::ServiceExt as _;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/confirmations")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"action":action,"printer":"test","job":job}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        code,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let challenge: Value = serde_json::from_slice(&bytes).unwrap();
    let route = match job {
        Some(job) => format!("/api/v1/jobs/{job}/{action}"),
        None => format!("/api/v1/printers/test/{action}"),
    };
    let request = || {
        axum::http::Request::builder()
            .method("POST")
            .uri(&route)
            .header("Content-Type", "application/json")
            .header("Plugin-Confirmation", format!("{token:064x}"))
            .body(Body::from(
                json!({"confirmation":challenge["id"]}).to_string(),
            ))
            .unwrap()
    };
    let response = app.clone().oneshot(request()).await.unwrap();
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        code,
        if action == "clear" {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::OK
        },
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn history_on_any_file_or_polled_activity_withdraws_clearance() {
    for mode in ["history", "identity", "state"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = f.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        assert_eq!(b.printer("test").await.unwrap().needs_clearing, Some(false));
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "history" => m.external_history = json!({"date":1793059200.75,"success":true}),
                "identity" => m.name = Some("external.gcode".into()),
                _ => m.state = "Printing".into(),
            }
        }
        b.printer("test").await.unwrap();
        f.shared.machine.lock().await.state = "Operational".into();
        assert_eq!(b.printer("test").await.unwrap().needs_clearing, Some(true));
        assert!(b.start("one").await.is_err());
        assert!(f.commands().await.is_empty());
        b.clear("test").await.unwrap();
        assert_eq!(b.printer("test").await.unwrap().needs_clearing, Some(false));
    }
}

#[tokio::test]
async fn failed_or_uncertain_upload_is_deleted_before_returning() {
    for mode in ["incomplete", "lost", "size", "cleanup"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = f.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            m.incomplete = mode == "incomplete" || mode == "cleanup";
            m.upload_lost = mode == "lost";
            m.bad_size = mode == "size";
            m.delete_fails = mode == "cleanup";
        }
        let error = b.start("one").await.unwrap_err().to_string();
        let m = f.shared.machine.lock().await;
        assert!(m
            .records
            .iter()
            .any(|(method, path, _)| *method == Method::DELETE
                && path.starts_with("/api/files/local/")));
        assert_eq!(m.name.is_none(), mode != "cleanup");
        if mode == "cleanup" {
            assert!(error.contains("cleanup is unconfirmed"));
        }
        drop(m);
        assert!(f.commands().await.is_empty());
    }
}

#[tokio::test]
async fn octoprint_nozzle_must_match_sliced_gcode_without_override() {
    {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path());
        std::fs::write(dir.path().join("mode"), "missing").unwrap();
        let raw = Printers::new(f.config()).unwrap();
        assert!(raw
            .prepare("one", &path, "test", Options::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("nozzle diameter is missing"));
        assert!(f.commands().await.is_empty());
    }
    for nozzle in [json!(0.6), Value::Null] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path());
        f.shared.machine.lock().await.profile["extruder"]["nozzleDiameter"] = nozzle.clone();
        let raw = Printers::new(f.config()).unwrap();
        let error = raw
            .prepare("one", &path, "test", Options::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("nozzle"));
        if !nozzle.is_null() {
            assert!(error.contains("0.4") && error.contains("0.6"));
        }
        assert!(f.commands().await.is_empty());
    }
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = f.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.profile["extruder"]["nozzleDiameter"] = json!(0.6);
    let error = b.start("one").await.unwrap_err().to_string();
    assert!(error.contains("0.4") && error.contains("0.6"));
    assert!(!f
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|(method, path, _)| *method == Method::POST && path == "/api/files/local"));
}

#[tokio::test]
async fn background_polling_remembers_activity_after_return_to_idle() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = Arc::new(
        Safety::new(Arc::new(Printers::new(f.config()).unwrap())).timing(
            Arc::new(dutchman_print::simulated::SystemClock::default()),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_secs(60),
        ),
    );
    b.prepare("one", &input(dir.path()), "test", Options::default())
        .await
        .unwrap();
    b.clear("test").await.unwrap();
    let before = {
        let mut m = f.shared.machine.lock().await;
        m.state = "Printing".into();
        m.records
            .iter()
            .filter(|(_, path, _)| path == "/api/files")
            .count()
    };
    b.monitor();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let seen = f
                .shared
                .machine
                .lock()
                .await
                .records
                .iter()
                .filter(|(_, path, _)| path == "/api/files")
                .count();
            if seen > before {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.shared.machine.lock().await.state = "Operational".into();
    assert_eq!(b.printer("test").await.unwrap().needs_clearing, Some(true));
    assert!(b.start("one").await.is_err());
    assert!(f.commands().await.is_empty());
}
