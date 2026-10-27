//! DSF REST and shared Duet safety exercised against test-owned loopback sockets.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "helpers/controls.rs"]
mod controls;
#[path = "helpers/duet_sbc.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use dutchman_print::{
    backend::{Backend, Options, PrinterState},
    duet::Client,
    printers::Printers,
    safety::Safety,
};
use fake::Fake;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tower::ServiceExt as _;

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
fn staged() -> String {
    format!(
        "0:/gcodes/dutchman-{}.gcode",
        dutchman_print::safety::hash(b"one")
    )
}
fn start_command() -> String {
    format!("M32 \"{}\"", staged())
}
struct Api {
    app: Router,
    token: AtomicU64,
    dir: tempfile::TempDir,
}
impl Api {
    async fn new(f: &Fake) -> Self {
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
        Self {
            app: dutchman_print::router(raw, settings),
            token: AtomicU64::new(1),
            dir,
        }
    }
    async fn request(
        &self,
        method: &str,
        route: &str,
        body: Value,
        header: Option<String>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(route)
            .header("Content-Type", "application/json");
        if let Some(header) = header {
            request = request.header("Plugin-Confirmation", header);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
        )
    }
    async fn challenge(&self, action: &str) -> (StatusCode, Value) {
        self.request("POST", "/api/v1/confirmations", json!({"action":action,"printer":"test","job":if matches!(action,"start"|"cancel"){Some("one")}else{None}}), None).await
    }
    async fn consume(&self, action: &str, challenge: &Value) -> (StatusCode, Value) {
        let route = if matches!(action, "start" | "cancel") {
            format!("/api/v1/jobs/one/{action}")
        } else {
            format!("/api/v1/printers/test/{action}")
        };
        self.request(
            "POST",
            &route,
            json!({"confirmation":challenge["id"]}),
            Some(format!(
                "{:064x}",
                self.token.fetch_add(1, Ordering::Relaxed)
            )),
        )
        .await
    }
    async fn action(&self, action: &str) -> (StatusCode, Value) {
        let (status, challenge) = self.challenge(action).await;
        if status != StatusCode::CREATED {
            return (status, challenge);
        }
        self.consume(action, &challenge).await
    }
    async fn ok(&self, action: &str) -> Value {
        let (status, value) = self.action(action).await;
        assert_eq!(
            status,
            if action == "clear" {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::OK
            },
            "{action}: {value}"
        );
        value
    }
    async fn job(&self) -> Value {
        let (status, value) = self
            .request("GET", "/api/v1/jobs/one", Value::Null, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        value
    }
}
async fn file_changes(f: &Fake) -> usize {
    f.shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .filter(|(path, _)| path.starts_with("/machine/file/"))
        .count()
}

#[tokio::test]
async fn password_header_expiry_status_and_disconnect() {
    let f = Fake::new().await;
    let c = Client::new(f.config().printers.remove(0)).unwrap();
    let p = c.snapshot().await.unwrap().printer;
    assert_eq!(p.state, PrinterState::Idle);
    assert_eq!(p.nozzle_temperature, Some(201.5));
    assert_eq!(p.bed_temperature, Some(59.0));
    assert!(p.progress.is_none());
    c.snapshot().await.unwrap();
    assert_eq!(f.shared.machine.lock().await.connects, 1);
    f.shared.machine.lock().await.keys.clear();
    c.snapshot().await.unwrap();
    assert_eq!(f.shared.machine.lock().await.connects, 2);
    drop(c);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if f.shared
                .machine
                .lock()
                .await
                .records
                .iter()
                .any(|(p, _)| p == "/machine/disconnect")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.shared.machine.lock().await.password = Some("different".into());
    let c = Client::new(f.config().printers.remove(0)).unwrap();
    assert!(matches!(
        c.snapshot().await,
        Err(dutchman_print::backend::Error::Authentication)
    ));
    let p = Printers::new(f.config()).unwrap().printers().await.unwrap();
    let rendered = serde_json::to_string(&p).unwrap();
    assert!(!rendered.contains("secret") && !rendered.contains(&f.url));
    assert_eq!(p[0].state, PrinterState::Offline);
    // No configured password sends no password parameter, and still uses the returned key.
    f.shared.machine.lock().await.password = None;
    let mut config = f.config().printers.remove(0);
    config.password = None;
    assert_eq!(
        Client::new(config)
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .printer
            .state,
        PrinterState::Idle
    );
    f.shared.machine.lock().await.password = Some("p&ss w=rd%#".into());
    let mut config = f.config().printers.remove(0);
    config.password = Some("p&ss w=rd%#".into());
    assert_eq!(
        Client::new(config)
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .printer
            .state,
        PrinterState::Idle
    );
}

#[tokio::test]
async fn challenge_controls_start_once_pause_resume_cancel_and_cleanup() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    for action in ["start", "pause", "resume", "cancel", "stop", "clear"] {
        let route = if matches!(action, "start" | "cancel") {
            format!("/api/v1/jobs/one/{action}")
        } else {
            format!("/api/v1/printers/test/{action}")
        };
        assert_eq!(
            api.request("POST", &route, json!({}), None).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    assert!(f.commands().await.is_empty());
    api.ok("clear").await;
    let (status, challenge) = api.challenge("start").await;
    assert_eq!(status, StatusCode::CREATED);
    f.shared.machine.lock().await.delay_reads = 3;
    assert_eq!(api.consume("start", &challenge).await.0, StatusCode::OK);
    assert_eq!(
        api.consume("start", &challenge).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    f.shared.machine.lock().await.delay_reads = 3;
    assert_eq!(api.ok("pause").await["state"], "paused");
    f.shared.machine.lock().await.delay_reads = 3;
    assert_eq!(api.ok("resume").await["state"], "printing");
    f.shared.machine.lock().await.delay_reads = 3;
    assert_eq!(api.ok("cancel").await["state"], "cancelled");
    assert_eq!(
        f.commands().await,
        vec![
            start_command(),
            "M25".into(),
            "M24".into(),
            "M25".into(),
            "M0".into()
        ]
    );
    assert!(f.shared.machine.lock().await.files.is_empty());
    assert_eq!(api.job().await["state"], "cancelled");
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    let m = f.shared.machine.lock().await;
    let upload = m
        .records
        .iter()
        .position(|(p, _)| p.starts_with("/machine/file/"))
        .unwrap();
    assert!(m.records[..upload]
        .iter()
        .any(|(p, _)| p == "/machine/model"));
    assert_eq!(
        m.records
            .iter()
            .filter(|(p, _)| p.starts_with("/machine/file/"))
            .count(),
        2
    );
}

#[tokio::test]
async fn changed_review_and_unknown_readiness_block_start_before_upload() {
    for mode in [
        "busy",
        "starting",
        "simulating",
        "updating",
        "halted",
        "off",
        "disconnected",
        "future",
        "missing",
        "unavailable",
        "firmware",
        "identity",
        "volume",
        "slots",
        "tools",
        "input",
        "output",
        "existing",
    ] {
        let f = Fake::new().await;
        let api = Api::new(&f).await;
        api.ok("clear").await;
        let (status, challenge) = api.challenge("start").await;
        assert_eq!(status, StatusCode::CREATED);
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "firmware" => m.firmware = "changed".into(),
                "identity" => m.board_name = "changed".into(),
                "volume" => m.axes[0]["max"] = json!(180),
                "slots" => m.tools = json!([{"number":0,"heaters":[1],"extruders":[0,1]}]),
                "tools" => m.tools = json!([{"number":1,"heaters":[0],"extruders":[1]}]),
                "input" => std::fs::write(api.dir.path().join("input.3mf"), b"changed").unwrap(),
                "output" => std::fs::write(api.dir.path().join("sliced.gcode"), b"G28\n").unwrap(),
                "existing" => {
                    m.files.insert(
                        staged().strip_prefix("0:/gcodes/").unwrap().into(),
                        b"other".to_vec(),
                    );
                }
                "unavailable" => m.block_model = true,
                "missing" => m.status = String::new(),
                _ => m.status = mode.into(),
            }
        }
        assert_ne!(
            api.consume("start", &challenge).await.0,
            StatusCode::OK,
            "{mode}"
        );
        assert!(f.commands().await.is_empty(), "{mode}");
        assert_eq!(file_changes(&f).await, 0, "{mode}");
    }
}

#[tokio::test]
async fn failed_lost_or_invalid_uploads_cleanup_and_never_start() {
    for mode in ["failed", "lost", "size", "info", "cleanup"] {
        let f = Fake::new().await;
        let api = Api::new(&f).await;
        api.ok("clear").await;
        {
            let mut m = f.shared.machine.lock().await;
            m.files.insert("unrelated.gcode".into(), b"G1".to_vec());
            match mode {
                "failed" => m.upload_err = true,
                "lost" => m.upload_lost = true,
                "size" => m.bad_size = true,
                "info" => m.bad_info = true,
                _ => {
                    m.upload_err = true;
                    m.delete_err = true;
                }
            }
        }
        let (status, value) = api.action("start").await;
        assert_ne!(status, StatusCode::OK, "{mode}");
        assert!(f.commands().await.is_empty(), "{mode}");
        let m = f.shared.machine.lock().await;
        assert!(m.files.contains_key("unrelated.gcode"));
        if mode == "cleanup" {
            assert!(value.to_string().contains("cleanup"));
            assert_eq!(m.files.len(), 2);
        } else {
            assert_eq!(m.files.len(), 1, "{mode}");
        }
        drop(m);
        assert_eq!(file_changes(&f).await, 2, "{mode}");
    }
}

#[tokio::test]
async fn lost_start_reply_is_reconciled_without_resending() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    api.ok("clear").await;
    f.shared.machine.lock().await.lost = true;
    let (status, value) = api.action("start").await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert!(value.to_string().contains("reply was lost"));
    assert_eq!(api.job().await["state"], "printing");
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    assert_eq!(f.commands().await, vec![start_command()]);
    api.ok("cancel").await;
}

