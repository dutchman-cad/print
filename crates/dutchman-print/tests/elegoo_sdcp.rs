//! Elegoo SDCP monitoring and resin Stop, exercised only against test-owned loopback servers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "helpers/controls.rs"]
mod controls;
#[path = "helpers/elegoo_sdcp.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{body::Body, http::StatusCode, Router};
use dutchman_print::{
    backend::{Backend, Error, Options, PrinterState},
    elegoo_sdcp::{report, Client, Timing, MONITORING, RESIN_FILES, RESIN_PAUSE},
    printers::Printers,
    safety::Safety,
};
use fake::{centauri, resin, Fake, BRAND, MAINBOARD};
use serde_json::{json, Value};
use std::{future::Future, sync::Arc, time::Duration};
use tower::ServiceExt as _;

fn quick() -> Timing {
    Timing {
        fresh: Duration::from_secs(1),
        poll: Duration::from_millis(100),
        keepalive: Duration::from_millis(50),
        silence: Duration::from_secs(5),
        backoff: Duration::from_millis(50),
    }
}
async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}
fn refused(result: Result<impl std::fmt::Debug, Error>, message: &str) {
    match result {
        Err(Error::Refused(text)) => assert!(text.contains(message), "{text}"),
        other => panic!("expected refusal containing {message:?}, got {other:?}"),
    }
}
fn printing() -> Value {
    resin(&[1], 3, "task-a", "part.ctb", 0)
}
async fn api(app: &Router, path: &str, body: Value, token: Option<u64>) -> (StatusCode, Value) {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Plugin-Confirmation", format!("{token:064x}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
async fn challenge(app: &Router, action: &str) -> (StatusCode, Value) {
    api(
        app,
        "/api/v1/confirmations",
        json!({"action":action,"printer":"test"}),
        None,
    )
    .await
}
fn router(f: &Fake, profile: &str, dir: &std::path::Path) -> Router {
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    dutchman_print::router(
        Arc::new(Printers::new(f.config(profile)).unwrap()),
        settings,
    )
}

#[test]
fn resin_status_follows_the_vendor_enumerations() {
    let f = dutchman_print::config::Printer {
        kind: Some("elegoo-sdcp".into()),
        profile: Some("resin".into()),
        id: "test".into(),
        ..Default::default()
    };
    for (activity, substate, expected, detail) in [
        (vec![0], 0, PrinterState::Idle, "IDLE"),
        (vec![0], 8, PrinterState::Idle, "STOPPED"),
        (vec![0], 9, PrinterState::Idle, "COMPLETED"),
        (vec![1], 1, PrinterState::Printing, "HOMING"),
        (vec![1], 2, PrinterState::Printing, "DESCENDING"),
        // Exposing and lifting are active stages, never completion.
        (vec![1], 3, PrinterState::Printing, "EXPOSING"),
        (vec![1], 4, PrinterState::Printing, "LIFTING"),
        (vec![1], 5, PrinterState::Printing, "PAUSING"),
        (vec![1], 6, PrinterState::Paused, "PAUSED"),
        (vec![1], 7, PrinterState::Printing, "STOPPING"),
        (vec![1], 10, PrinterState::Printing, "FILE_CHECKING"),
        // A retained terminal substate never overrides the active print activity.
        (vec![1], 9, PrinterState::Printing, "PRINTING"),
        (vec![1], 8, PrinterState::Printing, "PRINTING"),
        (vec![1], 0, PrinterState::Printing, "PRINTING"),
        // Transfers, exposure tests, and self-tests are busy, whatever the substate says.
        (vec![2], 9, PrinterState::Busy, "FILE_TRANSFERRING"),
        (vec![3], 0, PrinterState::Busy, "EXPOSURE_TESTING"),
        (vec![4], 0, PrinterState::Busy, "DEVICES_TESTING"),
        (vec![0, 2], 0, PrinterState::Busy, "FILE_TRANSFERRING"),
        // Idle activity during an active stage is contradictory.
        (
            vec![0],
            3,
            PrinterState::Error,
            "EXPOSING; The machine reports idle",
        ),
        (vec![5], 0, PrinterState::Error, "UNKNOWN ACTIVITY"),
        (vec![], 0, PrinterState::Error, "UNKNOWN ACTIVITY"),
        (vec![0], 11, PrinterState::Error, "UNKNOWN PRINT STATUS 11"),
    ] {
        let p = report(&f, &resin(&activity, substate, "task-a", "part.ctb", 0));
        assert_eq!(p.state, expected, "{activity:?} {substate}");
        assert!(
            p.detail.as_deref().unwrap().starts_with(detail),
            "{activity:?} {substate}: {:?}",
            p.detail
        );
    }
    let p = report(&f, &resin(&[1], 3, "task-a", "part.ctb", 0));
    assert_eq!(p.job_id.as_deref(), Some("task-a"));
    assert_eq!(p.job_name.as_deref(), Some("part.ctb"));
    assert_eq!(p.progress, Some(0.1));
    // Ticks are milliseconds: (165000 - 65000) / 1000.
    assert_eq!(p.seconds_left, Some(100));
    // UV LED and enclosure temperatures are detail only, never nozzle or bed.
    assert_eq!(p.nozzle_temperature, None);
    assert_eq!(p.bed_temperature, None);
    assert_eq!(
        p.detail.as_deref(),
        Some("EXPOSING; UV LED 31 °C; box 25 °C")
    );
    let idle = report(&f, &resin(&[0], 9, "task-a", "part.ctb", 0));
    assert_eq!((idle.progress, idle.seconds_left), (None, None));
    let error = report(&f, &resin(&[1], 3, "task-a", "part.ctb", 3));
    assert_eq!(error.state, PrinterState::Error);
    assert!(error
        .detail
        .unwrap()
        .starts_with("EXPOSING; Error 3: resolution mismatch"));
    let missing = report(&f, &json!({"CurrentStatus":[0]}));
    assert_eq!(missing.state, PrinterState::Error);
    let hostile = report(&f, &resin(&[1], 3, "bad\ntask", &"x".repeat(300), 0));
    assert_eq!((hostile.job_id, hostile.job_name), (None, None));
}

#[test]
fn centauri_status_never_takes_resin_meanings() {
    let f = dutchman_print::config::Printer {
        kind: Some("elegoo-sdcp".into()),
        profile: Some("centauri".into()),
        id: "test".into(),
        ..Default::default()
    };
    for (substate, expected, detail) in [
        (0, PrinterState::Idle, "IDLE"),
        (5, PrinterState::Printing, "PAUSING"),
        (8, PrinterState::Printing, "PREPARING"),
        // Status 9 is starting here, never the resin completion.
        (9, PrinterState::Printing, "STARTING"),
        (13, PrinterState::Printing, "PRINTING"),
        (20, PrinterState::Printing, "RESUMING"),
        (10, PrinterState::Paused, "PAUSED"),
        (6, PrinterState::Error, "UNKNOWN STATUS 6"),
        (7, PrinterState::Error, "UNKNOWN STATUS 7"),
        (99, PrinterState::Error, "UNKNOWN STATUS 99"),
    ] {
        let p = report(&f, &centauri(substate, "task-c", "part.gcode"));
        assert_eq!(p.state, expected, "{substate}");
        assert!(
            p.detail.as_deref().unwrap().starts_with(detail),
            "{substate}: {:?}",
            p.detail
        );
    }
    let p = report(&f, &centauri(13, "task-c", "part.gcode"));
    assert_eq!(p.job_id.as_deref(), Some("task-c"));
    assert_eq!(p.job_name.as_deref(), Some("part.gcode"));
    assert_eq!(p.progress, Some(0.2));
    // Tick units are undocumented for Centauri, so time left stays unavailable.
    assert_eq!(p.seconds_left, None);
    assert_eq!(p.nozzle_temperature, Some(215.4));
    assert_eq!(p.bed_temperature, Some(60.2));
    assert_eq!(p.detail.as_deref(), Some("PRINTING; box 26 °C"));
    let mut resin_error = centauri(13, "task-c", "part.gcode");
    resin_error["PrintInfo"]["ErrorNumber"] = json!(3);
    assert_eq!(report(&f, &resin_error).state, PrinterState::Printing);
    assert_eq!(
        report(&f, &json!({"CurrentStatus":[0]})).state,
        PrinterState::Error
    );
}

#[tokio::test]
async fn status_requests_keepalive_and_identity_over_the_websocket() {
    let f = Fake::new(resin(&[0], 9, "task-a", "part.ctb", 0)).await;
    let c = Client::new(f.printer("resin"))
        .unwrap()
        .with_timing(quick());
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Idle, "{:?}", p.detail);
    assert_eq!(p.needs_clearing, Some(false));
    let first = f.commands(0).await.remove(0);
    // Before the first report the mainboard is unknown; the envelope is otherwise the
    // specification's.
    assert_eq!(first["Topic"], "sdcp/request/");
    assert_eq!(first["Data"]["MainboardID"], "");
    assert_eq!(first["Data"]["From"], 0);
    assert_eq!(first["Data"]["Data"], json!({}));
    assert_eq!(first["Data"]["RequestID"].as_str().unwrap().len(), 32);
    until("learned identity", || async {
        f.commands(0).await.iter().any(|r| {
            r["Topic"] == format!("sdcp/request/{MAINBOARD}")
                && r["Data"]["MainboardID"] == MAINBOARD
                && r["Id"] == BRAND
        })
    })
    .await;
    until("ping keepalive", || async {
        f.machine.lock().await.pings > 0
    })
    .await;

    // A changed report latches bed clearance until acknowledged.
    f.set(printing()).await;
    until("printing", || async {
        c.snapshot().await.state == PrinterState::Printing
    })
    .await;
    assert_eq!(c.snapshot().await.needs_clearing, Some(true));
    c.acknowledge();
    assert_eq!(c.snapshot().await.needs_clearing, Some(false));

    // Centauri requests follow the notes: no topic, From 1.
    let g = Fake::new(centauri(0, "", "")).await;
    let c = Client::new(g.printer("centauri"))
        .unwrap()
        .with_timing(quick());
    assert_eq!(c.snapshot().await.state, PrinterState::Idle);
    let first = g.commands(0).await.remove(0);
    assert!(first.get("Topic").is_none());
    assert_eq!(first["Data"]["From"], 1);

    // A configured mainboard_id refuses reports from another board.
    let h = Fake::new(resin(&[0], 0, "", "", 0)).await;
    h.machine.lock().await.reported_mainboard = "ffffffff".into();
    let mut config = h.printer("resin");
    config.mainboard_id = Some(MAINBOARD.into());
    let c = Client::new(config).unwrap().with_timing(quick());
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(p.detail.unwrap().contains("different mainboard"));
    assert_eq!(h.commands(0).await[0]["Data"]["MainboardID"], MAINBOARD);
    h.machine.lock().await.reported_mainboard = MAINBOARD.into();
    until("matching board", || async {
        c.snapshot().await.state == PrinterState::Idle
    })
    .await;
}

