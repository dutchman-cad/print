//! Duet control policy exercised only against test-owned loopback servers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
#[path = "helpers/controls.rs"]
mod controls;
#[path = "helpers/duet.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{body::Body, http::StatusCode, Router};
use dutchman_print::{
    backend::{Backend, Error, JobState, Options, PrinterState},
    duet::Client,
    printers::Printers,
    safety::Safety,
};
use fake::Fake;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

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
fn staged() -> String {
    format!(
        "0:/gcodes/dutchman-{}.gcode",
        dutchman_print::safety::hash(b"one")
    )
}
fn start_command() -> String {
    format!("M32 \"{}\"", staged())
}
async fn deletes(f: &Fake) -> usize {
    f.shared
        .machine
        .lock()
        .await
        .records
        .iter()
        .filter(|(path, _)| path == "/rr_delete")
        .count()
}

#[tokio::test]
async fn session_password_errors_and_reconnect_after_401() {
    let f = Fake::new().await;
    let c = Client::new(f.config().printers.remove(0)).unwrap();
    assert_eq!(
        c.snapshot().await.unwrap().printer.state,
        PrinterState::Idle
    );
    c.snapshot().await.unwrap();
    // The reported session timeout is honoured: one login serves later reads.
    assert_eq!(f.shared.machine.lock().await.connects, 1);
    // A dropped session returns 401; reads log in again and continue.
    f.shared.machine.lock().await.keys.clear();
    c.snapshot().await.unwrap();
    assert_eq!(f.shared.machine.lock().await.connects, 2);
    // A short idle timeout causes a new login before the next request.
    f.shared.machine.lock().await.session_timeout = 1;
    f.shared.machine.lock().await.keys.clear();
    c.snapshot().await.unwrap();
    let before = f.shared.machine.lock().await.connects;
    let begin = std::time::Instant::now();
    while begin.elapsed() < Duration::from_millis(5) {
        tokio::task::yield_now().await;
    }
    c.model("state").await.unwrap();
    assert_eq!(f.shared.machine.lock().await.connects, before + 1);

    f.shared.machine.lock().await.password = Some("other".into());
    f.shared.machine.lock().await.keys.clear();
    assert!(matches!(c.snapshot().await, Err(Error::Authentication)));
    let public =
        serde_json::to_string(&Printers::new(f.config()).unwrap().printers().await.unwrap())
            .unwrap();
    assert!(!public.contains("secret") && !public.contains(&f.url));
    assert!(public.contains("Authentication failed. Check the Duet password."));

    f.shared.machine.lock().await.sessions_full = true;
    let error = c.snapshot().await.err().unwrap().to_string();
    assert!(error.contains("no free sessions"), "{error}");
    let public =
        serde_json::to_string(&Printers::new(f.config()).unwrap().printers().await.unwrap())
            .unwrap();
    assert!(public.contains("no free sessions") && public.contains("offline"));
}

