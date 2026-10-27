//! Repetier-Server monitoring and control, exercised only against test-owned loopback servers
//! and through the job API's confirmation challenges.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
#[path = "helpers/controls.rs"]
mod controls;
#[path = "helpers/repetier.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{body::Body, http::StatusCode, Router};
use dutchman_print::{
    backend::{Backend, Error, Options, PrinterState},
    printers::Printers,
    repetier::Client,
    safety::Safety,
};
use fake::{Fake, KEY, SLUG};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tower::ServiceExt as _;

static TOKEN: AtomicU64 = AtomicU64::new(1);

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
    format!("dutchman-{}", dutchman_print::safety::hash(b"one"))
}
/// A job API server holding one prepared job, "one", for the fake's printer.
async fn api(f: &Fake, dir: &Path) -> Router {
    let raw = Arc::new(Printers::new(f.config()).unwrap());
    let safety = Safety::new(raw.clone());
    safety
        .prepare("one", &input(dir), "test", Options::default())
        .await
        .unwrap();
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    safety
        .save(&settings.storage.data_dir.join("safety.json"))
        .await
        .unwrap();
    dutchman_print::router(raw, settings)
}
/// A job API server with no prepared job.
fn bare(f: &Fake, dir: &Path) -> Router {
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    dutchman_print::router(Arc::new(Printers::new(f.config()).unwrap()), settings)
}
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Value,
    token: Option<u64>,
) -> (StatusCode, Value) {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Plugin-Confirmation", format!("{token:064x}"));
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
/// Requests a challenge and, when one is issued, sends the confirmed action once.
async fn act(app: &Router, action: &str, job: Option<&str>) -> (StatusCode, Value) {
    let (code, challenge) = call(
        app,
        "POST",
        "/api/v1/confirmations",
        json!({"action":action,"printer":"test","job":job}),
        None,
    )
    .await;
    if code != StatusCode::CREATED {
        return (code, challenge);
    }
    let route = match job {
        Some(job) => format!("/api/v1/jobs/{job}/{action}"),
        None => format!("/api/v1/printers/test/{action}"),
    };
    call(
        app,
        "POST",
        &route,
        json!({"confirmation":challenge["id"]}),
        Some(TOKEN.fetch_add(1, Ordering::SeqCst)),
    )
    .await
}
async fn ok(app: &Router, action: &str, job: Option<&str>) -> Value {
    let (code, body) = act(app, action, job).await;
    let expected = if action == "clear" {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::OK
    };
    assert_eq!(code, expected, "{action}: {body}");
    body
}
fn error(body: &Value) -> String {
    body["error"].as_str().unwrap_or_default().to_owned()
}
async fn job(app: &Router) -> Value {
    let (code, body) = call(app, "GET", "/api/v1/jobs/one", Value::Null, None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    body
}
async fn printer(app: &Router) -> Value {
    let (code, body) = call(app, "GET", "/api/v1/printers", Value::Null, None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    body[0].clone()
}
async fn names(f: &Fake) -> (Vec<String>, Vec<String>) {
    let m = f.shared.machine.lock().await;
    (
        m.models.iter().map(|i| i.name.clone()).collect(),
        m.jobs.iter().map(|i| i.name.clone()).collect(),
    )
}

#[tokio::test]
async fn status_maps_only_documented_states() {
    let f = Fake::new().await;
    let c = Client::new(f.config().printers.remove(0)).unwrap();
    let idle = c.snapshot().await.unwrap().printer;
    assert_eq!(idle.state, PrinterState::Idle);
    assert_eq!(idle.detail.as_deref(), Some("online, no job"));
    // The temperatures are this printer's, never another slug's on the same server.
    assert_eq!(idle.nozzle_temperature, Some(201.5));
    assert_eq!(idle.bed_temperature, Some(59.0));
    assert_eq!(idle.progress, None);
    assert_eq!(idle.seconds_left, None);
    for (online, active, job, expected, detail) in [
        (json!(0), json!(true), json!("none"), PrinterState::Offline, "offline"),
        (json!(0), json!(false), json!("none"), PrinterState::Offline, "deactivated"),
        (json!(1), json!(false), json!("none"), PrinterState::Error, "Unknown Repetier-Server state: online 1, active false, job 'none'. Read fresh status."),
        (json!(2), json!(true), json!("none"), PrinterState::Error, "Unknown Repetier-Server state: online 2, active true, job 'none'. Read fresh status."),
        (json!(1), json!(true), json!(7), PrinterState::Error, "Unknown Repetier-Server state: online 1, active true, job 7. Read fresh status."),
        (json!(1), json!(true), json!(""), PrinterState::Error, "Unknown Repetier-Server state: online 1, active true, job ''. Read fresh status."),
        (json!(null), json!(true), json!("none"), PrinterState::Error, "Unknown Repetier-Server state: online null, active true, job 'none'. Read fresh status."),
    ] {
        {
            let mut m = f.shared.machine.lock().await;
            m.online = online;
            m.active = active;
            m.job = job;
        }
        let live = c.snapshot().await.unwrap();
        assert_eq!(live.printer.state, expected, "{detail}");
        assert_eq!(live.printer.detail.as_deref(), Some(detail));
        assert!(dutchman_print::repetier::ready(&live).is_err());
    }
    // A running job is printing; pause is not reported, so paused never appears.
    {
        let mut m = f.shared.machine.lock().await;
        m.online = json!(1);
        m.active = json!(true);
        m.job = json!("none");
        m.extruders = json!([{"tempRead":20.0,"tempSet":0},{"tempRead":215.0,"tempSet":215}]);
        m.active_extruder = 1;
    }
    f.external().await;
    let live = c.snapshot().await.unwrap();
    assert_eq!(live.printer.state, PrinterState::Printing);
    assert_eq!(live.printer.detail.as_deref(), Some("online, job active"));
    assert_eq!(live.printer.job_name.as_deref(), Some("external"));
    assert_eq!(live.printer.job_id.as_deref(), Some("11"));
    assert_eq!(live.printer.nozzle_temperature, Some(215.0));
    assert_eq!(live.printer.progress, None);
    // Two queued jobs with the live name leave the live job without an id.
    {
        let mut m = f.shared.machine.lock().await;
        let copy = m.jobs[0].clone();
        m.jobs.push(fake::Item { id: 99, ..copy });
    }
    assert_eq!(c.snapshot().await.unwrap().printer.job_id, None);
    f.shared.machine.lock().await.listed = false;
    let live = c.snapshot().await.unwrap().printer;
    assert_eq!(live.state, PrinterState::Error);
    assert!(live.detail.unwrap().contains("slug"));
}

#[tokio::test]
async fn unknown_state_blocks_start_and_offline_server_reports_offline() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    f.shared.machine.lock().await.online = json!(3);
    assert_eq!(printer(&app).await["state"], "error");
    let (code, body) = act(&app, "clear", None).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let (code, _) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(f.commands().await.is_empty());

    f.shared.machine.lock().await.key = "another".into();
    let live = printer(&app).await;
    assert_eq!(live["state"], "offline");
    assert_eq!(
        live["detail"],
        "Authentication failed. Check the Repetier-Server API key."
    );
    drop(f);
    let live = printer(&app).await;
    assert_eq!(live["state"], "offline");
    assert_eq!(
        live["detail"],
        "Fresh Repetier-Server telemetry unavailable."
    );
    assert!(!live.to_string().contains(KEY));
}

#[tokio::test]
async fn upload_queue_and_start_once_then_cancel_and_clean_up() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    let (code, body) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    ok(&app, "clear", None).await;
    let started = ok(&app, "start", Some("one")).await;
    assert_eq!(started["state"], "printing");
    let model = 11;
    let queued = 12;
    assert_eq!(
        f.commands().await,
        vec![
            format!("upload {}", staged()),
            format!("copyModel {}", json!({"autostart":false,"id":model})),
            format!("startJob {}", json!({"id":queued})),
        ]
    );
    {
        let m = f.shared.machine.lock().await;
        let position = |action: &str| {
            m.records
                .iter()
                .position(|r| {
                    r.action.as_deref() == Some(action)
                        || (action == "upload" && r.path.starts_with("/printer/model/"))
                })
                .unwrap()
        };
        assert!(position("canUploadGCodeWithSize") < position("upload"));
        assert!(position("upload") < position("copyModel"));
        assert!(position("copyModel") < position("startJob"));
        let upload = m
            .records
            .iter()
            .find(|r| r.path.starts_with("/printer/model/"))
            .unwrap();
        assert_eq!(upload.path, format!("/printer/model/{SLUG}"));
        assert_eq!(upload.query, format!("a=upload&name={}", staged()));
        assert_eq!(m.models[0].name, staged());
    }
    let live = printer(&app).await;
    assert_eq!(live["state"], "printing");
    assert_eq!(live["job_name"], staged());
    assert_eq!(live["job_id"], "12");

    // Pause is not documented and a paused job cannot be observed.
    let (code, body) = act(&app, "pause", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(error(&body).contains("documents no pause"), "{body}");
    let (code, _) = act(&app, "resume", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, body) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");

    let cancelled = ok(&app, "cancel", Some("one")).await;
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(f.actions().await[3], "stopJob");
    // The staged model and any queued copy are removed once the printer is idle.
    assert_eq!(names(&f).await, (vec![], vec![]));
    assert_eq!(job(&app).await["state"], "cancelled");
    assert_eq!(
        f.actions().await,
        vec!["upload", "copyModel", "startJob", "stopJob", "removeModel"]
    );
}

#[tokio::test]
async fn key_goes_only_in_the_header_and_requests_name_the_slug() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    ok(&app, "start", Some("one")).await;
    ok(&app, "cancel", Some("one")).await;
    let m = f.shared.machine.lock().await;
    assert!(m.records.len() > 10);
    for record in &m.records {
        assert_eq!(record.key.as_deref(), Some(KEY), "{record:?}");
        assert!(!record.query.contains(KEY), "{record:?}");
        assert!(
            !record.query.to_ascii_lowercase().contains("apikey"),
            "{record:?}"
        );
        assert!(!record.query.contains("sess="), "{record:?}");
        assert!(record.path.ends_with(&format!("/{SLUG}")), "{record:?}");
    }
    let stored = std::fs::read_dir(dir.path().join("api")).unwrap();
    for file in stored {
        let text = std::fs::read_to_string(file.unwrap().path()).unwrap_or_default();
        assert!(!text.contains(KEY) && !text.contains(&f.url));
    }
}

#[tokio::test]
async fn finished_job_ends_and_its_staged_model_is_removed() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    ok(&app, "start", Some("one")).await;
    f.finish().await;
    // The REST interface does not say whether the job finished or was stopped.
    assert_eq!(job(&app).await["state"], "ended");
    assert_eq!(names(&f).await, (vec![], vec![]));
    let removals = f
        .actions()
        .await
        .iter()
        .filter(|a| a.starts_with("remove"))
        .count();
    assert_eq!(job(&app).await["state"], "ended");
    assert_eq!(
        f.actions()
            .await
            .iter()
            .filter(|a| a.starts_with("remove"))
            .count(),
        removals
    );
    let live = printer(&app).await;
    assert_eq!(live["needs_clearing"], true);
    ok(&app, "clear", None).await;
    assert_eq!(printer(&app).await["needs_clearing"], false);
}