#[tokio::test]
async fn expired_command_is_never_resent_and_unknown_resume_is_refused() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    api.ok("clear").await;
    api.ok("start").await;
    f.shared.machine.lock().await.expire_on_gcode = true;
    assert_eq!(api.action("pause").await.0, StatusCode::CONFLICT);
    assert_eq!(f.commands().await, vec![start_command()]);
    api.ok("pause").await;
    let (_, challenge) = api.challenge("resume").await;
    f.shared.machine.lock().await.status = "future".into();
    assert_eq!(
        api.consume("resume", &challenge).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(f.commands().await, vec![start_command(), "M25".into()]);
}

#[tokio::test]
async fn external_stop_and_changed_identity_leave_external_files() {
    for paused in [false, true] {
        let f = Fake::new().await;
        let api = Api::new(&f).await;
        f.external(if paused { "paused" } else { "processing" })
            .await;
        assert_eq!(api.action("pause").await.0, StatusCode::CONFLICT);
        let (status, challenge) = api.challenge("stop").await;
        assert_eq!(status, StatusCode::CREATED);
        f.shared.machine.lock().await.file = Some("0:/gcodes/other.gcode".into());
        assert_eq!(
            api.consume("stop", &challenge).await.0,
            StatusCode::CONFLICT
        );
        assert!(f.commands().await.is_empty());
        f.shared.machine.lock().await.file = Some("0:/gcodes/external.gcode".into());
        assert_eq!(api.ok("stop").await["state"], "idle");
        assert_eq!(
            f.commands().await,
            if paused {
                vec!["M0".to_string()]
            } else {
                vec!["M25".into(), "M0".into()]
            }
        );
        assert!(f
            .shared
            .machine
            .lock()
            .await
            .files
            .contains_key("external.gcode"));
        assert_eq!(file_changes(&f).await, 0);
    }
}

