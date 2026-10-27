//! Handwritten protocol fixtures served only on a test-owned loopback socket.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

#[path = "helpers/slicer.rs"]
mod slicer_fixture;

use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use dutchman_print::{
    backend::{Backend, JobState, Options, PrinterState},
    config::Config,
    printers::Printers,
    prusalink::Client,
    safety::Safety,
};
use md5::{Digest as _, Md5};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
struct Recorded {
    peer: std::net::SocketAddr,
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}
struct Machine {
    state: String,
    id: u64,
    name: String,
    file: Vec<u8>,
    records: Vec<Recorded>,
    digest: bool,
    upload_code: u16,
    start_code: u16,
    lost: bool,
    bad_size: bool,
    bad_type: bool,
    wrong_id: bool,
    info: Value,
    version: Value,
    storage: Value,
    missing: bool,
    slow: bool,
    block_upload: bool,
    block_start: bool,
    hold_start: bool,
    hold_controls: bool,
    transition: Option<(String, usize)>,
    delayed: bool,
    stale: Option<Method>,
}
struct Shared {
    machine: Mutex<Machine>,
    entered: Notify,
    release: Notify,
    polled: Notify,
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
fn md5(text: &str) -> String {
    Md5::digest(text)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn digest_ok(header: Option<&str>, method: &Method, path: &str) -> bool {
    let Some(header) = header.and_then(|v| v.strip_prefix("Digest ")) else {
        return false;
    };
    let fields: std::collections::HashMap<_, _> = header
        .split(", ")
        .filter_map(|v| v.split_once('='))
        .map(|(k, v)| (k, v.trim_matches('"')))
        .collect();
    let Some(cnonce) = fields.get("cnonce") else {
        return false;
    };
    let ha1 = md5("maker:test realm:secret");
    let ha2 = md5(&format!("{method}:{path}"));
    fields.get("username") == Some(&"maker")
        && fields.get("uri") == Some(&path)
        && fields.get("qop") == Some(&"auth")
        && fields
            .get("nc")
            .is_some_and(|nc| u32::from_str_radix(nc, 16).is_ok_and(|n| n > 0))
        && fields.get("response").copied()
            == Some(
                md5(&format!(
                    "{ha1}:test-nonce:{}:{cnonce}:auth:{ha2}",
                    fields["nc"]
                ))
                .as_str(),
            )
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
async fn handle(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let bytes = axum::body::to_bytes(body, 200 << 20).await.unwrap();
    let mut m = shared.machine.lock().await;
    m.records.push(Recorded {
        peer,
        method: parts.method.clone(),
        path: path.clone(),
        headers: parts.headers.clone(),
        body: bytes.clone(),
    });
    let authenticated = if m.digest {
        digest_ok(
            parts
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            &parts.method,
            &path,
        )
    } else {
        parts
            .headers
            .get("x-api-key")
            .is_some_and(|v| v == "secret")
    };
    if !authenticated {
        return (StatusCode::UNAUTHORIZED, [("www-authenticate", "Digest realm=\"test realm\", nonce=\"test-nonce\", qop=\"auth,auth-int\", algorithm=MD5")]).into_response();
    }
    if m.stale.as_ref() == Some(&parts.method) {
        m.stale = None;
        return (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                "Digest realm=\"test realm\", nonce=\"test-nonce\", qop=\"auth\", stale=true",
            )],
        )
            .into_response();
    }
    if m.slow && path == "/api/v1/status" {
        drop(m);
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        return StatusCode::GATEWAY_TIMEOUT.into_response();
    }
    match (parts.method.as_str(), path.as_str()) {
        ("GET", "/api/v1/status") => {
            if m.hold_start && m.state == "PAUSED" { shared.polled.notify_one(); }
            if let Some((state, remaining)) = m.transition.take() {
                if remaining == 0 { m.state = state; }
                else { m.transition = Some((state, remaining - 1)); }
            }
            if m.missing { return axum::Json(json!({})).into_response(); }
            let mut value = json!({"printer":{"state":m.state,"temp_nozzle":205.0,"temp_bed":60.0}});
            if matches!(m.state.as_str(), "PRINTING" | "PAUSED" | "FINISHED" | "STOPPED" | "ATTENTION" | "ERROR") && !m.name.is_empty() {
                value["job"] = json!({"id":m.id,"progress":42,"time_remaining":120});
            }
            axum::Json(value).into_response()
        }
        ("GET", "/api/v1/job") => axum::Json(json!({"id":m.id + u64::from(m.wrong_id),"state":m.state,"file":{"name":m.name}})).into_response(),
        ("GET", "/api/v1/info") => axum::Json(m.info.clone()).into_response(),
        ("GET", "/api/version") => axum::Json(m.version.clone()).into_response(),
        ("GET", "/api/v1/storage") => axum::Json(m.storage.clone()).into_response(),
        ("PUT", _) if path.starts_with("/api/v1/files/") => {
            if m.block_upload { drop(m); shared.entered.notify_one(); shared.release.notified().await; m = shared.machine.lock().await; }
            if m.upload_code != 201 { return StatusCode::from_u16(m.upload_code).unwrap().into_response(); }
            m.name = path.rsplit('/').next().unwrap().into(); m.file = bytes.to_vec();
            StatusCode::CREATED.into_response()
        }
        ("DELETE", _) if path.starts_with("/api/v1/files/") => { m.file.clear(); StatusCode::NO_CONTENT.into_response() }
        ("GET", _) if path.starts_with("/api/v1/files/") => axum::Json(json!({"name":m.name,"size":m.file.len() + usize::from(m.bad_size),"type": if m.bad_type { "FIRMWARE" } else { "PRINT_FILE" }})).into_response(),
        ("POST", _) if path.starts_with("/api/v1/files/") => {
            if m.block_start { drop(m); shared.entered.notify_one(); shared.release.notified().await; m = shared.machine.lock().await; }
            if m.start_code != 204 { return StatusCode::from_u16(m.start_code).unwrap().into_response(); }
            m.state = if m.hold_start { "PAUSED" } else { "PRINTING" }.into(); m.id += 1;
            if m.lost { return Response::builder().status(200).body(Body::new(Broken)).unwrap(); }
            StatusCode::NO_CONTENT.into_response()
        }
        ("PUT" | "DELETE", _) if path.starts_with("/api/v1/job/") => {
            if m.hold_controls && parts.method == Method::PUT { shared.polled.notify_one(); return StatusCode::NO_CONTENT.into_response(); }
            let live = format!("/api/v1/job/{}", m.id);
            if path == format!("{live}/pause") { m.state = "PAUSED".into(); }
            else if path == format!("{live}/resume") { m.state = "PRINTING".into(); }
            else if path == live && parts.method == Method::DELETE { m.state = "STOPPED".into(); }
            else { return StatusCode::NOT_FOUND.into_response(); }
            if m.delayed {
                let next = m.state.clone();
                m.state = if next == "PRINTING" { "PAUSED" } else { "PRINTING" }.into();
                m.transition = Some((next, 2));
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
                state: "IDLE".into(),
                id: 40,
                name: String::new(),
                file: Vec::new(),
                records: Vec::new(),
                digest: false,
                upload_code: 201,
                start_code: 204,
                lost: false,
                bad_size: false,
                bad_type: false,
                wrong_id: false,
                missing: false,
                slow: false,
                block_upload: false,
                block_start: false,
                hold_start: false,
                hold_controls: false,
                delayed: false,
                stale: None,
                transition: None,
                info: json!({"serial":"synthetic-1","nozzle_diameter":0.4,"mmu":false}),
                version: json!({"firmware":"test-1"}),
                storage: json!({"storage_list":[{"path":"/test-disk","available":true,"read_only":false,"free_space":1000000}]}),
            }),
            entered: Notify::new(),
            release: Notify::new(),
            polled: Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self { shared, url, task }
    }
    fn config(&self) -> Config {
        let mut config: Config = toml::from_str("[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='prusalink'\nid='test'\nname='Test printer'\nmodel='Test model'\nprinter_profile='Machine'\nprint_profile='Draft'\nfilament_profile='PLA'\n").unwrap();
        let p = &mut config.printers[0];
        p.url = Some(self.url.clone());
        p.api_key = Some("secret".into());
        config.slicer.as_mut().unwrap().path =
            Some(env!("CARGO_BIN_EXE_dutchman-fake-slicer").into());
        config
    }
    async fn backend(&self, dir: &Path) -> Arc<Safety> {
        let input = slicer_input(dir);
        let backend = Arc::new(Safety::new(Arc::new(Printers::new(self.config()).unwrap())));
        backend
            .prepare("one", &input, "test", Options::default())
            .await
            .unwrap();
        backend
    }
    async fn count(&self, method: Method) -> usize {
        self.shared
            .machine
            .lock()
            .await
            .records
            .iter()
            .filter(|r| r.method == method && r.path.starts_with("/api/v1/files/"))
            .count()
    }
}

fn slicer_input(dir: &Path) -> PathBuf {
    slicer_fixture::input(dir, |input| {
        let folder = input.parent().unwrap();
        vec![
            "--export-gcode".to_string(),
            "--no-binary-gcode".into(),
            format!("--output={}", folder.join("sliced.gcode").display()),
            "--printer-profile=Machine".into(),
            "--print-profile=Draft".into(),
            "--material-profile=PLA".into(),
            input.to_str().unwrap().into(),
        ]
    })
}

#[tokio::test]
async fn preparation_uses_canonical_paths_and_single_value_arguments() {
    let fake = Fake::new().await;
    let dir = tempfile::Builder::new()
        .prefix("prusalink job= ")
        .tempdir()
        .unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    // Force a different canonical spelling on Linux too, without symlink
    // privileges on Windows. Spaces and '=' must remain inside one argument.
    let alias = dir.path().join("nested").join("..");
    assert_ne!(alias.join("input.3mf"), dir.path().join("input.3mf"));
    let backend = fake.backend(&alias).await;
    let job = backend.job("one").await.unwrap();
    assert_eq!(job.estimate.seconds, Some(123));
    assert_eq!(job.estimate.layers, Some(12));
    assert!(dir.path().join("started").is_file());
    assert_eq!(fake.count(Method::PUT).await, 0);
    assert_eq!(fake.count(Method::POST).await, 0);
}

#[tokio::test]
async fn every_documented_state_and_optional_telemetry() {
    let fake = Fake::new().await;
    let client = Client::new(fake.config().printers.remove(0)).unwrap();
    for (state, expected) in [
        ("IDLE", PrinterState::Idle),
        ("READY", PrinterState::Idle),
        ("FINISHED", PrinterState::Idle),
        ("STOPPED", PrinterState::Idle),
        ("PRINTING", PrinterState::Printing),
        ("BUSY", PrinterState::Busy),
        ("PAUSED", PrinterState::Paused),
        ("ATTENTION", PrinterState::Attention),
        ("ERROR", PrinterState::Error),
        ("FUTURE", PrinterState::Error),
    ] {
        fake.shared.machine.lock().await.state = state.into();
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.printer.state, expected);
        assert_eq!(snapshot.printer.detail.as_deref(), Some(state));
        assert_eq!(snapshot.printer.progress, None);
        assert_eq!(snapshot.printer.seconds_left, None);
    }
    {
        let mut m = fake.shared.machine.lock().await;
        m.state = "PRINTING".into();
        m.name = "test.gcode".into();
    }
    let p = client.snapshot().await.unwrap().printer;
    assert_eq!(p.progress, Some(0.42));
    assert_eq!(p.seconds_left, Some(120));
    assert_eq!(p.job_name.as_deref(), Some("test.gcode"));
    assert_eq!(p.nozzle_temperature, Some(205.0));
}