#[tokio::test]
async fn lost_start_reply_is_reconciled_and_never_resent() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    f.shared.machine.lock().await.start_lost = true;
    let (code, body) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(error(&body).contains("reply was lost"), "{body}");
    assert_eq!(job(&app).await["state"], "printing");
    let (code, _) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(f.actions().await, vec!["upload", "copyModel", "startJob"]);
}

#[tokio::test]
async fn lost_queue_reply_is_reconciled_and_start_follows_once() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    f.shared.machine.lock().await.copy_lost = true;
    assert_eq!(ok(&app, "start", Some("one")).await["state"], "printing");
    assert_eq!(f.actions().await, vec!["upload", "copyModel", "startJob"]);
}

#[tokio::test]
async fn delayed_outcomes_are_waited_for_and_each_command_sent_once() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    f.shared.machine.lock().await.delay_reads = 5;
    assert_eq!(ok(&app, "start", Some("one")).await["state"], "printing");
    f.shared.machine.lock().await.delay_reads = 5;
    assert_eq!(ok(&app, "cancel", Some("one")).await["state"], "cancelled");
    assert_eq!(
        f.actions().await,
        vec!["upload", "copyModel", "startJob", "stopJob", "removeModel"]
    );
}

#[tokio::test]
async fn server_that_starts_on_queueing_is_waited_for_not_started_again() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    f.shared.machine.lock().await.autostart = true;
    assert_eq!(ok(&app, "start", Some("one")).await["state"], "printing");
    assert_eq!(f.actions().await, vec!["upload", "copyModel"]);
}