#[tokio::test]
async fn object_model_states_progress_and_temperatures() {
    let f = Fake::new().await;
    let c = Client::new(f.config().printers.remove(0)).unwrap();
    for (status, expected) in [
        ("idle", PrinterState::Idle),
        ("processing", PrinterState::Printing),
        ("pausing", PrinterState::Printing),
        ("resuming", PrinterState::Printing),
        ("cancelling", PrinterState::Printing),
        ("changingTool", PrinterState::Printing),
        ("paused", PrinterState::Paused),
        ("busy", PrinterState::Busy),
        ("starting", PrinterState::Busy),
        ("updating", PrinterState::Busy),
        ("simulating", PrinterState::Busy),
        ("halted", PrinterState::Error),
        ("off", PrinterState::Offline),
        ("disconnected", PrinterState::Offline),
        ("future", PrinterState::Error),
    ] {
        f.shared.machine.lock().await.status = status.into();
        let live = c.snapshot().await.unwrap();
        assert_eq!(live.printer.state, expected, "{status}");
        if status == "future" {
            assert_eq!(
                live.printer.detail.as_deref(),
                Some("Unknown Duet state. Read fresh status.")
            );
            assert!(dutchman_print::duet::ready(&live).is_err());
        } else {
            assert_eq!(live.printer.detail.as_deref(), Some(status));
            assert_eq!(dutchman_print::duet::ready(&live).is_ok(), status == "idle");
        }
    }
    f.external("processing").await;
    let p = c.snapshot().await.unwrap().printer;
    assert_eq!(p.job_id.as_deref(), Some("0:/gcodes/external.gcode"));
    assert_eq!(p.job_name.as_deref(), Some("external.gcode"));
    assert_eq!(p.progress, Some(0.25));
    assert_eq!(p.seconds_left, Some(321));
    assert_eq!(p.nozzle_temperature, Some(201.5));
    assert_eq!(p.bed_temperature, Some(59.0));
    {
        let mut m = f.shared.machine.lock().await;
        m.current_tool = -1;
        m.times_left = Value::Null;
    }
    let p = c.snapshot().await.unwrap().printer;
    assert_eq!(p.nozzle_temperature, None);
    assert_eq!(p.seconds_left, None);
    // Tools are found by number and use their own heaters, never an array position.
    {
        let mut m = f.shared.machine.lock().await;
        m.current_tool = 1;
        m.tools = json!([{"number":0,"heaters":[2],"extruders":[1]},{"number":1,"heaters":[1],"extruders":[0]}]);
        m.heat["heaters"]
            .as_array_mut()
            .unwrap()
            .push(json!({"current":150.0}));
    }
    assert_eq!(
        c.snapshot().await.unwrap().printer.nozzle_temperature,
        Some(201.5)
    );
    f.shared.machine.lock().await.current_tool = 0;
    assert_eq!(
        c.snapshot().await.unwrap().printer.nozzle_temperature,
        Some(150.0)
    );
}

#[tokio::test]
async fn stage_start_control_finish_cleanup_and_clearance() {
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
    assert_eq!(
        f.commands().await,
        vec![start_command(), "M25".into(), "M24".into()]
    );
    {
        let m = f.shared.machine.lock().await;
        let upload = m
            .records
            .iter()
            .position(|(p, _)| p == "/rr_upload")
            .unwrap();
        let start = m
            .records
            .iter()
            .position(|(p, q)| p == "/rr_gcode" && q.contains("M32"))
            .unwrap();
        assert!(m.records[..upload]
            .iter()
            .any(|(p, q)| p == "/rr_model" && q.contains("key=state")));
        assert!(m.records[upload + 1..start]
            .iter()
            .any(|(p, _)| p == "/rr_filelist"));
        assert!(m
            .files
            .contains_key(staged().strip_prefix("0:/gcodes/").unwrap()));
    }
    // Standalone firmware does not report how a file ended.
    {
        let mut m = f.shared.machine.lock().await;
        m.status = "idle".into();
        m.last_file = m.file.take();
    }
    assert_eq!(b.job("one").await.unwrap().state, JobState::Ended);
    assert!(f.shared.machine.lock().await.files.is_empty());
    assert_eq!(deletes(&f).await, 1);
    assert_eq!(b.job("one").await.unwrap().state, JobState::Ended);
    assert_eq!(deletes(&f).await, 1);
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(true));
    b.clear("test").await.unwrap();
    assert_eq!(b.printers().await.unwrap()[0].needs_clearing, Some(false));
}

#[tokio::test]
async fn cancel_pauses_first_then_stops_once_and_cleans_up() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    assert_eq!(b.cancel("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(
        f.commands().await,
        vec![start_command(), "M25".into(), "M0".into()]
    );
    assert!(f.shared.machine.lock().await.files.is_empty());
    assert_eq!(b.job("one").await.unwrap().state, JobState::Cancelled);
}

#[tokio::test]
async fn unsafe_start_cases_send_no_start() {
    for mode in [
        "busy",
        "starting",
        "simulating",
        "updating",
        "halted",
        "off",
        "unknown",
        "input",
        "output",
        "firmware",
        "tools",
        "existing",
    ] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "busy" | "starting" | "simulating" | "updating" | "halted" | "off" => {
                    m.status = mode.into()
                }
                "unknown" => m.status = "future".into(),
                "input" => std::fs::write(dir.path().join("input.3mf"), b"changed").unwrap(),
                "output" => std::fs::write(dir.path().join("sliced.gcode"), b"G28\n").unwrap(),
                "firmware" => m.firmware = "3.6.0".into(),
                "tools" => {
                    m.tools = json!([{"number":0,"heaters":[1],"extruders":[0]},{"number":1,"heaters":[1],"extruders":[1]}])
                }
                "existing" => {
                    let name = staged().strip_prefix("0:/gcodes/").unwrap().to_owned();
                    m.files.insert(name, b"other".to_vec());
                }
                _ => unreachable!(),
            }
        }
        assert!(b.start("one").await.is_err(), "{mode}");
        assert!(f.commands().await.is_empty(), "{mode}");
        let m = f.shared.machine.lock().await;
        assert!(!m.records.iter().any(|(p, _)| p == "/rr_upload"), "{mode}");
        if mode == "existing" {
            assert_eq!(m.files.values().next().unwrap(), b"other");
        }
    }
}