#[tokio::test]
async fn key_digest_wrong_credentials_and_offline_list() {
    let fake = Fake::new().await;
    assert_eq!(
        Printers::new(fake.config())
            .unwrap()
            .printers()
            .await
            .unwrap()[0]
            .state,
        PrinterState::Idle
    );
    let mut bad = fake.config();
    bad.printers[0].api_key = Some("wrong".into());
    let result = Printers::new(bad).unwrap().printers().await.unwrap();
    assert_eq!(result[0].state, PrinterState::Offline);
    assert!(result[0]
        .detail
        .as_ref()
        .unwrap()
        .contains("Authentication"));
    fake.shared.machine.lock().await.digest = true;
    let mut config = fake.config();
    config.printers[0].api_key = None;
    config.printers[0].username = Some("maker".into());
    config.printers[0].password = Some("secret".into());
    assert_eq!(
        Printers::new(config).unwrap().printers().await.unwrap()[0].state,
        PrinterState::Idle
    );
    let mut bad = fake.config();
    bad.printers[0].api_key = None;
    bad.printers[0].username = Some("maker".into());
    bad.printers[0].password = Some("wrong".into());
    assert_eq!(
        Printers::new(bad).unwrap().printers().await.unwrap()[0].state,
        PrinterState::Offline
    );
    let backend = Printers::new(fake.config()).unwrap();
    fake.task.abort();
    assert_eq!(
        backend.printers().await.unwrap()[0].state,
        PrinterState::Offline
    );
}