#[tokio::test]
async fn refused_start_leaves_a_queued_job_that_cancel_removes() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    f.shared.machine.lock().await.start_refused = true;
    let (code, body) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(job(&app).await["state"], "ready");
    let (code, _) = act(&app, "start", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(ok(&app, "cancel", Some("one")).await["state"], "cancelled");
    assert_eq!(
        f.actions().await,
        vec![
            "upload",
            "copyModel",
            "startJob",
            "removeJob",
            "removeModel"
        ]
    );
    assert_eq!(names(&f).await, (vec![], vec![]));
}

#[tokio::test]
async fn controls_act_only_on_the_tracked_job() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = bare(&f, dir.path());
    f.external().await;
    assert_eq!(printer(&app).await["state"], "printing");
    for action in ["stop", "pause", "resume"] {
        let (code, body) = act(&app, action, None).await;
        assert_eq!(code, StatusCode::CONFLICT, "{action}");
        if action == "stop" {
            assert!(error(&body).contains("stopJob cannot name"), "{body}");
        }
    }
    // The backend refuses a Stop of the external job even with its exact identity.
    let backend = Printers::new(f.config()).unwrap();
    let error = backend
        .stop("test", "11", "external", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stopJob cannot name"), "{error}");
    assert!(f.commands().await.is_empty());

    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = api(&f, dir.path()).await;
    ok(&app, "clear", None).await;
    ok(&app, "start", Some("one")).await;
    // Another job replaces the tracked one: cancel and Stop send nothing.
    {
        let mut m = f.shared.machine.lock().await;
        m.jobs.clear();
        m.running = None;
        m.job = json!("none");
    }
    f.external().await;
    let (code, _) = act(&app, "cancel", Some("one")).await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, _) = act(&app, "stop", None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(f.actions().await, vec!["upload", "copyModel", "startJob"]);
    assert_eq!(job(&app).await["state"], "ended");
}