#[tokio::test]
async fn stale_status_and_reconnects_need_a_fresh_report() {
    let f = Fake::new(printing()).await;
    let c = Client::new(f.printer("resin"))
        .unwrap()
        .with_timing(quick());
    assert_eq!(c.snapshot().await.state, PrinterState::Printing);
    c.acknowledge();

    // A connection that stops reporting becomes stale, never idle.
    f.machine.lock().await.silent = true;
    until("stale", || async {
        let p = c.snapshot().await;
        p.state == PrinterState::Offline && p.detail.as_deref().is_some_and(|d| d.contains("stale"))
    })
    .await;
    let p = c.snapshot().await;
    assert_eq!((p.job_id, p.progress, p.needs_clearing), (None, None, None));
    f.machine.lock().await.silent = false;
    until("fresh again", || async {
        c.snapshot().await.state == PrinterState::Printing
    })
    .await;

    // After a dropped connection, nothing is fresh until the new connection reports.
    f.machine.lock().await.silent = true;
    f.drop_connections();
    until("reconnected without status", || async {
        f.machine.lock().await.connections >= 2
    })
    .await;
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Offline);
    refused(c.stop("task-a", "part.ctb").await, "live job changed");
    assert_eq!(f.controls().await, 0);
    f.machine.lock().await.silent = false;
    f.push().await;
    until("fresh after reconnect", || async {
        c.snapshot().await.state == PrinterState::Printing
    })
    .await;
    // The reconnect withdrew bed clearance.
    assert_eq!(c.snapshot().await.needs_clearing, Some(true));

    // A printer that cannot be reached is offline with a reason.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut config = f.printer("resin");
    config.port = Some(closed);
    let p = Client::new(config)
        .unwrap()
        .with_timing(quick())
        .snapshot()
        .await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(p.detail.unwrap().contains("unavailable"));
}