#[tokio::test]
async fn unconfirmed_pause_never_sends_m0_for_cancel_or_external_stop() {
    for external in [false, true] {
        let f = Fake::new().await;
        let api = Api::new(&f).await;
        if external {
            f.external("processing").await;
        } else {
            api.ok("clear").await;
            api.ok("start").await;
        }
        f.shared.machine.lock().await.hold_controls = true;
        let (status, value) = api.action(if external { "stop" } else { "cancel" }).await;
        assert_eq!(status, StatusCode::CONFLICT, "{value}");
        let commands = f.commands().await;
        assert_eq!(commands.iter().filter(|c| c.as_str() == "M25").count(), 1);
        assert!(!commands.iter().any(|c| c == "M0"));
    }
}

#[tokio::test]
async fn terminal_outcome_and_clearance_are_distinct_from_success() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    api.ok("clear").await;
    api.ok("start").await;
    {
        let mut m = f.shared.machine.lock().await;
        m.status = "idle".into();
        m.last_file = m.file.take();
    }
    assert_eq!(api.job().await["state"], "ended");
    assert!(f.shared.machine.lock().await.files.is_empty());
    let (_, printers) = api
        .request("GET", "/api/v1/printers", Value::Null, None)
        .await;
    assert_eq!(printers[0]["needs_clearing"], true);
    api.ok("clear").await;
}