#[tokio::test]
async fn start_upload_headers_live_controls_and_bed_clearance() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    assert!(b
        .start("one")
        .await
        .unwrap_err()
        .to_string()
        .contains("Test printer"));
    b.clear("test").await.unwrap();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
    assert_eq!(b.start("one").await.unwrap().state, JobState::Printing);
    {
        let m = fake.shared.machine.lock().await;
        let upload = m
            .records
            .iter()
            .find(|r| r.method == Method::PUT && r.path.starts_with("/api/v1/files/"))
            .unwrap();
        assert!(upload.path.starts_with("/api/v1/files/test-disk/"));
        assert_eq!(upload.headers["Print-After-Upload"], "?0");
        assert_eq!(upload.headers["Overwrite"], "?0");
        assert_eq!(upload.headers["Content-Type"], "application/octet-stream");
        assert_eq!(
            upload.headers["Content-Length"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            upload.body.len()
        );
        assert_eq!(
            upload.body.as_ref(),
            std::fs::read(dir.path().join("sliced.gcode")).unwrap()
        );
        assert_eq!(m.records[0].path, "/api/v1/status");
    }
    assert!(b.clear("test").await.is_err());
    assert_eq!(b.pause("test").await.unwrap().state, JobState::Paused);
    assert!(b.pause("test").await.is_err());
    assert_eq!(b.resume("test").await.unwrap().state, JobState::Printing);
    assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    b.clear("test").await.unwrap();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
    let m = fake.shared.machine.lock().await;
    for action in ["/pause", "/resume", ""] {
        assert!(m
            .records
            .iter()
            .any(|r| r.path == format!("/api/v1/job/41{action}") && r.method != Method::GET));
    }
}

#[tokio::test]
async fn refuse_busy_attention_unknown_missing_and_stale_readiness() {
    for state in [
        "BUSY",
        "ATTENTION",
        "PRINTING",
        "PAUSED",
        "ERROR",
        "FUTURE",
        "FINISHED",
        "STOPPED",
    ] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        fake.shared.machine.lock().await.state = state.into();
        assert!(b.start("one").await.is_err(), "{state}");
        assert_eq!(fake.count(Method::PUT).await, 0);
        assert!(b.resume("test").await.is_err());
    }
    for slow in [false, true] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = fake.shared.machine.lock().await;
            m.slow = slow;
            m.missing = !slow;
        }
        assert!(b.start("one").await.is_err());
        assert!(b.resume("test").await.is_err());
        assert_eq!(fake.count(Method::PUT).await, 0);
    }
}

#[tokio::test]
async fn upload_conflicts_incomplete_validation_and_start_conflicts() {
    for mode in ["upload", "size", "type", "start"] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = fake.shared.machine.lock().await;
            match mode {
                "upload" => m.upload_code = 409,
                "size" => m.bad_size = true,
                "type" => m.bad_type = true,
                _ => m.start_code = 409,
            }
        }
        assert!(b.start("one").await.is_err());
        assert_eq!(fake.count(Method::PUT).await, 1);
        assert_eq!(fake.count(Method::POST).await, usize::from(mode == "start"));
        if mode == "start" {
            assert!(b.start("one").await.is_err());
            assert_eq!(fake.count(Method::POST).await, 1);
        }
    }
}

#[tokio::test]
async fn lost_start_reply_is_reconciled_and_never_retried() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    fake.shared.machine.lock().await.lost = true;
    assert!(b
        .start("one")
        .await
        .unwrap_err()
        .to_string()
        .contains("reply was lost"));
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    assert!(b.start("one").await.is_err());
    assert_eq!(fake.count(Method::POST).await, 1);
    assert!(fake
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .rev()
        .any(|r| r.path == "/api/v1/status"));
}