#[tokio::test]
async fn centauri_refuses_every_machine_action_without_sending_anything() {
    let f = Fake::new(centauri(13, "task-c", "part.gcode")).await;
    let dir = tempfile::tempdir().unwrap();
    let app = router(&f, "centauri", dir.path());
    for action in ["stop", "pause", "resume", "start", "cancel"] {
        let (code, body) = challenge(&app, action).await;
        assert_eq!(code, StatusCode::CONFLICT, "{action}");
        assert!(
            body["error"].as_str().unwrap().contains("monitoring only"),
            "{action}: {body}"
        );
    }
    let b = Safety::new(Arc::new(Printers::new(f.config("centauri")).unwrap()));
    let p = b.printer("test").await.unwrap();
    assert_eq!(p.state, PrinterState::Printing);
    assert_eq!(p.job_id.as_deref(), Some("task-c"));
    refused(
        b.stop("test", "task-c", "part.gcode", None).await,
        MONITORING,
    );
    refused(b.pause("test").await, MONITORING);
    f.set(centauri(10, "task-c", "part.gcode")).await;
    until("paused", || async {
        b.printer("test").await.unwrap().state == PrinterState::Paused
    })
    .await;
    refused(b.resume("test").await, MONITORING);
    refused(b.check_control("test"), MONITORING);
    let input = slicer_fixture::input(dir.path(), |_| Vec::new());
    refused(
        b.prepare("one", &input, "test", Options::default()).await,
        MONITORING,
    );
    assert!(matches!(b.start("one").await, Err(Error::NotFound)));
    assert_eq!(f.controls().await, 0);
    assert!(!f.commands(0).await.is_empty());
}