#[tokio::test]
async fn failed_and_uncertain_uploads_are_deleted_without_start() {
    for mode in ["err", "lost", "size"] {
        let f = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let b = backend(&f, dir.path()).await;
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            // Several unrelated files make the listing span pages.
            for name in ["a.gcode", "b.gcode", "z.gcode"] {
                m.files.insert(name.into(), b"G1".to_vec());
            }
            match mode {
                "err" => m.upload_err = true,
                "lost" => m.upload_lost = true,
                _ => m.bad_size = true,
            }
        }
        assert!(b.start("one").await.is_err(), "{mode}");
        assert!(f.commands().await.is_empty(), "{mode}");
        assert_eq!(deletes(&f).await, 1, "{mode}");
        let m = f.shared.machine.lock().await;
        assert_eq!(m.files.len(), 3, "{mode}");
        assert!(
            !m.files.keys().any(|n| n.starts_with("dutchman-")),
            "{mode}"
        );
        drop(m);
        assert_eq!(b.job("one").await.unwrap().state, JobState::Ready);
    }
}

#[tokio::test]
async fn lost_start_reply_is_reconciled_and_never_resent() {
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
    assert_eq!(f.commands().await, vec![start_command()]);
}

#[tokio::test]
async fn expired_session_during_a_command_is_not_resent() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    f.shared.machine.lock().await.expire_on_gcode = true;
    let error = b.pause("test").await.unwrap_err().to_string();
    assert!(error.contains("session expired"), "{error}");
    assert_eq!(f.commands().await, vec![start_command()]);
    assert_eq!(b.job("one").await.unwrap().state, JobState::Printing);
    assert_eq!(b.pause("test").await.unwrap().state, JobState::Paused);
    assert_eq!(f.commands().await, vec![start_command(), "M25".into()]);
}

#[tokio::test]
async fn delayed_transitions_are_polled_and_each_command_sent_once() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    f.shared.machine.lock().await.delay_reads = 5;
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
            start_command(),
            "M25".into(),
            "M24".into(),
            "M25".into(),
            "M0".into()
        ]
    );
}

#[tokio::test]
async fn controls_require_the_tracked_live_file() {
    let f = Fake::new().await;
    let b = Safety::new(Arc::new(Printers::new(f.config()).unwrap()));
    f.external("processing").await;
    assert!(b.pause("test").await.is_err());
    f.shared.machine.lock().await.status = "paused".into();
    assert!(b.resume("test").await.is_err());
    assert!(f.commands().await.is_empty());

    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    {
        let mut m = f.shared.machine.lock().await;
        m.file = Some("0:/gcodes/external.gcode".into());
    }
    assert!(b.pause("test").await.is_err());
    assert!(b
        .review_action("cancel", "test", Some("one"))
        .await
        .is_err());
    assert_eq!(f.commands().await, vec![start_command()]);
}