#[tokio::test]
async fn replacement_job_and_old_terminal_outcome_never_control_a_new_job() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    b.pause("test").await.unwrap();
    {
        let mut m = fake.shared.machine.lock().await;
        m.id = 99;
        m.state = "FINISHED".into();
    }
    assert_eq!(b.job("one").await.unwrap().state, JobState::Ended);
    fake.shared.machine.lock().await.state = "PAUSED".into();
    assert!(b.resume("test").await.is_err());
    assert!(b.cancel("one").await.is_err());
    assert!(!fake
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|r| r.path.starts_with("/api/v1/job/99")));
}

#[tokio::test]
async fn changed_bytes_identity_firmware_and_tooling_require_review() {
    for mode in ["input", "output", "serial", "firmware", "nozzle"] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        match mode {
            "input" => std::fs::write(dir.path().join("input.3mf"), b"changed").unwrap(),
            "output" => std::fs::write(dir.path().join("sliced.gcode"), b"G1 X99").unwrap(),
            "serial" => fake.shared.machine.lock().await.info["serial"] = json!("changed"),
            "firmware" => fake.shared.machine.lock().await.version["firmware"] = json!("changed"),
            _ => fake.shared.machine.lock().await.info["nozzle_diameter"] = json!(0.6),
        }
        assert!(
            b.start("one")
                .await
                .unwrap_err()
                .to_string()
                .contains(if mode == "nozzle" {
                    "nozzle"
                } else {
                    "changed"
                }),
            "{mode}"
        );
        assert_eq!(fake.count(Method::PUT).await, 0);
    }
}

#[tokio::test]
async fn stop_during_upload_or_start_cancels_the_same_live_job() {
    for upload in [false, true] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = fake.shared.machine.lock().await;
            m.block_upload = upload;
            m.block_start = !upload;
        }
        let start = {
            let b = b.clone();
            tokio::spawn(async move { b.start("one").await })
        };
        fake.shared.entered.notified().await;
        let stop = {
            let b = b.clone();
            tokio::spawn(async move { b.cancel("one").await })
        };
        tokio::task::yield_now().await;
        fake.shared.release.notify_one();
        assert_eq!(start.await.unwrap().unwrap().state, JobState::Cancelled);
        assert_eq!(stop.await.unwrap().unwrap().state, JobState::Cancelled);
        assert!(fake
            .shared
            .machine
            .lock()
            .await
            .records
            .iter()
            .any(|r| r.method == Method::DELETE && r.path == "/api/v1/job/41"));
    }
}

#[tokio::test]
async fn profile_mismatches_and_unresolved_material_slots_are_refused() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let _ = fake.backend(dir.path()).await;
    let b = Printers::new(fake.config()).unwrap();
    for options in [
        Options {
            printer_profile: Some("Other model or plate".into()),
            ..Default::default()
        },
        Options {
            print_profile: Some("Other tooling".into()),
            ..Default::default()
        },
        Options {
            filament_profile: Some("Other material".into()),
            ..Default::default()
        },
    ] {
        assert!(b
            .prepare("other", &dir.path().join("input.3mf"), "test", options)
            .await
            .is_err());
    }
    fake.shared.machine.lock().await.info["mmu"] = json!(true);
    assert!(b
        .prepare(
            "other",
            &dir.path().join("input.3mf"),
            "test",
            Options::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("slot"));
}

#[tokio::test]
async fn binary_gcode_and_unwritable_storage_never_start() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let _ = fake.backend(dir.path()).await;
    std::fs::write(dir.path().join("mode"), b"binary").unwrap();
    let b = Printers::new(fake.config()).unwrap();
    assert!(b
        .prepare(
            "binary",
            &dir.path().join("input.3mf"),
            "test",
            Options::default()
        )
        .await
        .is_err());
    for storage in [
        json!({"storage_list":[]}),
        json!({"storage_list":[{"path":"/test","available":true,"read_only":true,"free_space":1000000}]}),
        json!({"storage_list":[{"path":"/test","available":true,"read_only":false,"free_space":0}]}),
    ] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        fake.shared.machine.lock().await.storage = storage;
        assert!(b.start("one").await.is_err());
        assert_eq!(fake.count(Method::PUT).await, 0);
    }
}

#[tokio::test]
async fn terminal_outcomes_are_distinct_and_idle_does_not_mean_done() {
    for (state, expected) in [
        ("FINISHED", JobState::Done),
        ("STOPPED", JobState::Cancelled),
        ("ERROR", JobState::Failed),
        ("IDLE", JobState::Ended),
    ] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        fake.shared.machine.lock().await.state = state.into();
        assert_eq!(b.job("one").await.unwrap().state, expected);
        assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    }
}

#[tokio::test]
async fn digest_machine_commands_are_sent_once_and_authentication_stays_private() {
    let fake = Fake::new().await;
    fake.shared.machine.lock().await.digest = true;
    let dir = tempfile::tempdir().unwrap();
    let input = slicer_input(dir.path());
    let mut config = fake.config();
    config.printers[0].api_key = None;
    config.printers[0].username = Some("maker".into());
    config.printers[0].password = Some("secret".into());
    let b = Safety::new(Arc::new(Printers::new(config).unwrap()));
    b.prepare("one", &input, "test", Options::default())
        .await
        .unwrap();
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    b.pause("test").await.unwrap();
    b.resume("test").await.unwrap();
    b.cancel("one").await.unwrap();
    assert_eq!(fake.count(Method::POST).await, 1);
    assert_eq!(fake.count(Method::PUT).await, 1);
    let public = serde_json::to_string(&b.printers().await.unwrap()).unwrap();
    assert!(!public.contains("secret"));
    assert!(!public.contains(&fake.url));
}