#[tokio::test]
async fn resin_stop_through_the_challenge_is_sent_once() {
    let f = Fake::new(printing()).await;
    let dir = tempfile::tempdir().unwrap();
    let app = router(&f, "resin", dir.path());
    let (code, body) = challenge(&app, "stop").await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    let summary = body["summary"].as_str().unwrap();
    assert!(
        summary.contains("'part.ctb'") && summary.contains("job task-a"),
        "{summary}"
    );
    let id = body["id"].clone();
    let (code, _) = api(
        &app,
        "/api/v1/printers/test/stop",
        json!({"confirmation":id}),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(f.controls().await, 0);
    let (code, body) = api(
        &app,
        "/api/v1/printers/test/stop",
        json!({"confirmation":id}),
        Some(1),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "idle");
    assert!(body["detail"].as_str().unwrap().starts_with("STOPPED"));
    let (code, _) = api(
        &app,
        "/api/v1/printers/test/stop",
        json!({"confirmation":id}),
        Some(2),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT);
    let stops = f.commands(130).await;
    assert_eq!(stops.len(), 1);
    assert_eq!(f.controls().await, 1);
    let stop = &stops[0];
    assert_eq!(stop["Topic"], format!("sdcp/request/{MAINBOARD}"));
    assert_eq!(stop["Id"], BRAND);
    assert_eq!(stop["Data"]["MainboardID"], MAINBOARD);
    assert_eq!(stop["Data"]["Data"], json!({}));
    // Idle afterwards: nothing more to stop.
    assert_eq!(challenge(&app, "stop").await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn resin_pause_and_resume_are_refused_in_favour_of_stop() {
    let f = Fake::new(printing()).await;
    let dir = tempfile::tempdir().unwrap();
    let app = router(&f, "resin", dir.path());
    let (code, body) = challenge(&app, "pause").await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("use Stop"),
        "{body}"
    );
    let b = Safety::new(Arc::new(Printers::new(f.config("resin")).unwrap()));
    refused(b.pause("test").await, RESIN_PAUSE);
    f.set(resin(&[1], 6, "task-a", "part.ctb", 0)).await;
    until("paused", || async {
        b.printer("test").await.unwrap().state == PrinterState::Paused
    })
    .await;
    let (code, body) = challenge(&app, "resume").await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("use Stop"),
        "{body}"
    );
    refused(b.resume("test").await, RESIN_PAUSE);
    // Stop remains available for the paused job.
    assert_eq!(challenge(&app, "stop").await.0, StatusCode::CREATED);
    assert_eq!(f.controls().await, 0);
}

#[tokio::test]
async fn resin_stop_ack_failure_is_reported_and_not_resent() {
    let f = Fake::new(printing()).await;
    f.machine.lock().await.ack = 1;
    let b = Safety::new(Arc::new(Printers::new(f.config("resin")).unwrap()));
    assert_eq!(
        b.printer("test").await.unwrap().state,
        PrinterState::Printing
    );
    refused(
        b.stop("test", "task-a", "part.ctb", None).await,
        "The printer refused Stop: busy.",
    );
    assert_eq!(f.commands(130).await.len(), 1);
    assert_eq!(
        b.printer("test").await.unwrap().state,
        PrinterState::Printing
    );
    // A different live task is never stopped.
    refused(
        b.stop("test", "task-b", "part.ctb", None).await,
        "live job changed",
    );
    assert_eq!(f.commands(130).await.len(), 1);
}

#[tokio::test]
async fn resin_stop_with_a_lost_reply_is_not_resent() {
    let f = Fake::new(printing()).await;
    let b = Safety::new(Arc::new(Printers::new(f.config("resin")).unwrap()));
    assert_eq!(
        b.printer("test").await.unwrap().state,
        PrinterState::Printing
    );
    {
        let mut m = f.machine.lock().await;
        m.swallow = true;
        m.silent = true;
    }
    refused(
        b.stop("test", "task-a", "part.ctb", None).await,
        "Stop reply was lost. It was sent once and will not be resent",
    );
    until("reconnected", || async {
        f.machine.lock().await.connections >= 2
    })
    .await;
    // No fresh report on the new connection yet: Stop cannot even be confirmed.
    let dir = tempfile::tempdir().unwrap();
    let app = router(&f, "resin", dir.path());
    assert_eq!(challenge(&app, "stop").await.0, StatusCode::CONFLICT);
    f.machine.lock().await.silent = false;
    f.push().await;
    until("fresh after reconnect", || async {
        b.printer("test").await.unwrap().state == PrinterState::Printing
    })
    .await;
    assert_eq!(f.commands(130).await.len(), 1);
}

#[tokio::test]
async fn resin_stop_never_takes_another_task_as_its_outcome() {
    let f = Fake::new(printing()).await;
    // The printer reports a retained STOPPED for a different task after acknowledging.
    f.machine.lock().await.stop_status = Some(resin(&[0], 8, "task-b", "other.ctb", 0));
    let b = Safety::new(Arc::new(Printers::new(f.config("resin")).unwrap()));
    assert_eq!(
        b.printer("test").await.unwrap().state,
        PrinterState::Printing
    );
    refused(
        b.stop("test", "task-a", "part.ctb", None).await,
        "Live job changed while Stop was pending",
    );
    assert_eq!(f.commands(130).await.len(), 1);

    // A completion of the same task before Stop took effect is not reported as stopped.
    let g = Fake::new(printing()).await;
    g.machine.lock().await.stop_status = Some(resin(&[0], 9, "task-a", "part.ctb", 0));
    let b = Safety::new(Arc::new(Printers::new(g.config("resin")).unwrap()));
    assert_eq!(
        b.printer("test").await.unwrap().state,
        PrinterState::Printing
    );
    refused(
        b.stop("test", "task-a", "part.ctb", None).await,
        "completed before Stop was confirmed",
    );
}

#[tokio::test]
async fn resin_prepare_and_start_are_refused() {
    let f = Fake::new(resin(&[0], 0, "", "", 0)).await;
    let dir = tempfile::tempdir().unwrap();
    let b = Safety::new(Arc::new(Printers::new(f.config("resin")).unwrap()));
    let input = slicer_fixture::input(dir.path(), |_| Vec::new());
    refused(
        b.prepare("one", &input, "test", Options::default()).await,
        RESIN_FILES,
    );
    assert!(matches!(b.start("one").await, Err(Error::NotFound)));
    let app = router(&f, "resin", dir.path());
    let (code, _) = api(
        &app,
        "/api/v1/confirmations",
        json!({"action":"start","printer":"test","job":"one"}),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT);
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/jobs?printer=test")
                .header("Content-Type", "model/3mf")
                .body(Body::from(std::fs::read(&input).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("resin slicer"));
    assert_eq!(f.controls().await, 0);
}

#[test]
fn config_requires_a_profile_and_no_slicing_fields() {
    let base = "[backend]\nkind='printers'\n[[printers]]\nkind='elegoo-sdcp'\nid='test'\nname='Test'\nmodel='Test'\nhost='192.168.1.20'\n";
    let load = |extra: &str| {
        let config: dutchman_print::config::Config =
            toml::from_str(&format!("{base}{extra}")).unwrap();
        Printers::new(config).err()
    };
    // No slicer and no slicer profiles are needed.
    assert_eq!(load("profile='resin'"), None);
    assert_eq!(
        load("profile='centauri'\nport=8080\nmainboard_id='abc123'"),
        None
    );
    for (extra, message) in [
        ("", "Set profile to centauri or resin"),
        ("profile='auto'", "Set profile to centauri or resin"),
        ("profile='resin'\nport=0", "port"),
        ("profile='resin'\nmainboard_id='bad id'", "mainboard_id"),
        ("profile='resin'\nprinter_profile='M'", "leave slicer"),
        ("profile='resin'\nslicer='prusa'", "leave slicer"),
        (
            "profile='resin'\nurl='http://192.168.1.20'",
            "Set only host",
        ),
        ("profile='resin'\nlan_control=true", "Set only host"),
    ] {
        let error = load(extra).unwrap();
        assert!(error.contains(message), "{extra}: {error}");
    }
    let text = "[backend]\nkind='printers'\n[[printers]]\nkind='elegoo-sdcp'\nid='test'\nname='Test'\nmodel='Test'\nhost='http://192.168.1.20'\nprofile='resin'";
    let config: dutchman_print::config::Config = toml::from_str(text).unwrap();
    assert!(Printers::new(config)
        .err()
        .unwrap()
        .contains("without a scheme"));
    let text = "[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='duet'\nid='test'\nname='Test'\nmodel='Test'\nurl='http://127.0.0.1'\nprofile='resin'\nprinter_profile='M'\nprint_profile='D'\nfilament_profile='P'";
    let config: dutchman_print::config::Config = toml::from_str(text).unwrap();
    assert!(Printers::new(config)
        .err()
        .unwrap()
        .contains("only for Elegoo SDCP"));
}

#[tokio::test]
async fn api_reports_controls_and_refuses_unsupported_actions_without_reads() {
    for (profile, expected, unsupported) in [
        (
            "resin",
            vec!["stop"],
            vec!["start", "pause", "resume", "cancel", "clear"],
        ),
        ("centauri", vec![], controls::ALL.to_vec()),
    ] {
        let f = Fake::new(printing()).await;
        let (_dir, app) = controls::app(f.config(profile));
        controls::unsupported(&app, "test", &unsupported).await;
        assert_eq!(f.machine.lock().await.connections, 0);
        assert!(f.machine.lock().await.requests.is_empty());
        controls::listed(&app, &expected).await;
    }
}
