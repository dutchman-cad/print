//! Moonraker control policy exercised only against test-owned loopback servers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
#[path = "helpers/moonraker.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{body::Body, http::StatusCode, Router};
use dutchman_print::{
    backend::{Backend, Error, JobState, Options, PrinterState},
    moonraker::Client,
    printers::Printers,
    safety::Safety,
};
use fake::Fake;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
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
async fn backend(f: &Fake, dir: &Path) -> Arc<Safety> {
    let path = input(dir);
    let b = Arc::new(Safety::new(Arc::new(Printers::new(f.config()).unwrap())));
    b.prepare("one", &path, "test", Options::default())
        .await
        .unwrap();
    b
}
#[tokio::test]
async fn mapping_klippy_states_auth_and_optional_telemetry() {
    let f = Fake::new().await;
    let c = Client::new(f.config().printers.remove(0)).unwrap();
    for (state, expected) in [
        ("standby", PrinterState::Idle),
        ("printing", PrinterState::Printing),
        ("paused", PrinterState::Paused),
        ("complete", PrinterState::Idle),
        ("cancelled", PrinterState::Idle),
        ("error", PrinterState::Error),
        ("future", PrinterState::Error),
    ] {
        f.shared.machine.lock().await.state = state.into();
        assert_eq!(
            c.snapshot().await.unwrap().printer.state,
            expected,
            "{state}"
        );
    }
    for (klippy, expected) in [
        ("startup", PrinterState::Busy),
        ("shutdown", PrinterState::Error),
        ("error", PrinterState::Error),
        ("disconnected", PrinterState::Offline),
        ("future", PrinterState::Error),
    ] {
        f.shared.machine.lock().await.klippy = klippy.into();
        assert_eq!(
            c.snapshot().await.unwrap().printer.state,
            expected,
            "{klippy}"
        );
    }
    {
        let mut m = f.shared.machine.lock().await;
        m.klippy = "ready".into();
        m.state = "printing".into();
    }
    let p = c.snapshot().await.unwrap().printer;
    assert_eq!(p.progress, Some(0.25));
    assert_eq!(p.seconds_left, None);
    assert_eq!(p.nozzle_temperature, Some(201.5));
    assert_eq!(p.bed_temperature, Some(59.0));
    f.shared.machine.lock().await.progress = Value::Null;
    assert_eq!(c.snapshot().await.unwrap().printer.progress, None);
    f.shared.machine.lock().await.auth = true;
    assert!(matches!(c.snapshot().await, Err(Error::Authentication)));
    let public =
        serde_json::to_string(&Printers::new(f.config()).unwrap().printers().await.unwrap())
            .unwrap();
    assert!(!public.contains("secret") && !public.contains(&f.url));
    assert!(public.contains("Authentication failed"));
}
#[tokio::test]
async fn stage_start_control_finish_and_clearance() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
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
    {
        let mut m = f.shared.machine.lock().await;
        m.state = "complete".into();
        m.history_state = "completed".into();
    }
    assert_eq!(b.job("one").await.unwrap().state, JobState::Done);
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    b.clear("test").await.unwrap();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
    let m = f.shared.machine.lock().await;
    let upload = m
        .records
        .iter()
        .position(|(_, p, _)| p == "/server/files/upload")
        .unwrap();
    let start = m
        .records
        .iter()
        .position(|(_, p, _)| p == "/printer/print/start")
        .unwrap();
    assert!(m.records[..upload]
        .iter()
        .any(|(_, p, _)| p == "/printer/objects/query"));
    assert!(m.records[upload + 1..start]
        .iter()
        .any(|(_, p, _)| p == "/server/files/metadata"));
}
#[tokio::test]
async fn unsafe_start_cases_send_no_start() {
    for mode in [
        "size",
        "incomplete",
        "queued",
        "input",
        "output",
        "identity",
        "firmware",
        "controller_firmware",
        "nozzle",
        "volume",
        "material",
        "macros",
        "shutdown",
        "disconnected",
        "webhooks",
        "unknown",
        "contradictory",
        "missing",
        "partial",
    ] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "size" => m.bad_size = true,
                "incomplete" => m.incomplete = true,
                "queued" => m.queued = true,
                "input" => std::fs::write(dir.path().join("input.3mf"), b"changed").unwrap(),
                "output" => std::fs::write(dir.path().join("sliced.gcode"), b"G28\n").unwrap(),
                "identity" => m.identity = "changed".into(),
                "firmware" => m.firmware = "changed".into(),
                "controller_firmware" => m.mcu_version = "changed".into(),
                "nozzle" => m.settings["extruder"]["nozzle_diameter"] = json!(0.6),
                "volume" => m.settings["stepper_x"] = json!({"position_max":200}),
                "material" => m.settings["extruder1"] = json!({"nozzle_diameter":0.4}),
                "macros" => {
                    m.config["gcode_macro RESUME"] = json!({"gcode":"synthetic changed macro"})
                }
                "shutdown" => m.klippy = "shutdown".into(),
                "disconnected" => m.connected = false,
                "webhooks" => m.webhook = "shutdown".into(),
                "unknown" => m.state = "future".into(),
                "contradictory" => m.active = Some(true),
                "missing" => m.missing = true,
                "partial" => m.partial = true,
                _ => unreachable!(),
            }
        }
        assert!(b.start("one").await.is_err(), "{mode}");
        assert!(f.commands().await.is_empty(), "{mode}");
    }
}
#[tokio::test]
async fn lost_reply_and_replacement_execution_are_never_retried_or_controlled() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
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
    b.pause("test").await.unwrap();
    f.shared.machine.lock().await.id = Some("000002".into());
    assert!(b.resume("test").await.is_err());
    f.shared.machine.lock().await.state = "complete".into();
    f.shared.machine.lock().await.history_state = "completed".into();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Paused);
    assert!(b.cancel("one").await.is_err());
    assert!(b.job("one").await.is_err());
    assert_eq!(
        f.commands().await,
        vec!["/printer/print/start", "/printer/print/pause"]
    );
}
#[tokio::test]
async fn stop_during_upload_or_start_cancels_only_the_live_job() {
    for path in ["/server/files/upload", "/printer/print/start"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        f.shared.machine.lock().await.block = Some(path.into());
        let start = tokio::spawn({
            let b = b.clone();
            async move { b.start("one").await }
        });
        f.shared.entered.notified().await;
        let cancel = tokio::spawn({
            let b = b.clone();
            async move { b.cancel("one").await }
        });
        tokio::task::yield_now().await;
        f.shared.release.notify_one();
        assert_eq!(start.await.unwrap().unwrap().state, JobState::Cancelled);
        assert_eq!(cancel.await.unwrap().unwrap().state, JobState::Cancelled);
        assert_eq!(
            f.commands().await,
            vec!["/printer/print/start", "/printer/print/cancel"]
        );
        assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    }
}
#[tokio::test]
async fn resume_requires_commissioning_unchanged_macros_and_complete_fresh_snapshot() {
    for mode in ["commissioning", "macros", "partial", "shutdown"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path());
        let mut config = f.config();
        if mode == "commissioning" {
            config.printers[0].resume_commissioned = false;
        }
        let b = Safety::new(Arc::new(Printers::new(config).unwrap()));
        b.prepare("one", &path, "test", Options::default())
            .await
            .unwrap();
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        b.pause("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "macros" => m.config["changed"] = json!(true),
                "partial" => m.partial = true,
                "shutdown" => m.webhook = "shutdown".into(),
                _ => {}
            }
        }
        assert!(b.resume("test").await.is_err(), "{mode}");
        assert_eq!(f.commands().await.len(), 2);
    }
}
#[tokio::test]
async fn job_outcomes_are_distinct_and_idle_is_not_success() {
    for (state, history, expected) in [
        ("complete", "completed", JobState::Done),
        ("cancelled", "cancelled", JobState::Cancelled),
        ("error", "error", JobState::Failed),
        ("standby", "in_progress", JobState::Printing),
        ("complete", "in_progress", JobState::Printing),
    ] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        b.start("one").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            m.state = state.into();
            m.history_state = history.into();
        }
        assert_eq!(b.job("one").await.unwrap().state, expected);
        assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
        if expected != JobState::Printing {
            if state == "error" {
                f.shared.machine.lock().await.state = "standby".into();
                assert_eq!(b.job("one").await.unwrap().state, expected);
            }
            assert!(f.shared.machine.lock().await.bytes.is_empty());
            assert_eq!(
                f.shared
                    .machine
                    .lock()
                    .await
                    .records
                    .iter()
                    .filter(|(method, path, _)| method == "DELETE"
                        && path.starts_with("/server/files/gcodes/dutchman-"))
                    .count(),
                1
            );
        } else {
            assert!(!f.shared.machine.lock().await.bytes.is_empty());
        }
    }
}
#[tokio::test]
async fn start_conflict_and_stale_history_never_prove_a_new_execution() {
    for lost in [false, true] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            m.conflict = !lost;
            m.stale_history = lost;
        }
        assert!(b.start("one").await.is_err());
        assert_eq!(b.job("one").await.unwrap().state, JobState::Ready);
        assert!(b.start("one").await.is_err());
        assert_eq!(f.commands().await.len(), 1);
    }
}
#[tokio::test]
async fn trusted_clients_may_omit_api_key_and_bad_telemetry_is_bounded() {
    let f = Fake::new().await;
    f.shared.machine.lock().await.require_key = false;
    let mut config = f.config();
    config.printers[0].api_key = None;
    let c = Client::new(config.printers.remove(0)).unwrap();
    assert_eq!(
        c.snapshot().await.unwrap().printer.state,
        PrinterState::Idle
    );
    f.shared.machine.lock().await.oversized = true;
    assert!(c.snapshot().await.is_err());
    f.shared.machine.lock().await.oversized = false;
    f.shared.machine.lock().await.slow = true;
    let begin = std::time::Instant::now();
    assert!(c.snapshot().await.is_err());
    assert!(begin.elapsed().as_secs() < 6);
}
#[test]
fn config_validates_urls_and_credentials_without_echoing_secrets() {
    let mut p = dutchman_print::config::Printer::default();
    for url in ["http://127.0.0.1:7125", "https://127.0.0.1/proxy/path/"] {
        p.url = Some(url.into());
        assert!(dutchman_print::moonraker::validate(&p).is_ok());
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
        assert!(!dutchman_print::moonraker::validate(&p)
            .unwrap_err()
            .contains("secret"));
    }
    p.url = Some("https://127.0.0.1".into());
    p.api_key = Some("bad\nsecret".into());
    assert!(dutchman_print::moonraker::validate(&p).is_err());
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
async fn api_actions_require_fresh_confirmation_before_commands() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
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
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let challenge = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/confirmations")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"action":"clear","printer":"test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(challenge.status(), StatusCode::CREATED);
    let challenge: Value = serde_json::from_slice(
        &axum::body::to_bytes(challenge.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    let token = "a".repeat(64);
    for expected in [StatusCode::NO_CONTENT, StatusCode::CONFLICT] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/v1/printers/test/clear?confirmation={}",
                        challenge["id"].as_str().unwrap()
                    ))
                    .header("Plugin-Confirmation", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    f.shared.machine.lock().await.settings["extruder"]["nozzle_diameter"] = json!(0.6);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/confirmations")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"action":"start","printer":"test","job":"one"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let message = String::from_utf8_lossy(&bytes);
    assert!(message.contains("0.4") && message.contains("0.6"));
    assert!(f.commands().await.is_empty());
    f.shared.machine.lock().await.settings["extruder"]["nozzle_diameter"] = json!(0.4);
    api_action(&app, "start", Some("one"), 2).await;
    api_action(&app, "pause", None, 3).await;
    api_action(&app, "resume", None, 4).await;
    api_action(&app, "cancel", Some("one"), 5).await;
    assert_eq!(f.commands().await.len(), 4);
}
#[tokio::test]
async fn restart_preserves_uncertain_attempt_and_requires_new_bed_clearance() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = input(dir.path());
    let state = dir.path().join("jobs.json");
    let safety = dir.path().join("safety.json");
    let raw = Arc::new(Printers::new(f.config()).unwrap().with_store(state.clone()));
    let b = Safety::new(raw).with_store(safety.clone());
    b.prepare("one", &input, "test", Options::default())
        .await
        .unwrap();
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.lost = true;
    assert!(b.start("one").await.is_err());
    let raw = Arc::new(Printers::new(f.config()).unwrap());
    raw.restore(&state).await.unwrap();
    let b = Safety::new(raw);
    b.restore(&safety).await.unwrap();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    assert!(b.start("one").await.is_err());
    assert_eq!(f.commands().await.len(), 1);
    for file in [&state, &safety] {
        let value = std::fs::read_to_string(file).unwrap();
        assert!(!value.contains("secret") && !value.contains(&f.url));
    }
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
}
#[tokio::test]
async fn profile_mismatches_unresolved_slots_and_binary_gcode_are_refused() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = input(dir.path());
    let b = Printers::new(f.config()).unwrap();
    for options in [
        Options {
            printer_profile: Some("Other model volume or plate".into()),
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
        assert!(b.prepare("other", &input, "test", options).await.is_err());
    }
    f.shared.machine.lock().await.settings["extruder1"] = json!({"nozzle_diameter":0.4});
    assert!(b
        .prepare("other", &input, "test", Options::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("slot"));
    f.shared
        .machine
        .lock()
        .await
        .settings
        .as_object_mut()
        .unwrap()
        .remove("extruder1");
    std::fs::write(dir.path().join("mode"), b"binary").unwrap();
    assert!(b
        .prepare("other", &input, "test", Options::default())
        .await
        .is_err());
    assert!(f.commands().await.is_empty());
}
#[tokio::test]
async fn changed_configured_model_and_profiles_require_new_review() {
    for field in ["model", "printer", "print", "material"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let input = input(dir.path());
        let b = Printers::new(f.config()).unwrap();
        b.prepare("one", &input, "test", Options::default())
            .await
            .unwrap();
        let state = dir.path().join("jobs.json");
        b.save(&state).await.unwrap();
        let mut config = f.config();
        let p = &mut config.printers[0];
        match field {
            "model" => p.model = "changed".into(),
            "printer" => p.printer_profile = Some("changed".into()),
            "print" => p.print_profile = Some("changed".into()),
            _ => p.filament_profile = Some("changed".into()),
        }
        let b = Printers::new(config).unwrap();
        b.restore(&state).await.unwrap();
        assert!(b
            .start("one")
            .await
            .unwrap_err()
            .to_string()
            .contains("profiles changed"));
        assert!(f.commands().await.is_empty());
    }
}

#[tokio::test]
async fn old_history_for_the_staged_filename_cannot_finish_a_new_attempt() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    {
        let mut m = f.shared.machine.lock().await;
        m.id = Some("000001".into());
        m.history_state = "completed".into();
        m.stale_history = true;
    }
    b.clear("test").await.unwrap();
    assert!(b.start("one").await.is_err());
    assert_eq!(b.job("one").await.unwrap().state, JobState::Ready);
    assert!(b.start("one").await.is_err());
    assert_eq!(f.commands().await.len(), 1);
}
#[tokio::test]
async fn existing_staging_file_is_not_overwritten_and_backend_messages_stay_private() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    {
        let mut m = f.shared.machine.lock().await;
        m.name = format!("dutchman-{}.gcode", dutchman_print::safety::hash(b"one"));
    }
    assert!(b.start("one").await.is_err());
    assert!(f.commands().await.is_empty());
    assert!(!f
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|(_, p, _)| p == "/server/files/upload"));
    f.shared.machine.lock().await.state = "error".into();
    let public = serde_json::to_string(&b.printers().await.unwrap()).unwrap();
    assert!(
        !public.contains("secret")
            && !public.contains("private.invalid")
            && !public.contains(&f.url)
    );
}