#[tokio::test]
async fn printer_stop_external_job_pauses_then_stops_and_leaves_its_file() {
    for paused in [false, true] {
        let f = Fake::new().await;
        let b = Safety::new(Arc::new(Printers::new(f.config()).unwrap()));
        f.external(if paused { "paused" } else { "processing" })
            .await;
        let live = b.printer("test").await.unwrap();
        assert!(b
            .stop("test", "0:/gcodes/other.gcode", "other.gcode", None)
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
        assert_eq!(deletes(&f).await, 0);
    }
}

#[tokio::test]
async fn stop_overtakes_a_pending_pause_of_the_tracked_job() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    f.shared.machine.lock().await.hold_controls = true;
    let pause = tokio::spawn({
        let b = b.clone();
        async move { b.pause("test").await }
    });
    timeout(Duration::from_secs(5), f.shared.polled.notified())
        .await
        .unwrap();
    assert!(!pause.is_finished());
    let live = b.printer("test").await.unwrap();
    assert_eq!(live.detail.as_deref(), Some("pausing"));
    let stop = tokio::spawn({
        let b = b.clone();
        async move {
            b.stop(
                "test",
                live.job_id.as_deref().unwrap(),
                live.job_name.as_deref().unwrap(),
                live.job_path.as_deref(),
            )
            .await
        }
    });
    timeout(Duration::from_secs(5), f.shared.polled.notified())
        .await
        .unwrap();
    f.shared.machine.lock().await.status = "paused".into();
    assert_eq!(
        timeout(Duration::from_secs(10), stop)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state,
        PrinterState::Idle
    );
    let paused = timeout(Duration::from_secs(10), pause)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        paused.state,
        JobState::Paused | JobState::Cancelled
    ));
    assert_eq!(b.job("one").await.unwrap().state, JobState::Cancelled);
    assert_eq!(
        f.commands().await,
        vec![start_command(), "M25".into(), "M0".into()]
    );
    assert!(f.shared.machine.lock().await.files.is_empty());
}

#[tokio::test]
async fn halted_printer_fails_the_tracked_job() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&f, dir.path()).await;
    b.clear("test").await.unwrap();
    b.start("one").await.unwrap();
    f.shared.machine.lock().await.status = "halted".into();
    assert_eq!(b.job("one").await.unwrap().state, JobState::Failed);
    // The staged file stays until the printer is idle again.
    assert_eq!(deletes(&f).await, 0);
    {
        let mut m = f.shared.machine.lock().await;
        m.status = "idle".into();
        m.last_file = m.file.take();
    }
    assert_eq!(b.job("one").await.unwrap().state, JobState::Failed);
    assert!(f.shared.machine.lock().await.files.is_empty());
}

