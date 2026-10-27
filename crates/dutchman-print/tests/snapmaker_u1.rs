//! Snapmaker U1 toolhead policy exercised only against test-owned loopback servers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
#[path = "helpers/controls.rs"]
mod controls;
#[path = "helpers/moonraker.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{body::Body, http::StatusCode, Router};
use dutchman_print::{
    backend::{Backend, JobState, Options, PrinterState},
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

const SECOND_TOOL: &str = "; nozzle_diameter = 0.4,0.4,0.6,0.4\nT1\nG1 X0\n";

fn input(dir: &Path, gcode: &str) -> PathBuf {
    std::fs::write(dir.join("output.gcode"), gcode).unwrap();
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
async fn u1() -> Fake {
    let f = Fake::new().await;
    {
        let mut m = f.shared.machine.lock().await;
        m.toolheads = 4;
        m.settings = json!({"mcu":{"serial":"synthetic"},
            "extruder":{"nozzle_diameter":0.4},"extruder1":{"nozzle_diameter":0.4},
            "extruder2":{"nozzle_diameter":0.6},"extruder3":{"nozzle_diameter":0.4}});
    }
    f
}
fn config(f: &Fake) -> dutchman_print::config::Config {
    let mut c = f.config();
    c.printers[0].kind = Some("snapmaker-u1".into());
    c.printers[0].model = "Snapmaker U1".into();
    c
}
async fn error(f: &Fake, gcode: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = input(dir.path(), gcode);
    let b = Printers::new(config(f)).unwrap();
    b.prepare("one", &path, "test", Options::default())
        .await
        .unwrap_err()
        .to_string()
}

#[tokio::test]
async fn status_reports_the_active_toolhead_and_plain_moonraker_still_refuses_tools() {
    let f = u1().await;
    f.shared.machine.lock().await.active_tool = "extruder2".into();
    let c = Client::new(config(&f).printers.remove(0)).unwrap();
    let live = c.snapshot().await.unwrap();
    assert_eq!(live.printer.state, PrinterState::Idle);
    assert_eq!(live.printer.nozzle_temperature, Some(202.0));
    for tool in ["extruder1", "extruder2", "extruder3"] {
        assert!(live.objects.get(tool).is_some(), "{tool}");
    }
    let info = c.review(&live).await.unwrap();
    assert_eq!(info["info"], json!({"tools":[0.4, 0.4, 0.6, 0.4]}));

    let dir = tempfile::tempdir().unwrap();
    let path = input(dir.path(), SECOND_TOOL);
    let plain = Printers::new(f.config()).unwrap();
    assert!(plain
        .prepare("plain", &path, "test", Options::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("slot"));
    let b = Printers::new(config(&f)).unwrap();
    b.prepare("one", &path, "test", Options::default())
        .await
        .unwrap();
    let path = input(
        dir.path(),
        "; nozzle_diameter = 0.4\n; T2 in a comment\nT3 ; select tool\nM104 T2 S0\nG1 X0\n",
    );
    b.prepare("two", &path, "test", Options::default())
        .await
        .unwrap();
    assert!(f.commands().await.is_empty());
}

#[tokio::test]
async fn sliced_tools_must_exist_match_their_nozzle_and_use_one_material() {
    let f = u1().await;
    for (gcode, expected) in [
        (
            "; nozzle_diameter = 0.4,0.4,0.6,0.4\nT0\nG1 X0\nT1\nG1 X1\n",
            vec!["T0, T1", "material"],
        ),
        (
            "; nozzle_diameter = 0.4,0.4,0.6,0.4\nT5\n",
            vec!["T5", "4 tools"],
        ),
        (
            "; nozzle_diameter = 0.4,0.4,0.4,0.4\nT2\n",
            vec!["T2", "0.6", "0.4"],
        ),
        ("; nozzle_diameter = 0.4\nG1 X0\n", vec!["no tool"]),
        (
            "; nozzle_diameter = 0.4\nT0\nACTIVATE_EXTRUDER EXTRUDER=extruder1\n",
            vec!["ACTIVATE_EXTRUDER"],
        ),
        ("T0\nG1 X0\n", vec!["nozzle"]),
        ("; nozzle_diameter = 0.4,0.4\nT3\n", vec!["T3"]),
        (
            "; nozzle_diameter = 0.4,0.4,0.6,0.4\n; nozzle_diameter = 0.6,0.6,0.6,0.6\nT1\n",
            vec!["disagree"],
        ),
        ("; nozzle_diameter = 0.4,x\nT0\n", vec!["nozzle"]),
    ] {
        let message = error(&f, gcode).await;
        for part in expected {
            assert!(message.contains(part), "{gcode:?}: {message}");
        }
    }
    assert!(f.commands().await.is_empty());
}

#[tokio::test]
async fn unknown_or_incomplete_toolheads_are_refused() {
    for mode in ["gap", "nozzle", "active", "stepper"] {
        let f = u1().await;
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "gap" => {
                    m.settings.as_object_mut().unwrap().remove("extruder1");
                }
                "nozzle" => m.settings["extruder3"] = json!({}),
                "active" => m.active_tool = "extruder7".into(),
                "stepper" => m.settings["extruder_stepper belt"] = json!({}),
                _ => unreachable!(),
            }
        }
        let message = error(&f, SECOND_TOOL).await;
        assert!(
            message.to_lowercase().contains("toolhead"),
            "{mode}: {message}"
        );
        assert!(f.commands().await.is_empty());
    }
}

#[tokio::test]
async fn changed_tools_after_review_block_start() {
    for mode in ["used", "other", "removed"] {
        let f = u1().await;
        let dir = tempfile::tempdir().unwrap();
        let path = input(dir.path(), SECOND_TOOL);
        let b = Safety::new(Arc::new(Printers::new(config(&f)).unwrap()));
        b.prepare("one", &path, "test", Options::default())
            .await
            .unwrap();
        b.clear("test").await.unwrap();
        {
            let mut m = f.shared.machine.lock().await;
            match mode {
                "used" => m.settings["extruder1"]["nozzle_diameter"] = json!(0.6),
                "other" => m.settings["extruder3"]["nozzle_diameter"] = json!(0.8),
                "removed" => {
                    m.settings.as_object_mut().unwrap().remove("extruder3");
                    m.toolheads = 3;
                }
                _ => unreachable!(),
            }
        }
        let message = b.start("one").await.unwrap_err().to_string();
        if mode == "used" {
            assert!(
                message.contains("T1") && message.contains("0.6") && message.contains("0.4"),
                "{message}"
            );
        }
        assert!(f.commands().await.is_empty(), "{mode}");
        assert_eq!(b.job("one").await.unwrap().state, JobState::Ready);
    }
}

#[tokio::test]
async fn controls_and_stop_require_fresh_confirmation() {
    use tower::ServiceExt as _;
    let f = u1().await;
    let dir = tempfile::tempdir().unwrap();
    let raw = Arc::new(Printers::new(config(&f)).unwrap());
    let safety = Safety::new(raw.clone());
    for id in ["one", "two"] {
        let sub = dir.path().join(id);
        std::fs::create_dir(&sub).unwrap();
        safety
            .prepare(id, &input(&sub, SECOND_TOOL), "test", Options::default())
            .await
            .unwrap();
    }
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.path().join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    safety
        .save(&settings.storage.data_dir.join("safety.json"))
        .await
        .unwrap();
    let app = dutchman_print::router(raw.clone(), settings);
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
    api_action(&app, "clear", None, 1).await;
    f.shared.machine.lock().await.settings["extruder1"]["nozzle_diameter"] = json!(0.6);
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
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
    assert!(message.contains("T1") && message.contains("0.6") && message.contains("0.4"));
    assert!(f.commands().await.is_empty());
    f.shared.machine.lock().await.settings["extruder1"]["nozzle_diameter"] = json!(0.4);
    api_action(&app, "start", Some("one"), 2).await;
    api_action(&app, "pause", None, 3).await;
    api_action(&app, "resume", None, 4).await;
    let job = api_action(&app, "cancel", Some("one"), 5).await;
    assert_eq!(job["state"], "cancelled");

    api_action(&app, "clear", None, 6).await;
    api_action(&app, "start", Some("two"), 7).await;
    let stopped = api_action(&app, "stop", None, 8).await;
    assert_eq!(stopped["state"], "idle");
    assert_eq!(stopped["needs_clearing"], true);
    assert_eq!(raw.job("two").await.unwrap().state, JobState::Cancelled);
    assert_eq!(
        f.commands().await,
        vec![
            "/printer/print/start",
            "/printer/print/pause",
            "/printer/print/resume",
            "/printer/print/cancel",
            "/printer/print/start",
            "/printer/print/cancel"
        ]
    );
    let m = f.shared.machine.lock().await;
    assert!(m.bytes.is_empty());
    let uploads = m
        .records
        .iter()
        .filter(|(_, p, _)| p == "/server/files/upload")
        .map(|(_, _, body)| String::from_utf8_lossy(body).into_owned())
        .collect::<Vec<_>>();
    assert_eq!(uploads.len(), 2);
    assert!(uploads.iter().all(|u| u.contains("\nT1\n")));
}

async fn api_action(app: &Router, action: &str, job: Option<&str>, token: u64) -> Value {
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
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[test]
fn config_uses_moonraker_validation() {
    let text = "[backend]\nkind='printers'\n[slicer]\nkind='prusaslicer'\n[[printers]]\nkind='snapmaker-u1'\nid='u1'\nname='U1'\nmodel='Snapmaker U1'\nurl='http://127.0.0.1:7125'\nprinter_profile='M'\nprint_profile='D'\nfilament_profile='P'";
    let config: dutchman_print::config::Config = toml::from_str(text).unwrap();
    assert!(Printers::new(config).is_ok());
    for (extra, expected) in [
        ("nozzle_diameter=0.4", "only for Duet"),
        ("username='maker'", "only api_key"),
    ] {
        let config: dutchman_print::config::Config =
            toml::from_str(&text.replace("url=", &format!("{extra}\nurl="))).unwrap();
        assert!(
            Printers::new(config).err().unwrap().contains(expected),
            "{extra}"
        );
    }
}

#[tokio::test]
async fn api_reports_configured_controls() {
    let f = u1().await;
    let (_dir, app) = controls::app(config(&f));
    controls::listed(&app, controls::ALL).await;
}

#[tokio::test]
async fn uncommissioned_resume_is_omitted_and_refused_without_reads() {
    let f = u1().await;
    let mut config = config(&f);
    config.printers[0].resume_commissioned = false;
    let (_dir, app) = controls::app(config);
    controls::unsupported(&app, "test", &["resume"]).await;
    assert!(f.shared.machine.lock().await.records.is_empty());
    controls::listed(&app, &["start", "pause", "cancel", "stop", "clear"]).await;
}