#[tokio::test]
async fn history_fractions_and_observed_activity_withdraw_clearance() {
    for change in ["history", "filename", "state"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        f.shared.machine.lock().await.id = Some("old".into());
        b.clear("test").await.unwrap();
        assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
        {
            let mut m = f.shared.machine.lock().await;
            match change {
                "history" => m.history_time += 0.125,
                "filename" => m.name = "external.gcode".into(),
                _ => m.state = "complete".into(),
            }
        }
        assert_eq!(
            b.printers().await.unwrap()[0].needs_clearing,
            Some(true),
            "{change}"
        );
    }
}
#[tokio::test]
async fn configured_nozzle_must_match_slicer_and_be_known() {
    for nozzle in [json!(0.6), Value::Null] {
        let f = Fake::new().await;
        f.shared.machine.lock().await.settings["extruder"]["nozzle_diameter"] = nozzle.clone();
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path());
        let b = Printers::new(f.config()).unwrap();
        let error = b
            .prepare("one", &path, "test", Options::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("nozzle"));
        if nozzle == json!(0.6) {
            assert!(error.contains("0.6") && error.contains("0.4"));
        }
        assert!(f.commands().await.is_empty());
    }
}
#[tokio::test]
async fn failed_and_uncertain_uploads_are_deleted_without_start() {
    for mode in ["lost", "size", "queued"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "lost" => m.upload_lost = true,
                "size" => m.bad_size = true,
                _ => m.queued = true,
            }
        }
        assert!(b.start("one").await.is_err());
        assert!(f.commands().await.is_empty());
        let m = f.shared.machine.lock().await;
        assert!(m.records.iter().any(
            |(method, path, _)| method == "DELETE" && path.starts_with("/server/files/gcodes/")
        ));
        assert!(m.bytes.is_empty());
    }
}
#[tokio::test]
async fn delayed_commands_are_polled_and_sent_once() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.delay_reads = 7;
    assert_eq!(b.start("one").await.unwrap().state, JobState::Printing);
    f.shared.machine.lock().await.delay_reads = 4;
    assert_eq!(b.pause("test").await.unwrap().state, JobState::Paused);
    f.shared.machine.lock().await.delay_reads = 4;
    assert_eq!(b.resume("test").await.unwrap().state, JobState::Printing);
    f.shared.machine.lock().await.delay_reads = 4;
    assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(
        f.commands().await,
        vec![
            "/printer/print/start",
            "/printer/print/pause",
            "/printer/print/resume",
            "/printer/print/cancel"
        ]
    );
}
#[tokio::test]
async fn missing_sliced_nozzle_is_refused() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let path = input(dir.path());
    std::fs::write(dir.path().join("mode"), "missing").unwrap();
    let b = Printers::new(f.config()).unwrap();
    assert!(b
        .prepare("one", &path, "test", Options::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("nozzle"));
    assert!(f.commands().await.is_empty());
}
#[tokio::test]
async fn background_polling_withdraws_clearance_after_external_history() {
    let f = Fake::new().await;
    let b = Arc::new(
        Safety::new(Arc::new(Printers::new(f.config()).unwrap())).timing(
            Arc::new(dutchman_print::simulated::SystemClock::default()),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(60),
        ),
    );
    b.clear("test").await.unwrap();
    let before = {
        let mut m = f.shared.machine.lock().await;
        m.id = Some("external".into());
        m.records
            .iter()
            .filter(|(_, path, _)| path == "/server/history/list")
            .count()
    };
    b.monitor();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let count = f
                .shared
                .machine
                .lock()
                .await
                .records
                .iter()
                .filter(|(_, path, _)| path == "/server/history/list")
                .count();
            if count > before {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(b.printer("test").await.unwrap().needs_clearing, Some(true));
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
async fn each_job_change_is_checkpointed_and_restored() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("jobs.json");
    let raw = Arc::new(Printers::new(f.config()).unwrap().with_store(state.clone()));
    let b = Safety::new(raw.clone());
    b.prepare("one", &input(dir.path()), "test", Options::default())
        .await
        .unwrap();
    b.clear("test").await.unwrap();
    for (action, expected) in [
        ("start", JobState::Printing),
        ("pause", JobState::Paused),
        ("resume", JobState::Printing),
        ("cancel", JobState::Cancelled),
    ] {
        match action {
            "start" => b.start("one").await.unwrap(),
            "pause" => b.pause("test").await.unwrap(),
            "resume" => b.resume("test").await.unwrap(),
            _ => b.cancel("one").await.unwrap(),
        };
        let restored = Printers::new(f.config()).unwrap();
        restored.restore(&state).await.unwrap();
        assert_eq!(restored.job("one").await.unwrap().state, expected);
        let stored: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        assert_eq!(
            stored["one"]["job"]["state"],
            serde_json::to_value(expected).unwrap()
        );
    }
    raw.forget("one").await.unwrap();
    let stored: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert!(stored.get("one").is_none());
}

#[tokio::test]
async fn independent_printers_and_stop_during_start_poll() {
    use std::time::Duration;
    use tokio::time::timeout;
    for pending in [false, true] {
        let a = Fake::new().await;
        let b = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path());
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
                m.block = Some("/server/files/upload".into());
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
            let live = backend.printer("other").await.unwrap();
            assert_eq!(
                backend
                    .stop(
                        "other",
                        live.job_id.as_deref().unwrap(),
                        live.job_name.as_deref().unwrap(),
                        live.job_path.as_deref()
                    )
                    .await
                    .unwrap()
                    .state,
                PrinterState::Idle
            );
            assert_eq!(backend.job("two").await.unwrap().state, JobState::Cancelled);
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
        let backend = backend(&f, dir.path()).await;
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
        let live = backend.printer("test").await.unwrap();
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                backend.stop(
                    "test",
                    live.job_id.as_deref().unwrap(),
                    live.job_name.as_deref().unwrap(),
                    live.job_path.as_deref(),
                )
            )
            .await
            .unwrap()
            .unwrap()
            .state,
            PrinterState::Idle
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

#[tokio::test]
async fn printer_stop_external_history_execution_leaves_its_file() {
    let f = Fake::new().await;
    let b = Safety::new(Arc::new(Printers::new(f.config()).unwrap()));
    {
        let mut m = f.shared.machine.lock().await;
        m.name = "external.gcode".into();
        m.id = Some("E123".into());
        m.state = "printing".into();
        m.history_state = "in_progress".into();
    }
    let live = b.printer("test").await.unwrap();
    assert!(b
        .stop(
            "test",
            "old-execution",
            live.job_name.as_deref().unwrap(),
            None
        )
        .await
        .is_err());
    assert!(f.commands().await.is_empty());
    let p = b
        .stop(
            "test",
            live.job_id.as_deref().unwrap(),
            live.job_name.as_deref().unwrap(),
            live.job_path.as_deref(),
        )
        .await
        .unwrap();
    assert_eq!(p.state, PrinterState::Idle);
    assert_eq!(p.needs_clearing, Some(true));
    assert_eq!(f.commands().await, vec!["/printer/print/cancel"]);
    assert!(!f
        .shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .any(|(method, _, _)| method == "DELETE"));
}

#[tokio::test]
async fn uploaded_ready_file_is_cleaned_on_cancel_or_forget() {
    for forget in [false, true] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        f.shared.machine.lock().await.block = Some("/server/files/upload".into());
        let start = tokio::spawn({
            let b = b.clone();
            async move { b.start("one").await }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.shared.entered.notified(),
        )
        .await
        .unwrap();
        f.shared.machine.lock().await.settings["extruder"]["nozzle_diameter"] = json!(0.6);
        f.shared.release.notify_one();
        assert!(start.await.unwrap().is_err());
        assert!(!f.shared.machine.lock().await.name.is_empty());
        assert_eq!(b.job("one").await.unwrap().state, JobState::Ready);
        if forget {
            b.forget("one").await.unwrap();
        } else {
            assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
        }
        let m = f.shared.machine.lock().await;
        assert!(m.name.is_empty());
        assert_eq!(
            m.records
                .iter()
                .filter(|(method, path, _)| method == "DELETE"
                    && path.starts_with("/server/files/gcodes/dutchman-"))
                .count(),
            1
        );
        drop(m);
        assert!(f.commands().await.is_empty());
    }
}