#[tokio::test]
async fn configured_nozzle_must_match_slicer_and_be_set() {
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

#[test]
fn config_validates_urls_password_and_nozzle_without_echoing_secrets() {
    let mut p = dutchman_print::config::Printer {
        kind: Some("duet".into()),
        ..Default::default()
    };
    for url in ["http://127.0.0.1", "https://127.0.0.1/gateway/path/"] {
        p.url = Some(url.into());
        assert!(dutchman_print::duet::validate(&p).is_ok());
    }
    for url in [
        "file:///secret",
        "http://maker:secret@127.0.0.1",
        "http://127.0.0.1/?secret",
        "https://127.0.0.1/../secret",
        "http://127.0.0.1:0",
    ] {
        p.url = Some(url.into());
        assert!(!dutchman_print::duet::validate(&p)
            .unwrap_err()
            .contains("secret"));
    }
    p.url = Some("http://127.0.0.1".into());
    for password in ["bad\nsecret", ""] {
        p.password = Some(password.into());
        assert!(!dutchman_print::duet::validate(&p)
            .unwrap_err()
            .contains("secret"));
    }
    p.password = Some("p&ss w=rd".into());
    for nozzle in [0.0, -0.4, f64::NAN, 11.0] {
        p.nozzle_diameter = Some(nozzle);
        assert!(dutchman_print::duet::validate(&p).is_err());
    }
    p.nozzle_diameter = Some(0.4);
    assert!(dutchman_print::duet::validate(&p).is_ok());
    p.api_key = Some("secret".into());
    assert!(dutchman_print::duet::validate(&p).is_err());

    let text = "[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='moonraker'\nid='test'\nname='Test'\nmodel='Test'\nurl='http://127.0.0.1'\nnozzle_diameter=0.4\nprinter_profile='M'\nprint_profile='D'\nfilament_profile='P'";
    let config: dutchman_print::config::Config = toml::from_str(text).unwrap();
    assert!(Printers::new(config)
        .err()
        .unwrap()
        .contains("only for Duet"));
}

#[tokio::test]
async fn special_characters_in_the_password_are_encoded() {
    let f = Fake::new().await;
    f.shared.machine.lock().await.password = Some("p&ss w=rd%#".into());
    let mut config = f.config().printers.remove(0);
    config.password = Some("p&ss w=rd%#".into());
    let c = Client::new(config).unwrap();
    assert_eq!(
        c.snapshot().await.unwrap().printer.state,
        PrinterState::Idle
    );
}

#[tokio::test]
async fn api_actions_require_fresh_confirmation_before_commands() {
    use axum::http::Request;
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
        "/api/v1/printers/test/stop",
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
    api_action(&app, "clear", None, 1).await;
    f.shared.machine.lock().await.firmware = "3.6.0".into();
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
    assert!(f.commands().await.is_empty());
    f.shared.machine.lock().await.firmware = "3.5.0".into();
    api_action(&app, "start", Some("one"), 2).await;
    api_action(&app, "pause", None, 3).await;
    api_action(&app, "resume", None, 4).await;
    api_action(&app, "stop", None, 5).await;
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
async fn api_reports_configured_controls() {
    let f = Fake::new().await;
    let (_dir, app) = controls::app(f.config());
    controls::listed(&app, controls::ALL).await;
}

#[tokio::test]
async fn shutdown_releases_existing_key_once_without_machine_actions() {
    let f = Fake::new().await;
    let backend = Safety::new(Arc::new(Printers::new(f.config()).unwrap()));
    backend.printers().await.unwrap();
    backend.shutdown().await.unwrap();
    backend.shutdown().await.unwrap();
    let machine = f.shared.machine.lock().await;
    assert_eq!(
        machine
            .records
            .iter()
            .filter(|(path, _)| path == "/rr_disconnect")
            .count(),
        1
    );
    assert!(machine.keys.is_empty());
    assert!(machine.commands.is_empty());
    assert_eq!(machine.connects, 1);
}

#[tokio::test]
async fn shutdown_deadline_covers_a_printer_that_never_replies() {
    let f = Fake::new().await;
    let backend = Printers::new(f.config()).unwrap();
    backend.printers().await.unwrap();
    f.shared.machine.lock().await.block = Some("/rr_disconnect".into());
    let started = std::time::Instant::now();
    let _ = backend.shutdown().await;
    assert!(started.elapsed() < Duration::from_millis(3500));
    assert!(f.commands().await.is_empty());
}

#[tokio::test]
async fn shutdown_of_unreachable_printer_finishes_within_deadline() {
    let f = Fake::new().await;
    let backend = Printers::new(f.config()).unwrap();
    backend.printers().await.unwrap();
    drop(f);
    let started = std::time::Instant::now();
    let _ = backend.shutdown().await;
    assert!(started.elapsed() < Duration::from_millis(3500));
}

#[tokio::test]
async fn shutdown_does_not_wait_for_an_outstanding_status_poll() {
    let f = Fake::new().await;
    let backend = Arc::new(Printers::new(f.config()).unwrap());
    backend.printers().await.unwrap();
    f.shared.machine.lock().await.block = Some("/rr_model".into());
    let pending = tokio::spawn({
        let backend = backend.clone();
        async move { backend.printers().await }
    });
    timeout(Duration::from_secs(5), f.shared.entered.notified())
        .await
        .unwrap();
    timeout(Duration::from_secs(1), backend.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.shared
            .machine
            .lock()
            .await
            .records
            .iter()
            .filter(|(path, _)| path == "/rr_disconnect")
            .count(),
        1
    );
    assert!(f.commands().await.is_empty());
    f.shared.release.notify_one();
    pending.await.unwrap().unwrap();
}

#[tokio::test]
async fn cli_saves_state_and_keeps_lock_until_disconnect_finishes() {
    let f = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!(
        "[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='duet'\nid='test'\nname='Test'\nmodel='Synthetic'\nprinter_profile='Machine'\nprint_profile='Draft'\nfilament_profile='PLA'\nurl='{}'\npassword='secret'\nnozzle_diameter=0.4\n", f.url
    )).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let data = dir.path().join("data");
    f.shared.machine.lock().await.block = Some("/rr_disconnect".into());
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dutchman-print"));
    command
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .args(["clear", "test", "--yes"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let first = command.spawn().unwrap();
    timeout(Duration::from_secs(5), f.shared.entered.notified())
        .await
        .unwrap();
    let files: Vec<_> = std::fs::read_dir(&data)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    for ending in ["printers.json", "printers.safety.json"] {
        let path = files
            .iter()
            .find(|path| path.to_string_lossy().ends_with(ending))
            .unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(saved.is_object());
    }
    let second = timeout(Duration::from_secs(1), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("Another CLI command"));
    f.shared.release.notify_one();
    let first = first.wait_with_output().await.unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(f.commands().await.is_empty());
}