#[tokio::test]
async fn failed_and_uncertain_uploads_are_removed_without_queueing() {
    for mode in ["error", "lost", "length"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let app = api(&f, dir.path()).await;
        ok(&app, "clear", None).await;
        {
            let mut m = f.shared.machine.lock().await;
            m.models.push(fake::Item {
                id: 5,
                name: "unrelated".into(),
                bytes: b"G1".to_vec(),
            });
            match mode {
                "error" => m.upload_error = true,
                "lost" => m.upload_lost = true,
                _ => m.bad_length = true,
            }
        }
        let (code, body) = act(&app, "start", Some("one")).await;
        assert_ne!(code, StatusCode::OK, "{mode}: {body}");
        assert_eq!(f.actions().await, vec!["upload", "removeModel"], "{mode}");
        assert_eq!(
            names(&f).await,
            (vec!["unrelated".into()], vec![]),
            "{mode}"
        );
        assert_eq!(job(&app).await["state"], "ready", "{mode}");
    }
}

#[tokio::test]
async fn unsafe_start_cases_upload_nothing() {
    for mode in [
        "printing",
        "offline",
        "unknown",
        "extruders",
        "existing",
        "space",
        "firmware",
        "input",
    ] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let app = api(&f, dir.path()).await;
        ok(&app, "clear", None).await;
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "printing" => m.job = json!("other job"),
                "offline" => m.online = json!(0),
                "unknown" => m.online = json!(5),
                "extruders" => m.config_extruders = 2,
                "existing" => m.models.push(fake::Item {
                    id: 5,
                    name: staged(),
                    bytes: b"other".to_vec(),
                }),
                "space" => m.no_space = true,
                "firmware" => m.firmware = "Repetier".into(),
                "input" => std::fs::write(dir.path().join("input.3mf"), b"changed").unwrap(),
                _ => unreachable!(),
            }
        }
        let (code, body) = act(&app, "start", Some("one")).await;
        assert_eq!(code, StatusCode::CONFLICT, "{mode}: {body}");
        assert!(f.commands().await.is_empty(), "{mode}");
        if mode == "existing" {
            assert_eq!(f.shared.machine.lock().await.models[0].bytes, b"other");
        }
    }
}