#[tokio::test]
async fn nozzle_missing_or_mismatch_refuses_preparation_without_commands() {
    for nozzle in [None, Some(0.6)] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let mut config = f.config();
        config.printers[0].nozzle_diameter = nozzle;
        let raw = Printers::new(config).unwrap();
        let error = raw
            .prepare("one", &input(dir.path()), "test", Options::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("nozzle"), "{error}");
        if nozzle.is_some() {
            assert!(error.contains("0.4") && error.contains("0.6"));
        }
        assert!(f.commands().await.is_empty());
        assert_eq!(file_changes(&f).await, 0);
    }
}

#[tokio::test]
async fn cancel_during_upload_cancels_the_same_eventual_start() {
    let f = Fake::new().await;
    let api = Arc::new(Api::new(&f).await);
    api.ok("clear").await;
    f.shared.machine.lock().await.block = Some(format!("/machine/file/{}", staged()));
    let start = tokio::spawn({
        let api = api.clone();
        async move { api.action("start").await }
    });
    tokio::time::timeout(Duration::from_secs(3), f.shared.entered.notified())
        .await
        .unwrap();
    let cancel = tokio::spawn({
        let api = api.clone();
        async move { api.action("cancel").await }
    });
    // Consume the cancellation challenge before unblocking the upload. The backend records
    // the pending cancellation before waiting for the machine command mutex.
    tokio::time::timeout(Duration::from_secs(3), async {
        let path = api.dir.path().join("api/confirmations.json");
        loop {
            let bytes = tokio::fs::read(&path).await.unwrap();
            let ledger: Value = serde_json::from_slice(&bytes).unwrap();
            if ledger["challenges"]
                .as_object()
                .unwrap()
                .values()
                .any(|v| v["challenge"]["action"] == "cancel" && v["used"] == true)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Let cancellation reach the command boundary without relying on an elapsed sleep.
    tokio::task::yield_now().await;
    f.shared.release.notify_one();
    let (status, value) = tokio::time::timeout(Duration::from_secs(10), cancel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["state"], "cancelled");
    let _ = tokio::time::timeout(Duration::from_secs(10), start)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(api.job().await["state"], "cancelled");
    assert_eq!(
        f.commands().await,
        vec![start_command(), "M25".into(), "M0".into()]
    );
    assert!(f.shared.machine.lock().await.files.is_empty());
}

#[tokio::test]
async fn nonempty_code_reply_is_private_and_never_retried() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    api.ok("clear").await;
    f.shared.machine.lock().await.code_reply = "private backend text".into();
    let (status, value) = api.action("start").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!value.to_string().contains("private backend text"));
    assert_eq!(api.job().await["state"], "printing");
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    assert_eq!(f.commands().await, vec![start_command()]);
}

#[tokio::test]
async fn shared_model_mapping_keeps_progress_and_tool_heater_identity() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    for (status, expected) in [
        ("idle", "idle"),
        ("processing", "printing"),
        ("pausing", "printing"),
        ("resuming", "printing"),
        ("cancelling", "printing"),
        ("changingTool", "printing"),
        ("paused", "paused"),
        ("busy", "busy"),
        ("starting", "busy"),
        ("updating", "busy"),
        ("simulating", "busy"),
        ("halted", "error"),
        ("off", "offline"),
        ("disconnected", "offline"),
        ("future", "error"),
    ] {
        f.shared.machine.lock().await.status = status.into();
        let (_, printers) = api
            .request("GET", "/api/v1/printers", Value::Null, None)
            .await;
        assert_eq!(printers[0]["state"], expected, "{status}");
    }
    f.external("processing").await;
    let (_, printers) = api
        .request("GET", "/api/v1/printers", Value::Null, None)
        .await;
    assert_eq!(printers[0]["job_id"], "0:/gcodes/external.gcode");
    assert_eq!(printers[0]["progress"], 0.25);
    assert_eq!(printers[0]["seconds_left"], 321);
    {
        let mut m = f.shared.machine.lock().await;
        m.current_tool = 1;
        m.times_left = Value::Null;
        m.tools = json!([{"number":0,"heaters":[0],"extruders":[0]},{"number":1,"heaters":[1],"extruders":[1]}]);
    }
    let (_, printers) = api
        .request("GET", "/api/v1/printers", Value::Null, None)
        .await;
    assert_eq!(printers[0]["nozzle_temperature"], 201.5);
    assert!(printers[0].get("seconds_left").is_none());
}