#[test]
fn config_refuses_credential_ambiguity_redirect_targets_and_non_http_urls() {
    let config: Config = toml::from_str(
        "[backend]\nkind='simulated'\n[[printers]]\nid='test'\nname='Test'\nmodel='Test'",
    )
    .unwrap();
    let base = config.printers[0].clone();
    for url in [
        "https://127.0.0.1",
        "http://user:secret@127.0.0.1",
        "http://127.0.0.1/path",
        "http://127.0.0.1/?secret",
        "file:///private",
    ] {
        let mut p = base.clone();
        p.url = Some(url.into());
        p.api_key = Some("secret".into());
        let error = Client::new(p).err().unwrap();
        assert!(!error.contains("secret"));
    }
    for (key, user, password) in [
        (None, None, None),
        (Some("secret"), Some("maker"), Some("secret")),
        (None, Some("maker"), None),
    ] {
        let mut p = base.clone();
        p.url = Some("http://127.0.0.1".into());
        p.api_key = key.map(str::to_owned);
        p.username = user.map(str::to_owned);
        p.password = password.map(str::to_owned);
        assert!(Client::new(p).is_err());
    }
}

#[tokio::test]
async fn restart_preserves_consumed_clearance_and_uncertain_start_marker() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let _ = fake.backend(dir.path()).await;
    let jobs_path = dir.path().join("jobs.json");
    let safety_path = dir.path().join("safety.json");
    let raw = Arc::new(
        Printers::new(fake.config())
            .unwrap()
            .with_store(jobs_path.clone()),
    );
    let b = Safety::new(raw).with_store(safety_path.clone());
    b.prepare(
        "one",
        &dir.path().join("input.3mf"),
        "test",
        Options::default(),
    )
    .await
    .unwrap();
    b.clear("test").await.unwrap();
    fake.shared.machine.lock().await.lost = true;
    assert!(b.start("one").await.is_err());
    // Recreate from the files written before sending Start, without a final save.
    let raw = Arc::new(Printers::new(fake.config()).unwrap());
    raw.restore(&jobs_path).await.unwrap();
    let b = Safety::new(raw);
    b.restore(&safety_path).await.unwrap();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    assert!(b.start("one").await.is_err());
    assert_eq!(fake.count(Method::POST).await, 1);
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    for path in [&jobs_path, &safety_path] {
        assert!(!std::fs::read_to_string(path).unwrap().contains("secret"));
    }
}

#[tokio::test]
async fn changed_configured_model_and_profiles_require_another_preparation() {
    for field in ["model", "printer", "print", "material"] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let _ = fake.backend(dir.path()).await;
        let b = Printers::new(fake.config()).unwrap();
        b.prepare(
            "one",
            &dir.path().join("input.3mf"),
            "test",
            Options::default(),
        )
        .await
        .unwrap();
        let state = dir.path().join("review.json");
        b.save(&state).await.unwrap();
        let mut config = fake.config();
        match field {
            "model" => config.printers[0].model = "Different model".into(),
            "printer" => {
                config.printers[0].printer_profile =
                    Some("Different model, volume, nozzle, or plate".into())
            }
            "print" => config.printers[0].print_profile = Some("Different tooling".into()),
            _ => config.printers[0].filament_profile = Some("Different material".into()),
        }
        let b = Printers::new(config).unwrap();
        b.restore(&state).await.unwrap();
        assert!(b
            .start("one")
            .await
            .unwrap_err()
            .to_string()
            .contains("profiles changed"));
        assert_eq!(fake.count(Method::PUT).await, 0);
    }
}

#[tokio::test]
async fn contradictory_live_job_telemetry_blocks_controls() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    b.pause("test").await.unwrap();
    fake.shared.machine.lock().await.wrong_id = true;
    assert!(b.resume("test").await.is_err());
    assert!(b.cancel("one").await.is_err());
    assert_eq!(b.printers().await.unwrap()[0].state, PrinterState::Offline);
    assert!(!fake
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|r| r.path.ends_with("/resume") || r.method == Method::DELETE));
}

#[tokio::test]
async fn a_pending_stop_survives_failed_reconciliation_until_status_returns() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    fake.shared.machine.lock().await.wrong_id = true;
    assert!(b.cancel("one").await.is_err());
    assert!(!fake
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|r| r.method == Method::DELETE));
    fake.shared.machine.lock().await.wrong_id = false;
    assert_eq!(b.job("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(fake.count(Method::POST).await, 1);
    assert!(fake
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|r| r.method == Method::DELETE && r.path == "/api/v1/job/41"));
}

#[tokio::test]
async fn a_new_external_terminal_report_requires_clearance_once() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    {
        let mut m = fake.shared.machine.lock().await;
        m.state = "FINISHED".into();
        m.name = "external.gcode".into();
        m.id = 80;
    }
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    b.clear("test").await.unwrap();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
    fake.shared.machine.lock().await.id = 81;
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
}

#[allow(dead_code, clippy::panic)]
#[path = "helpers/bambu.rs"]
mod bambu_fake;

