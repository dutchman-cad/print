//! LAN monitoring uses only this test's loopback broker and synthetic certificates.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "helpers/bambu.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use dutchman_print::{
    backend::{Backend, Options, PrinterState},
    config::Config,
    printers::Printers,
    safety::Safety,
};
use fake::Fake;
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread")]
async fn cli_prepare_control_refusals_and_confirmation() {
    let fake = Fake::new().await;
    let temp = tempfile::Builder::new()
        .prefix("bambu job= ")
        .tempdir()
        .unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let config = format!(
        r#"
[backend]
kind = "printers"
[slicer]
kind = "orcaslicer"
path = {:?}
datadir = {:?}
[[printers]]
kind = "bambu"
id = "bambu"
name = "Test Bambu"
model = "Synthetic"
host = "127.0.0.1"
port = {}
serial = "TESTSERIAL"
access_code = "test-access"
cert_sha256 = "{}"
printer_profile = "Machine"
print_profile = "Draft"
filament_profile = "PLA"
"#,
        env!("CARGO_BIN_EXE_dutchman-fake-slicer"),
        dir.join("presets").to_str().unwrap(),
        fake.port,
        dutchman_print::safety::hash(fake.cert.as_ref())
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, &config).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // Enrollment works before trust is configured and never sends MQTT credentials.
    let enrollment = config
        .lines()
        .filter(|line| !line.starts_with("cert_sha256"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&config_path, &enrollment).unwrap();
    for json in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dutchman-print"));
        command
            .arg("--config")
            .arg(&config_path)
            .args(["fingerprint", "bambu"]);
        if json {
            command.arg("--json");
        }
        let result = command.output().await.unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains(&dutchman_print::safety::hash(fake.cert.as_ref())));
        assert!(text.contains("cert_sha256"));
        assert!(!text.contains("test-access"));
        if json {
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(!value["subject"].as_str().unwrap().is_empty());
            assert!(!value["issuer"].as_str().unwrap().is_empty());
            assert!(!value["not_before"].as_str().unwrap().is_empty());
            assert!(!value["not_after"].as_str().unwrap().is_empty());
        }
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), enrollment);
    }
    assert_eq!(fake.connections.load(Ordering::SeqCst), 2);
    assert_eq!(fake.credentials.load(Ordering::SeqCst), 0);
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
    assert!(Config::load(&config_path).is_err());
    std::fs::write(&config_path, &config).unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
        .arg("--config")
        .arg(&config_path)
        .arg("--data-dir")
        .arg(dir.join("cli-data"))
        .args(["printers", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let reports: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(reports[0]["state"], "idle");
    assert!(!String::from_utf8_lossy(&result.stdout).contains("test-access"));
    Fake::wait(&fake.disconnects, 1).await;
    assert_eq!(fake.connections.load(Ordering::SeqCst), 3);

    let backend = Arc::new(Printers::new(toml::from_str::<Config>(&config).unwrap()).unwrap());
    let safety = Safety::new(backend.clone());
    assert_eq!(
        safety.printers().await.unwrap()[0].state,
        PrinterState::Idle
    );
    safety.clear("bambu").await.unwrap();
    // A terminal report followed by idle between polls still consumes clearance.
    fake.reports
        .send(json!({"print":{"gcode_state":"FINISH"}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    fake.reports
        .send(json!({"print":{"gcode_state":"IDLE"}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        safety.printers().await.unwrap()[0].needs_clearing,
        Some(true)
    );
    safety.clear("bambu").await.unwrap();
    // A second completion without an intervening status poll must also remove clearance.
    fake.reports
        .send(json!({"print":{"gcode_state":"FAILED"}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    fake.reports
        .send(json!({"print":{"gcode_state":"IDLE"}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        safety.printers().await.unwrap()[0].needs_clearing,
        Some(true)
    );
    for (category, name) in [
        ("machine", "Machine"),
        ("process", "Draft"),
        ("filament", "PLA"),
    ] {
        let path = dir.join("presets/user/default").join(category);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("preset.json"), json!({"name":name}).to_string()).unwrap();
    }
    std::fs::create_dir(dir.join("nested")).unwrap();
    // Exercise canonicalization on Linux as well, without Windows symlink privileges.
    let alias = temp.path().join("nested").join("..");
    let input = slicer_fixture::input(&alias, |input| {
        let folder = input.parent().unwrap();
        let path_text = |s: &str| folder.join(s).display().to_string();
        vec![
            format!("--datadir={}", path_text("presets")),
            format!(
                "--load-filaments={}",
                path_text("presets/user/default/filament/preset.json")
            ),
            format!(
                "--load-settings={};{}",
                path_text("presets/user/default/machine/preset.json"),
                path_text("presets/user/default/process/preset.json")
            ),
            "--slice=0".into(),
            "--export-3mf=sliced.3mf".into(),
            format!("--outputdir={}", folder.display()),
            input.to_str().unwrap().into(),
        ]
    });
    assert_ne!(input, input.canonicalize().unwrap());
    let estimate = safety
        .prepare("job", &input, "bambu", Options::default())
        .await
        .unwrap();
    assert_eq!(estimate.seconds, Some(123));
    let mut archive =
        zip::ZipArchive::new(std::fs::File::open(dir.join("sliced.3mf")).unwrap()).unwrap();
    assert!(archive.by_name("Metadata/plate_1.gcode").is_ok());
    for result in [
        safety.start("job").await,
        safety.cancel("job").await,
        safety.pause("bambu").await,
        safety.resume("bambu").await,
    ] {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not supported yet"));
    }
    let app = dutchman_print::router(backend, dutchman_print::ServerConfig::default());
    for (i, path) in [
        "/api/v1/jobs/job/start",
        "/api/v1/jobs/job/cancel",
        "/api/v1/printers/bambu/pause",
        "/api/v1/printers/bambu/resume",
    ]
    .into_iter()
    .enumerate()
    {
        let response = app
            .clone()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("Plugin-Confirmation", format!("{i:064x}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("not supported yet"));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 2);
    // Read all fixture metadata so the same helper stays warning-free in both test targets.
    assert!(!fake.pem.is_empty());
    assert_ne!(fake.port, 0);
    assert_eq!(fake.credentials.load(Ordering::SeqCst), 2);
}