#[tokio::test]
async fn configured_nozzle_must_match_the_slicer_and_be_set() {
    for nozzle in [Some(0.6), None] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path());
        let mut config = f.config();
        config.printers[0].nozzle_diameter = nozzle;
        let b = Printers::new(config).unwrap();
        let error = b
            .prepare("one", &path, "test", Options::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("nozzle"), "{error}");
        if nozzle.is_some() {
            assert!(error.contains("0.6") && error.contains("0.4"), "{error}");
        } else {
            assert!(error.contains("nozzle_diameter"), "{error}");
        }
        assert!(f.commands().await.is_empty());
    }
}

#[tokio::test]
async fn restart_preserves_an_uncertain_attempt() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = input(dir.path());
    let state = dir.path().join("jobs.json");
    let raw = Arc::new(Printers::new(f.config()).unwrap().with_store(state.clone()));
    let b = Safety::new(raw);
    b.prepare("one", &input, "test", Options::default())
        .await
        .unwrap();
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.start_lost = true;
    assert!(b.start("one").await.is_err());
    let raw = Arc::new(Printers::new(f.config()).unwrap());
    raw.restore(&state).await.unwrap();
    assert_eq!(
        raw.job("one").await.unwrap().state,
        dutchman_print::backend::JobState::Printing
    );
    assert!(matches!(raw.start("one").await, Err(Error::Refused(_))));
    assert_eq!(f.actions().await, vec!["upload", "copyModel", "startJob"]);
    let saved = std::fs::read_to_string(&state).unwrap();
    assert!(!saved.contains(KEY) && !saved.contains(&f.url));
}

#[test]
fn config_validates_url_key_slug_and_nozzle_without_echoing_secrets() {
    let mut p = dutchman_print::config::Printer {
        kind: Some("repetier".into()),
        api_key: Some("secret".into()),
        slug: Some("bench_1".into()),
        ..Default::default()
    };
    for url in ["http://127.0.0.1:3344", "https://127.0.0.1/proxy/path/"] {
        p.url = Some(url.into());
        assert!(dutchman_print::repetier::validate(&p).is_ok());
    }
    for url in [
        "file:///secret",
        "http://maker:secret@127.0.0.1",
        "http://127.0.0.1/?apikey=secret",
        "https://127.0.0.1/../secret",
        "http://127.0.0.1:0",
    ] {
        p.url = Some(url.into());
        assert!(!dutchman_print::repetier::validate(&p)
            .unwrap_err()
            .contains("secret"));
    }
    p.url = Some("http://127.0.0.1:3344".into());
    for key in [None, Some(""), Some("bad\nsecret")] {
        p.api_key = key.map(Into::into);
        assert!(!dutchman_print::repetier::validate(&p)
            .unwrap_err()
            .contains("secret"));
    }
    p.api_key = Some("secret".into());
    for slug in [None, Some(""), Some("a/b"), Some("bench 1")] {
        p.slug = slug.map(Into::into);
        assert!(dutchman_print::repetier::validate(&p)
            .unwrap_err()
            .contains("slug"));
    }
    p.slug = Some("bench_1".into());
    for nozzle in [0.0, -0.4, f64::NAN, 11.0] {
        p.nozzle_diameter = Some(nozzle);
        assert!(dutchman_print::repetier::validate(&p).is_err());
    }
    p.nozzle_diameter = Some(0.4);
    assert!(dutchman_print::repetier::validate(&p).is_ok());
    p.password = Some("secret".into());
    assert!(dutchman_print::repetier::validate(&p).is_err());

    let text = "[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='octoprint'\nid='test'\nname='Test'\nmodel='Test'\nurl='http://127.0.0.1'\napi_key='k'\nslug='bench'\nprinter_profile='M'\nprint_profile='D'\nfilament_profile='P'";
    let config: dutchman_print::config::Config = toml::from_str(text).unwrap();
    assert!(Printers::new(config)
        .err()
        .unwrap()
        .contains("slug only for Repetier-Server"));
}

#[tokio::test]
async fn api_reports_controls_and_refuses_unsupported_actions_without_reads() {
    let f = Fake::new().await;
    let (_dir, app) = controls::app(f.config());
    controls::unsupported(&app, "test", &["pause", "resume", "stop"]).await;
    assert!(f.shared.machine.lock().await.records.is_empty());
    controls::listed(&app, &["start", "cancel", "clear"]).await;
}