#[tokio::test]
async fn mixed_fleet_prepares_with_each_printers_slicer() {
    let prusa = Fake::new().await;
    let bambu = bambu_fake::Fake::new().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mini_dir = root.join("mini");
    let a1_dir = root.join("a1");
    std::fs::create_dir(&mini_dir).unwrap();
    std::fs::create_dir(&a1_dir).unwrap();
    let mini_input = slicer_input(&mini_dir);
    let mut config = prusa.config();
    config
        .slicers
        .insert("prusa".into(), config.slicer.take().unwrap());
    let mut a1 = config.printers[0].clone();
    a1.id = "a1".into();
    a1.kind = Some("bambu".into());
    a1.url = None;
    a1.api_key = None;
    a1.host = Some("127.0.0.1".into());
    a1.port = Some(bambu.port);
    a1.serial = Some("TESTSERIAL".into());
    a1.access_code = Some("test-access".into());
    a1.cert_sha256 = Some(dutchman_print::safety::hash(bambu.cert.as_ref()));
    config.printers.push(a1);
    let mut orca = config.slicers["prusa"].clone();
    orca.kind = "orcaslicer".into();
    orca.datadir = Some(a1_dir.join("presets"));
    config.slicers.insert("orca".into(), orca);
    for (category, name) in [
        ("machine", "Machine"),
        ("process", "Draft"),
        ("filament", "PLA"),
    ] {
        let dir = a1_dir.join("presets/user/default").join(category);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("preset.json"),
            serde_json::json!({"name": name}).to_string(),
        )
        .unwrap();
    }
    let a1_input = slicer_fixture::input(&a1_dir, |input| {
        vec![
            format!("--datadir={}", a1_dir.join("presets").display()),
            format!(
                "--load-filaments={}",
                a1_dir
                    .join("presets/user/default/filament/preset.json")
                    .display()
            ),
            format!(
                "--load-settings={};{}",
                a1_dir
                    .join("presets/user/default/machine/preset.json")
                    .display(),
                a1_dir
                    .join("presets/user/default/process/preset.json")
                    .display()
            ),
            "--slice=0".into(),
            "--export-3mf=sliced.3mf".into(),
            format!("--outputdir={}", a1_dir.display()),
            input.display().to_string(),
        ]
    });
    let orca = config.slicers.remove("orca").unwrap();
    assert!(Printers::new({
        // Rebuild below from the same mixed settings after each resolution refusal.
        let mut bad: Config = toml::from_str("[backend]\nkind='printers'\n").unwrap();
        bad.printers = config.printers.clone();
        bad.slicers = config.slicers.clone();
        bad
    })
    .err()
    .unwrap()
    .contains("needs a slicer of kind orcaslicer"));
    config.slicers.insert("orca".into(), orca.clone());
    config.slicers.insert("other-orca".into(), orca);
    let mut bad: Config = toml::from_str("[backend]\nkind='printers'\n").unwrap();
    bad.printers = config.printers.clone();
    bad.slicers = config.slicers.clone();
    assert!(Printers::new(bad).err().unwrap().contains("ambiguous"));
    config.printers[1].slicer = Some("orca".into());
    let backend = Printers::new(config).unwrap();
    assert_eq!(
        backend
            .prepare("mini", &mini_input, "test", Options::default())
            .await
            .unwrap()
            .seconds,
        Some(123)
    );
    assert_eq!(
        backend
            .prepare("a1", &a1_input, "a1", Options::default())
            .await
            .unwrap()
            .seconds,
        Some(123)
    );
    assert!(mini_dir.join("sliced.gcode").is_file());
    assert!(a1_dir.join("sliced.3mf").is_file());
}

#[derive(Default)]
struct ClearanceClock(std::sync::atomic::AtomicU64);
impl dutchman_print::simulated::Clock for ClearanceClock {
    fn now(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.0.load(std::sync::atomic::Ordering::SeqCst))
    }
}
#[tokio::test]
async fn clearance_expires_and_failed_late_or_changed_reads_withdraw_it() {
    for mode in ["failure", "gap", "expiry", "identity", "activity"] {
        let fake = Fake::new().await;
        let clock = Arc::new(ClearanceClock::default());
        let b = Safety::new(Arc::new(Printers::new(fake.config()).unwrap())).timing(
            clock.clone(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(60),
        );
        b.clear("test").await.unwrap();
        match mode {
            "failure" => {
                fake.shared.machine.lock().await.missing = true;
                assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
                fake.shared.machine.lock().await.missing = false;
            }
            "gap" => clock.0.store(11, std::sync::atomic::Ordering::SeqCst),
            "expiry" => {
                for second in (5..=60).step_by(5) {
                    clock.0.store(second, std::sync::atomic::Ordering::SeqCst);
                    let p = b.printers().await.unwrap();
                    assert_eq!(p[0].needs_clearing, Some(second == 60));
                }
            }
            "identity" => {
                let mut m = fake.shared.machine.lock().await;
                m.state = "FINISHED".into();
                m.name = "external.gcode".into();
                m.id = 99;
            }
            _ => {
                fake.shared.machine.lock().await.state = "BUSY".into();
                b.printers().await.unwrap();
                fake.shared.machine.lock().await.state = "IDLE".into();
            }
        }
        assert_eq!(
            b.printers().await.unwrap()[0].needs_clearing,
            Some(true),
            "{mode}"
        );
    }
}
#[tokio::test]
async fn monitor_sees_activity_between_client_reads_and_stops_without_clearance() {
    let fake = Fake::new().await;
    let b = Arc::new(
        Safety::new(Arc::new(Printers::new(fake.config()).unwrap())).timing(
            Arc::new(dutchman_print::simulated::SystemClock::default()),
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(60),
        ),
    );
    b.monitor();
    b.clear("test").await.unwrap();
    fake.shared.machine.lock().await.state = "BUSY".into();
    tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
    fake.shared.machine.lock().await.state = "IDLE".into();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    let reads = fake.shared.machine.lock().await.records.len();
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    assert_eq!(fake.shared.machine.lock().await.records.len(), reads);
}
#[tokio::test]
async fn ready_substitutes_for_remote_clearance_but_idle_does_not() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    assert!(b.start("one").await.is_err());
    fake.shared.machine.lock().await.state = "READY".into();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
    assert_eq!(b.start("one").await.unwrap().state, JobState::Printing);
}
#[tokio::test]
async fn installed_nozzle_must_match_sliced_profile_before_upload() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = slicer_input(dir.path());
    fake.shared.machine.lock().await.info["nozzle_diameter"] = json!(0.6);
    let b = Printers::new(fake.config()).unwrap();
    let error = b
        .prepare("one", &input, "test", Options::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("0.6 mm"));
    assert!(error.contains("0.4 mm"));
    assert_eq!(fake.count(Method::PUT).await, 0);
}
#[tokio::test]
async fn server_restart_reconciles_tracked_prusalink_job_before_cancel() {
    use tower::ServiceExt as _;
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = slicer_input(dir.path());
    let raw = Arc::new(
        Printers::new(fake.config())
            .unwrap()
            .with_store(dir.path().join("jobs.json")),
    );
    let safety = Safety::new(raw);
    safety
        .prepare("one", &input, "test", Options::default())
        .await
        .unwrap();
    safety.clear("test").await.unwrap();
    safety.start("one").await.unwrap();
    safety.save(&dir.path().join("safety.json")).await.unwrap();
    // Only the pre-Start record is needed: startup recovers the live id and state.
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.path().into();
    let app = dutchman_print::router(
        Arc::new(
            Printers::new(fake.config())
                .unwrap()
                .with_store(dir.path().join("jobs.json")),
        ),
        settings,
    );
    let request = axum::http::Request::post("/api/v1/confirmations")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"action":"cancel","printer":"test","job":"one"}).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let challenge: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(challenge["observed"]["job_id"], "41");
    let request = axum::http::Request::post("/api/v1/jobs/one/cancel")
        .header("Plugin-Confirmation", "a".repeat(64))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"confirmation":challenge["id"]}).to_string(),
        ))
        .unwrap();
    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    assert_eq!(fake.shared.machine.lock().await.state, "STOPPED");
}