#[tokio::test]
async fn earlier_terminal_file_cannot_finish_an_unobserved_start() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    f.shared.machine.lock().await.last_file = Some("0:/gcodes/previous.gcode".into());
    api.ok("clear").await;
    {
        let mut m = f.shared.machine.lock().await;
        m.suppress_start = true;
        m.lost = true;
    }
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    assert_eq!(api.job().await["state"], "ready");
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    assert_eq!(f.commands().await, vec![start_command()]);
    assert_eq!(f.shared.machine.lock().await.files.len(), 1);
}

#[tokio::test]
async fn changed_last_file_withdraws_bed_clearance_even_after_idle() {
    let f = Fake::new().await;
    let api = Api::new(&f).await;
    api.ok("clear").await;
    f.shared.machine.lock().await.last_file = Some("0:/gcodes/external.gcode".into());
    assert_eq!(api.action("start").await.0, StatusCode::CONFLICT);
    assert!(f.commands().await.is_empty());
    assert_eq!(file_changes(&f).await, 0);
    api.ok("clear").await;
    api.ok("start").await;
}

#[tokio::test]
async fn api_reports_configured_controls() {
    let f = fake::Fake::new().await;
    let (_dir, app) = controls::app(f.config());
    controls::listed(&app, controls::ALL).await;
}

#[tokio::test]
async fn shutdown_releases_the_session_once_without_machine_actions() {
    use dutchman_print::backend::Backend as _;
    let f = Fake::new().await;
    let backend = Safety::new(Arc::new(Printers::new(f.config()).unwrap()));
    backend.printers().await.unwrap();
    backend.shutdown().await.unwrap();
    backend.shutdown().await.unwrap();
    drop(backend);
    // A dropped client must not release the same session a second time.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let machine = f.shared.machine.lock().await;
    let released = machine
        .records
        .iter()
        .filter(|(path, _)| path == "/machine/disconnect")
        .count();
    assert_eq!(released, 1);
    assert!(machine.commands.is_empty());
}