#[tokio::test]
async fn vanished_execution_ends_and_terminal_outcomes_stay_terminal() {
    for terminal in [None, Some("FINISHED"), Some("STOPPED")] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        if let Some(state) = terminal {
            fake.shared.machine.lock().await.state = state.into();
            b.printers().await.unwrap();
        }
        fake.shared.machine.lock().await.state = "IDLE".into();
        let expected = match terminal {
            Some("FINISHED") => JobState::Done,
            Some("STOPPED") => JobState::Cancelled,
            _ => JobState::Ended,
        };
        assert_eq!(b.job("one").await.unwrap().state, expected);
        fake.shared.machine.lock().await.state = "BUSY".into();
        assert_eq!(b.job("one").await.unwrap().state, expected);
    }
}

#[tokio::test]
async fn attention_and_busy_refuse_movement_and_attention_allows_cancel() {
    for (state, expected) in [
        ("ATTENTION", PrinterState::Attention),
        ("BUSY", PrinterState::Busy),
    ] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        b.pause("test").await.unwrap();
        fake.shared.machine.lock().await.state = state.into();
        assert_eq!(b.printer("test").await.unwrap().state, expected);
        assert!(b.pause("test").await.is_err());
        assert!(b.resume("test").await.is_err());
        assert!(b.clear("test").await.is_err());
        assert!(b.start("one").await.is_err());
        assert_eq!(b.job("one").await.unwrap().state, JobState::Paused);
        if state == "ATTENTION" {
            assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
        }
    }
}

#[tokio::test]
async fn delayed_commands_poll_without_resending() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    fake.shared.machine.lock().await.delayed = true;
    assert_eq!(b.pause("test").await.unwrap().state, JobState::Paused);
    assert_eq!(b.resume("test").await.unwrap().state, JobState::Printing);
    assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
    let m = fake.shared.machine.lock().await;
    for suffix in ["/pause", "/resume", "/41"] {
        assert_eq!(
            m.records
                .iter()
                .filter(|r| r.method != Method::GET && r.path.ends_with(suffix))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn digest_reuses_connection_and_challenge_and_increments_nonce_count() {
    let fake = Fake::new().await;
    fake.shared.machine.lock().await.digest = true;
    let mut config = fake.config().printers.remove(0);
    config.api_key = None;
    config.username = Some("maker".into());
    config.password = Some("secret".into());
    let client = Client::new(config).unwrap();
    for _ in 0..4 {
        client.snapshot().await.unwrap();
    }
    let m = fake.shared.machine.lock().await;
    assert_eq!(
        m.records
            .iter()
            .map(|r| r.peer)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        m.records
            .iter()
            .filter(|r| !r.headers.contains_key("authorization"))
            .count(),
        1
    );
    let counts: Vec<_> = m
        .records
        .iter()
        .filter_map(|r| r.headers.get("authorization"))
        .map(|v| {
            v.to_str()
                .unwrap()
                .split(", ")
                .find(|s| s.starts_with("nc="))
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        counts,
        ["nc=00000001", "nc=00000002", "nc=00000003", "nc=00000004"]
    );
}

#[tokio::test]
async fn failed_upload_and_validation_delete_without_start() {
    for mode in ["rejected", "size", "type"] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = fake.backend(dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = fake.shared.machine.lock().await;
            match mode {
                "rejected" => m.upload_code = 500,
                "size" => m.bad_size = true,
                _ => m.bad_type = true,
            }
        }
        assert!(b.start("one").await.is_err());
        assert_eq!(fake.count(Method::DELETE).await, 1);
        assert_eq!(fake.count(Method::POST).await, 0);
    }
}

#[tokio::test]
async fn stale_digest_retries_only_reads() {
    let fake = Fake::new().await;
    fake.shared.machine.lock().await.digest = true;
    let mut config = fake.config().printers.remove(0);
    config.api_key = None;
    config.username = Some("maker".into());
    config.password = Some("secret".into());
    let client = Client::new(config).unwrap();
    client.snapshot().await.unwrap();
    fake.shared.machine.lock().await.stale = Some(Method::GET);
    client.snapshot().await.unwrap();
    fake.shared.machine.lock().await.stale = Some(Method::PUT);
    assert!(client
        .request(Method::PUT, "/api/v1/job/40/pause", Bytes::new(), false)
        .await
        .is_err());
    let m = fake.shared.machine.lock().await;
    assert_eq!(
        m.records.iter().filter(|r| r.method == Method::PUT).count(),
        1
    );
    assert_eq!(
        m.records.iter().filter(|r| r.method == Method::GET).count(),
        4
    );
}

#[tokio::test]
async fn stalled_upload_is_deleted_and_never_started() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = fake.backend(dir.path()).await;
    b.clear("test").await.unwrap();
    fake.shared.machine.lock().await.block_upload = true;
    let result = tokio::time::timeout(std::time::Duration::from_secs(40), b.start("one"))
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(fake.count(Method::DELETE).await, 1);
    assert_eq!(fake.count(Method::POST).await, 0);
    fake.shared.release.notify_one();
}

#[tokio::test]
async fn status_remains_bounded_while_an_upload_holds_the_job_lock() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = slicer_input(dir.path());
    let backend = Arc::new(Printers::new(fake.config()).unwrap());
    backend
        .prepare("one", &input, "test", Options::default())
        .await
        .unwrap();
    fake.shared.machine.lock().await.block_upload = true;
    let start = {
        let backend = backend.clone();
        tokio::spawn(async move { backend.start("one").await })
    };
    fake.shared.entered.notified().await;
    let reads = async { tokio::join!(backend.printer("test"), backend.printers()) };
    let (single, fleet) = tokio::time::timeout(std::time::Duration::from_secs(6), reads)
        .await
        .unwrap();
    assert_eq!(single.unwrap().state, PrinterState::Offline);
    assert_eq!(fleet.unwrap()[0].state, PrinterState::Offline);
    fake.shared.release.notify_one();
    assert_eq!(start.await.unwrap().unwrap().state, JobState::Printing);
}

#[tokio::test]
async fn independent_printers_and_stop_during_start_poll() {
    use std::time::Duration;
    use tokio::time::timeout;
    for pending in [false, true] {
        let a = Fake::new().await;
        let b = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let path = slicer_input(dir.path());
        let mut config = a.config();
        let mut other = b.config().printers.remove(0);
        other.id = "other".into();
        config.printers.push(other);
        let backend = Arc::new(Safety::new(Arc::new(Printers::new(config).unwrap())));
        backend
            .prepare("one", &path, "test", Options::default())
            .await
            .unwrap();
        backend
            .prepare("two", &path, "other", Options::default())
            .await
            .unwrap();
        backend.clear("other").await.unwrap();
        backend.start("two").await.unwrap();
        backend.clear("test").await.unwrap();
        {
            let mut m = a.shared.machine.lock().await;
            if pending {
                m.hold_start = true;
            } else {
                m.block_upload = true;
            }
        }
        let start = tokio::spawn({
            let backend = backend.clone();
            async move { backend.start("one").await }
        });
        timeout(Duration::from_secs(5), async {
            if pending {
                a.shared.polled.notified().await;
            } else {
                a.shared.entered.notified().await;
            }
        })
        .await
        .unwrap();
        assert!(!start.is_finished());
        timeout(Duration::from_secs(2), async {
            assert_eq!(
                backend.printer("other").await.unwrap().state,
                PrinterState::Printing
            );
            assert_eq!(backend.job("two").await.unwrap().state, JobState::Printing);
            backend
                .review_action("cancel", "other", Some("two"))
                .await
                .unwrap();
            assert_eq!(
                backend.cancel("two").await.unwrap().state,
                JobState::Cancelled
            );
        })
        .await
        .unwrap();
        if pending {
            assert_eq!(
                timeout(Duration::from_secs(2), backend.cancel("one"))
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                JobState::Cancelled
            );
            assert_eq!(
                timeout(Duration::from_secs(2), start)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .state,
                JobState::Cancelled
            );
            assert_eq!(backend.job("one").await.unwrap().state, JobState::Cancelled);
        } else {
            a.shared.release.notify_one();
            assert_eq!(
                timeout(Duration::from_secs(5), start)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .state,
                JobState::Printing
            );
        }
    }
}

#[tokio::test]
async fn stop_overtakes_pause_and_resume_polls() {
    use std::time::Duration;
    use tokio::time::timeout;
    for resume in [false, true] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let backend = f.backend(dir.path()).await;
        backend.clear("test").await.unwrap();
        backend.start("one").await.unwrap();
        if resume {
            backend.pause("test").await.unwrap();
        }
        f.shared.machine.lock().await.hold_controls = true;
        let command = tokio::spawn({
            let backend = backend.clone();
            async move {
                if resume {
                    backend.resume("test").await
                } else {
                    backend.pause("test").await
                }
            }
        });
        timeout(Duration::from_secs(5), f.shared.polled.notified())
            .await
            .unwrap();
        assert!(!command.is_finished());
        assert_eq!(
            timeout(Duration::from_secs(2), backend.cancel("one"))
                .await
                .unwrap()
                .unwrap()
                .state,
            JobState::Cancelled
        );
        assert_eq!(
            timeout(Duration::from_secs(2), command)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .state,
            JobState::Cancelled
        );
    }
}
