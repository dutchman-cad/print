//! LAN monitoring uses only this test's loopback broker and synthetic certificates.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "helpers/controls.rs"]
mod controls;
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

async fn api_printer(app: &axum::Router) -> dutchman_print::backend::Printer {
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/printers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    serde_json::from_slice::<Vec<dutchman_print::backend::Printer>>(&body)
        .unwrap()
        .remove(0)
}

async fn machine(
    app: &axum::Router,
    action: &str,
    job: Option<&str>,
) -> Result<serde_json::Value, String> {
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/confirmations")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"action":action,"printer":"bambu","job":job}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    if status != StatusCode::CREATED {
        return Err(String::from_utf8_lossy(&body).into());
    }
    let challenge: serde_json::Value = serde_json::from_slice(&body).unwrap();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let path = match job {
        Some(id) => format!("/api/v1/jobs/{id}/{action}"),
        None => format!("/api/v1/printers/bambu/{action}"),
    };
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .header(
                    "Plugin-Confirmation",
                    format!("{:064x}", NEXT.fetch_add(1, Ordering::SeqCst)),
                )
                .body(Body::from(
                    json!({"confirmation":challenge["id"]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    if !status.is_success() {
        return Err(String::from_utf8_lossy(&body).into());
    }
    Ok(if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    })
}

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
    std::fs::create_dir_all(dir.join("api")).unwrap();
    let app = dutchman_print::router(
        backend.clone(),
        dutchman_print::ServerConfig {
            storage: dutchman_print::StorageConfig {
                data_dir: dir.join("api"),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let safety = Safety::new(backend.clone());
    assert_eq!(
        safety.printers().await.unwrap()[0].state,
        PrinterState::Idle
    );
    assert!(machine(&app, "clear", None)
        .await
        .unwrap_err()
        .contains("lan_control"));
    assert!(api_printer(&app).await.controls.is_empty());
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
        assert!(result.unwrap_err().to_string().contains("lan_control"));
    }

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
        assert_eq!(response.status(), StatusCode::CONFLICT);
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
        assert!(String::from_utf8_lossy(&body).contains("lan_control"));
    }
    assert_eq!(fake.requests.load(Ordering::SeqCst), 2);
    // Read all fixture metadata so the same helper stays warning-free in both test targets.
    assert!(!fake.pem.is_empty());
    assert_ne!(fake.port, 0);
    assert_eq!(fake.credentials.load(Ordering::SeqCst), 2);
}

use dutchman_print::config::Printer as PrinterConfig;
use dutchman_print::safety::hash;
#[path = "helpers/ftps.rs"]
mod ftps;
fn copy_config(config: &Config) -> Config {
    Config {
        backend: dutchman_print::config::Backend {
            kind: config.backend.kind.clone(),
            slice: config.backend.slice,
        },
        slicer: config.slicer.clone(),
        slicers: config.slicers.clone(),
        printers: config.printers.clone(),
    }
}
struct Control {
    fake: Fake,
    ftps: ftps::Fake,
    safety: Safety,
    app: axum::Router,
    backend: Arc<Printers>,
    config: Config,
    dir: tempfile::TempDir,
    input: std::path::PathBuf,
}
impl Control {
    async fn new() -> Self {
        Self::with_size(false).await
    }
    async fn with_size(wrong_size: bool) -> Self {
        Self::with_mapping(wrong_size, false).await
    }
    async fn with_mapping(wrong_size: bool, mapped: bool) -> Self {
        Self::with_mode(wrong_size, mapped, "").await
    }
    async fn with_mode(wrong_size: bool, mapped: bool, mode: &str) -> Self {
        let fake = Fake::new().await;
        fake.ready.store(true, Ordering::SeqCst);
        let ftps = ftps::Fake::with_server(
            fake.server.clone(),
            fake.cert.to_vec(),
            fake.pem.clone(),
            false,
            wrong_size,
            0,
        )
        .await;
        let dir = tempfile::Builder::new()
            .prefix("bambu control= ")
            .tempdir()
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        for (category, name) in [
            ("machine", "Machine"),
            ("process", "Draft"),
            ("filament", "PLA"),
        ] {
            let folder = root.join("presets/user/default").join(category);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("preset.json"), json!({"name":name}).to_string()).unwrap();
        }
        let input = slicer_fixture::input(&root, |input| {
            let folder = input.parent().unwrap();
            vec![
                format!("--datadir={}", folder.join("presets").display()),
                format!(
                    "--load-filaments={}",
                    folder
                        .join("presets/user/default/filament/preset.json")
                        .display()
                ),
                format!(
                    "--load-settings={};{}",
                    folder
                        .join("presets/user/default/machine/preset.json")
                        .display(),
                    folder
                        .join("presets/user/default/process/preset.json")
                        .display()
                ),
                "--slice=0".into(),
                "--export-3mf=sliced.3mf".into(),
                format!("--outputdir={}", folder.display()),
                input.to_str().unwrap().into(),
            ]
        });
        let config = Config {
            backend: dutchman_print::config::Backend {
                kind: "printers".into(),
                slice: false,
            },
            slicer: Some(
                toml::from_str(&format!(
                    "kind = 'orcaslicer'\npath = {:?}\ndatadir = {:?}",
                    env!("CARGO_BIN_EXE_dutchman-fake-slicer"),
                    root.join("presets").to_str().unwrap()
                ))
                .unwrap(),
            ),
            slicers: Default::default(),
            printers: vec![dutchman_print::config::Printer {
                kind: Some("bambu".into()),
                id: "bambu".into(),
                name: "Test printer".into(),
                model: "Synthetic".into(),
                host: Some("127.0.0.1".into()),
                port: Some(fake.port),
                ftps_port: ftps.config.ftps_port,
                serial: Some("TESTSERIAL".into()),
                access_code: Some("test-access".into()),
                cert_sha256: Some(dutchman_print::safety::hash(fake.cert.as_ref())),
                lan_control: true,
                ams_mapping: if mapped {
                    if mode == "multi" {
                        [("1".into(), 5), ("2".into(), 0)].into()
                    } else {
                        [("1".into(), 5)].into()
                    }
                } else {
                    Default::default()
                },
                printer_profile: Some("Machine".into()),
                print_profile: Some("Draft".into()),
                filament_profile: Some("PLA".into()),
                ..Default::default()
            }],
        };
        let backend = Arc::new(
            Printers::new(copy_config(&config))
                .unwrap()
                .with_store(root.join("jobs.json")),
        );
        std::fs::write(root.join("mode"), mode).unwrap();
        let safety = Safety::new(backend.clone());
        safety.printers().await.unwrap();
        safety
            .prepare("control", &input, "bambu", Options::default())
            .await
            .unwrap();
        safety.save(&root.join("safety.json")).await.unwrap();
        let app = dutchman_print::router(
            backend.clone(),
            dutchman_print::ServerConfig {
                storage: dutchman_print::StorageConfig {
                    data_dir: root.clone(),
                    min_free: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        Self {
            app,
            config,
            fake,
            ftps,
            safety,
            backend,
            dir,
            input,
        }
    }
    async fn machine(&self, action: &str, job: Option<&str>) -> Result<serde_json::Value, String> {
        machine(&self.app, action, job).await
    }
    async fn status(&self) -> dutchman_print::backend::Printer {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get("/api/v1/printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        serde_json::from_slice::<Vec<dutchman_print::backend::Printer>>(&body)
            .unwrap()
            .remove(0)
    }
    /// Sends a report and waits until the client has processed it. Reports arrive in
    /// order, so a marker progress value seen in status means the report was handled.
    async fn report(&self, report: serde_json::Value) {
        self.fake.reports.send(report).unwrap();
        let marker = if self.status().await.progress == Some(0.37) {
            38
        } else {
            37
        };
        self.fake
            .reports
            .send(json!({"print":{"mc_percent":marker}}))
            .unwrap();
        for _ in 0..500 {
            if self.status().await.progress == Some(f64::from(marker) / 100.0) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the client did not process the report");
    }
    async fn clear(&self) -> Result<(), String> {
        self.machine("clear", None).await.map(|_| ())
    }
    async fn start(&self) -> Result<dutchman_print::backend::Job, String> {
        let value = self.machine("start", Some("control")).await?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
    async fn action(&self, action: &str) -> Result<dutchman_print::backend::Job, String> {
        let value = self
            .machine(action, (action == "cancel").then_some("control"))
            .await?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
    async fn clear_start(&self) -> dutchman_print::backend::Job {
        self.clear().await.unwrap();
        self.start().await.unwrap()
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn control_lifecycle_and_finish_consume_clearance() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    assert_eq!(c.status().await.controls, controls::ALL);
    assert!(c
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("clearance"));
    assert!(c.fake.commands.lock().await.is_empty());
    assert_eq!(c.clear_start().await.state, JobState::Printing);
    assert!(c.ftps.received.load(Ordering::SeqCst) > 0);
    assert_eq!(c.action("pause").await.unwrap().state, JobState::Paused);
    assert_eq!(c.action("resume").await.unwrap().state, JobState::Printing);
    let name = c.backend.job("control").await.unwrap();
    assert_eq!(name.state, JobState::Printing);
    let commands = c.fake.commands.lock().await.clone();
    for (index, command) in ["project_file", "pause", "resume"].iter().enumerate() {
        assert_eq!(commands[index]["print"]["command"], *command);
        assert!(commands[index]["print"]["sequence_id"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok());
    }
    let name = format!("dutchman-{}", dutchman_print::safety::hash(b"control"));
    let archive = std::fs::read(c.dir.path().join("sliced.3mf")).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
    let mut gcode = Vec::new();
    use std::io::Read as _;
    zip.by_name("Metadata/plate_1.gcode")
        .unwrap()
        .read_to_end(&mut gcode)
        .unwrap();
    use md5::{Digest as _, Md5};
    let checksum: String = Md5::digest(&gcode)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        commands[0],
        json!({"print":{"command":"project_file","sequence_id":commands[0]["print"]["sequence_id"],"param":"Metadata/plate_1.gcode","project_id":"0","profile_id":"0","task_id":"0","subtask_id":"0","subtask_name":name,"url":format!("file:///sdcard/{name}.gcode.3mf"),"md5":checksum,"timelapse":false,"bed_type":"auto","bed_leveling":true,"flow_cali":false,"vibration_cali":false,"layer_inspect":false,"use_ams":false,"ams_mapping":""}})
    );
    assert_eq!(commands[1]["print"].as_object().unwrap().len(), 3);
    assert_eq!(commands[1]["print"]["param"], "");
    c.fake
        .reports
        .send(json!({"print":{"gcode_state":"FINISH","print_error":42}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(c.safety.job("control").await.unwrap().state, JobState::Done);
    c.fake
        .reports
        .send(json!({"print":{"gcode_state":"FINISH","print_error":0}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(c.safety.job("control").await.unwrap().state, JobState::Done);
    assert_eq!(c.status().await.needs_clearing, Some(true));
    assert!(c.action("pause").await.is_err());
    c.fake
        .reports
        .send(json!({"print":{"gcode_state":"IDLE"}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    c.clear().await.unwrap();
    assert_eq!(c.status().await.needs_clearing, Some(false));
}
#[tokio::test(flavor = "multi_thread")]
async fn stop_only_the_tracked_live_job() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    c.clear_start().await;
    c.report(json!({"print":{"gcode_state":"NEW_STATE"}})).await;
    assert_eq!(c.status().await.state, PrinterState::Error);

    assert_eq!(c.action("cancel").await.unwrap().state, JobState::Cancelled);
    assert_eq!(c.status().await.needs_clearing, Some(true));
    let count = c.fake.commands.lock().await.len();
    assert!(c.action("cancel").await.is_err());
    assert!(c.action("resume").await.is_err());
    assert_eq!(c.fake.commands.lock().await.len(), count);
}
#[tokio::test(flavor = "multi_thread")]
async fn replacement_live_job_blocks_control() {
    let c = Control::new().await;
    c.clear_start().await;
    c.fake
        .reports
        .send(json!({"print":{"gcode_state":"RUNNING","subtask_name":"another job"}}))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(c.action("pause").await.is_err());
    assert!(c.action("cancel").await.is_err());
    assert_eq!(c.fake.commands.lock().await.len(), 1);
}
#[tokio::test(flavor = "multi_thread")]
async fn authorization_rejects_start_and_never_repeats_it() {
    let c = Control::new().await;
    c.fake.authorization.store(true, Ordering::SeqCst);
    c.clear().await.unwrap();
    let error = c.start().await.unwrap_err().to_string();
    assert!(
        error.contains("rejected") && error.contains("authorization"),
        "{error}"
    );
    assert!(c
        .backend
        .start("control")
        .await
        .unwrap_err()
        .to_string()
        .contains("already attempted"));
    assert_eq!(c.fake.commands.lock().await.len(), 1);
    assert!(std::fs::read_to_string(c.dir.path().join("jobs.json"))
        .unwrap()
        .contains("\"attempted\":true"));
}
#[tokio::test(flavor = "multi_thread")]
async fn lost_start_reply_reconciles_without_resending() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    c.fake.lose_reply.store(true, Ordering::SeqCst);
    c.clear().await.unwrap();
    let _ = c.start().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while c.fake.credentials.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let command = c.fake.commands.lock().await[0].clone();
    c.report(json!({"print":{"gcode_state":"RUNNING","print_error":0,"subtask_name":command["print"]["subtask_name"],"gcode_start_time":command["print"]["sequence_id"]}})).await;
    assert_eq!(
        c.safety.job("control").await.unwrap().state,
        JobState::Printing
    );
    assert!(c.backend.start("control").await.is_err());
    assert_eq!(c.fake.commands.lock().await.len(), 1);
}
#[tokio::test(flavor = "multi_thread")]
async fn reviewed_archive_must_not_change() {
    let c = Control::new().await;
    assert!(c.input.is_file());
    std::fs::write(c.dir.path().join("sliced.3mf"), b"changed").unwrap();
    c.clear().await.unwrap();
    assert!(c.start().await.unwrap_err().to_string().contains("changed"));
    assert!(c.fake.commands.lock().await.is_empty());
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
}
#[tokio::test(flavor = "multi_thread")]
async fn delivery_success_without_state_change_is_not_confirmed() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    c.fake.no_state.store(true, Ordering::SeqCst);
    c.clear().await.unwrap();
    let error = c.start().await.unwrap_err().to_string();
    assert!(error.contains("not confirmed"), "{error}");
    assert_eq!(
        c.safety.job("control").await.unwrap().state,
        JobState::Ready
    );
    assert!(c.backend.start("control").await.is_err());
    assert_eq!(c.fake.commands.lock().await.len(), 1);
}
#[tokio::test(flavor = "multi_thread")]
async fn failed_upload_never_sends_start() {
    let c = Control::with_size(true).await;
    c.clear().await.unwrap();
    assert!(c.start().await.is_err());
    assert!(c.fake.commands.lock().await.is_empty());
    assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 1);
}
#[tokio::test(flavor = "multi_thread")]
async fn authorization_rejects_live_control_without_claiming_transition() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    c.clear_start().await;
    c.fake.authorization.store(true, Ordering::SeqCst);
    let error = c.action("pause").await.unwrap_err().to_string();
    assert!(error.contains("rejected"));
    assert_eq!(
        c.safety.job("control").await.unwrap().state,
        JobState::Printing
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn multi_filament_prepare_names_every_missing_assignment() {
    let c = Control::new().await;
    std::fs::write(c.dir.path().join("mode"), "multi").unwrap();
    let error = c
        .safety
        .prepare("multi", &c.input, "bambu", Options::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("filament 1 (PLA)"), "{error}");
    assert!(error.contains("filament 2 (PETG)"), "{error}");
    assert!(c.fake.commands.lock().await.is_empty());
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
}
#[tokio::test(flavor = "multi_thread")]
async fn another_execution_of_the_same_file_is_not_the_tracked_job() {
    let c = Control::new().await;
    c.clear_start().await;
    let command = c.fake.commands.lock().await[0].clone();
    c.fake.reports.send(json!({"print":{"gcode_state":"RUNNING","subtask_name":command["print"]["subtask_name"],"gcode_start_time":"99999999999999"}})).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(c.action("pause").await.is_err());
    assert!(c.action("cancel").await.is_err());
    assert_eq!(c.fake.commands.lock().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_states_allow_clearance_and_a_second_job() {
    use dutchman_print::backend::JobState;
    for terminal in ["FINISH", "FAILED"] {
        let mut c = Control::new().await;
        c.clear_start().await;
        c.fake
            .reports
            .send(json!({"print":{"gcode_state":terminal,"print_error":42}}))
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            c.safety.job("control").await.unwrap().state,
            if terminal == "FINISH" {
                JobState::Done
            } else {
                JobState::Failed
            }
        );
        assert_eq!(c.status().await.state, PrinterState::Idle);
        assert_eq!(c.status().await.needs_clearing, Some(true));
        c.safety
            .prepare("second", &c.input, "bambu", Options::default())
            .await
            .unwrap();
        c.safety
            .save(&c.dir.path().join("safety.json"))
            .await
            .unwrap();
        c.backend
            .save(&c.dir.path().join("jobs.json"))
            .await
            .unwrap();
        // Reload prepared reviews; clearance itself must be reacquired by the running service.
        c.app = dutchman_print::router(
            c.backend.clone(),
            dutchman_print::ServerConfig {
                storage: dutchman_print::StorageConfig {
                    data_dir: c.dir.path().to_owned(),
                    min_free: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(c.machine("start", Some("second")).await.is_err());
        c.clear().await.unwrap();
        assert_eq!(c.status().await.needs_clearing, Some(false));
        // Repeating the historical error is harmless; a changed error is active.
        c.report(json!({"print":{"print_error":42}})).await;
        assert_eq!(
            c.machine("start", Some("second")).await.unwrap()["state"],
            "printing"
        );
        assert_eq!(c.fake.commands.lock().await.len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_error_changes_are_active() {
    let c = Control::new().await;
    c.report(json!({"print":{"gcode_state":"FAILED","print_error":42}}))
        .await;
    c.clear().await.unwrap();
    c.report(json!({"print":{"gcode_state":"IDLE"}})).await;
    assert_eq!(c.status().await.state, PrinterState::Idle);
    c.report(json!({"print":{"print_error":43}})).await;
    assert_eq!(c.status().await.state, PrinterState::Error);
    assert!(c.start().await.is_err());
    assert!(c.clear().await.is_err());
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn hms_severity_and_attention_pause() {
    let c = Control::new().await;
    for level in [1, 2] {
        c.report(
            json!({"print":{"hms":[{"attr":0x03000000u32 | (level << 8),"code":0x00040007}]}}),
        )
        .await;
        let p = c.status().await;
        assert_eq!(p.state, PrinterState::Error);
        let detail = p.detail.unwrap();
        assert!(
            detail.contains(&format!("0300_0{level}00_0004_0007")),
            "{detail}"
        );
        assert!(detail.contains("https://wiki.bambulab.com/en/x1/troubleshooting/hmscode/"));
        assert!(c.start().await.is_err());
        // Also exercise the backend guard independently of challenge issuance.
        assert!(c
            .backend
            .start("control")
            .await
            .unwrap_err()
            .to_string()
            .contains("HMS"));
    }
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
    c.report(json!({"print":{"hms":[{"attr":0x03000400,"code":0x00010007}]}}))
        .await;
    assert_eq!(c.status().await.state, PrinterState::Idle);
    c.clear_start().await;
    c.report(json!({"print":{"gcode_state":"PAUSE"}})).await;
    let p = c.status().await;
    assert_eq!(p.state, PrinterState::Attention);
    assert!(p.detail.unwrap().contains("Needs attention"));
    assert!(c.action("resume").await.is_err());
    assert!(c.safety.resume("bambu").await.is_err());
    // Stop remains available even when a serious fault requires attention.
    c.report(json!({"print":{"hms":[{"attr":0x03000200,"code":0x00040007}]}}))
        .await;
    assert_eq!(c.status().await.state, PrinterState::Attention);
    assert!(c.action("cancel").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn nozzle_and_material_mismatches_refuse_before_upload() {
    for (report, expected) in [
        (json!({"nozzle_diameter":"0.2"}), ["0.4", "0.2"]),
        (json!({"vt_tray":{"tray_type":"PETG"}}), ["PLA", "PETG"]),
    ] {
        let c = Control::new().await;
        c.report(json!({"print":report})).await;
        c.clear().await.unwrap();
        let error = c.start().await.unwrap_err();
        for text in expected {
            assert!(error.contains(text), "{error}");
        }
        assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
        assert!(c.fake.commands.lock().await.is_empty());
    }
    let c = Control::with_mapping(false, true).await;
    c.report(json!({"print":{"ams":{"ams":[{"id":"1","tray":[{"id":"1","tray_type":"PETG"}]}]}}}))
        .await;
    c.clear().await.unwrap();
    let error = c.start().await.unwrap_err();
    for text in ["AMS slot 5", "PLA", "PETG"] {
        assert!(error.contains(text), "{error}");
    }
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
    c.report(json!({"print":{"ams":{"ams":[{"id":"1","tray":[{"id":"1","tray_type":"PLA"}]}]}}}))
        .await;
    c.start().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_after_reconnect_uses_live_delta_and_matching_cached_identity() {
    let c = Control::new().await;
    c.clear_start().await;
    let count = c.fake.connections.load(Ordering::SeqCst);
    let requests = c.fake.requests.load(Ordering::SeqCst);
    c.fake.reports.send(serde_json::Value::Null).unwrap();
    Fake::wait(&c.fake.connections, count + 1).await;
    c.report(json!({"print":{"bed_temper":40}})).await;
    assert_eq!(c.status().await.state, PrinterState::Printing);
    assert_eq!(c.fake.requests.load(Ordering::SeqCst), requests);
    c.action("cancel").await.unwrap();
    assert_eq!(
        c.fake.commands.lock().await.last().unwrap()["print"]["command"],
        "stop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nozzle_material_requirement_is_enforced() {
    let c = Control::with_mode(false, false, "hardened").await;
    c.clear().await.unwrap();
    let error = c.start().await.unwrap_err();
    for material in ["hardened_steel", "stainless_steel"] {
        assert!(error.contains(material), "{error}");
    }
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
    c.report(json!({"print":{"nozzle_type":"hardened_steel"}}))
        .await;
    c.start().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn every_mapped_filament_is_checked_and_tray_deltas_merge() {
    let c = Control::with_mode(false, true, "multi").await;
    c.report(json!({"print":{"ams":{"ams":[
        {"id":"1","tray":[{"id":"1","tray_type":"PLA"}]},
        {"id":"0","tray":[{"id":"0","tray_type":"PLA"}]}
    ]}}}))
    .await;
    c.clear().await.unwrap();
    let error = c.start().await.unwrap_err();
    for text in ["filament 2", "AMS slot 0", "PETG", "PLA"] {
        assert!(error.contains(text), "{error}");
    }
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
    c.report(json!({"print":{"ams":{"ams":[{"id":"0","tray":[{"id":"0","tray_type":"PETG"}]}]}}}))
        .await;
    c.start().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_between_polls_revokes_server_clearance() {
    let c = Control::new().await;
    c.clear().await.unwrap();
    assert_eq!(c.status().await.needs_clearing, Some(false));
    let count = c.fake.connections.load(Ordering::SeqCst);
    c.fake.reports.send(serde_json::Value::Null).unwrap();
    Fake::wait(&c.fake.connections, count + 1).await;
    c.report(json!({"print":{"bed_temper":40}})).await;
    assert_eq!(c.status().await.state, PrinterState::Idle);
    assert_eq!(c.status().await.needs_clearing, Some(true));
    assert!(c.start().await.is_err());
    c.clear().await.unwrap();
    c.start().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_wait_for_later_reports_without_resending() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    c.clear_start().await;
    c.fake.no_state.store(true, Ordering::SeqCst);
    for (action, state, expected) in [
        ("pause", "PAUSE", JobState::Paused),
        ("resume", "RUNNING", JobState::Printing),
        ("cancel", "FAILED", JobState::Cancelled),
    ] {
        let count = c.fake.commands.lock().await.len();
        let transition = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if c.fake.commands.lock().await.len() > count {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            c.fake
                .reports
                .send(json!({"print":{"gcode_state":state}}))
                .unwrap();
        };
        let (result, ()) = tokio::join!(c.action(action), transition);
        assert_eq!(result.unwrap().state, expected);
        assert_eq!(c.fake.commands.lock().await.len(), count + 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_printers_and_stop_during_start_poll() {
    use dutchman_print::backend::JobState;
    use std::time::Duration;
    use tokio::time::timeout;
    for pending in [false, true] {
        let a = Control::new().await;
        let b = Control::new().await;
        let mut config = copy_config(&a.config);
        let mut other = b.config.printers[0].clone();
        other.id = "other".into();
        config.printers.push(other);
        let backend = Arc::new(Safety::new(Arc::new(Printers::new(config).unwrap())));
        backend
            .prepare("one", &a.input, "bambu", Options::default())
            .await
            .unwrap();
        backend
            .prepare("two", &a.input, "other", Options::default())
            .await
            .unwrap();
        backend.clear("other").await.unwrap();
        backend.start("two").await.unwrap();
        backend.clear("bambu").await.unwrap();
        if pending {
            a.fake.no_state.store(true, Ordering::SeqCst);
        } else {
            a.ftps.block_upload.store(true, Ordering::SeqCst);
        }
        let start = tokio::spawn({
            let backend = backend.clone();
            async move { backend.start("one").await }
        });
        timeout(Duration::from_secs(5), async {
            if pending {
                let command = loop {
                    if let Some(command) = a.fake.commands.lock().await.first().cloned() { break command; }
                    tokio::task::yield_now().await;
                };
                a.fake.reports.send(json!({"print":{"gcode_state":"PAUSE","subtask_name":command["print"]["subtask_name"],"gcode_start_time":"1234","print_error":0}})).unwrap();
                loop {
                    if backend.printer("bambu").await.unwrap().state == PrinterState::Paused { break; }
                    tokio::task::yield_now().await;
                }
            } else { a.ftps.entered.notified().await; }
        }).await.unwrap();
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
            a.fake.no_state.store(false, Ordering::SeqCst);
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
            assert_eq!(a.fake.commands.lock().await.len(), 2);
        } else {
            a.ftps.release.notify_one();
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
        assert_eq!(b.fake.commands.lock().await.len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_overtakes_pause_and_resume_polls() {
    use dutchman_print::backend::JobState;
    use std::time::Duration;
    use tokio::time::timeout;
    for resume in [false, true] {
        let c = Control::new().await;
        c.backend.start("control").await.unwrap();
        if resume {
            c.backend.pause("bambu").await.unwrap();
        }
        let count = c.fake.commands.lock().await.len();
        c.fake.no_state.store(true, Ordering::SeqCst);
        let command = tokio::spawn({
            let backend = c.backend.clone();
            async move {
                if resume {
                    backend.resume("bambu").await
                } else {
                    backend.pause("bambu").await
                }
            }
        });
        timeout(Duration::from_secs(5), async {
            while c.fake.processed.load(Ordering::SeqCst) == count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!command.is_finished());
        c.fake.no_state.store(false, Ordering::SeqCst);
        let live = c.backend.printer("bambu").await.unwrap();
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                c.backend.stop(
                    "bambu",
                    live.job_id.as_deref().unwrap(),
                    live.job_name.as_deref().unwrap(),
                    live.job_path.as_deref()
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
        assert_eq!(c.fake.commands.lock().await.len(), count + 2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn printer_stop_external_attention_job_does_not_delete_external_file() {
    let c = Control::new().await;
    c.report(json!({"print":{"gcode_state":"PAUSE","subtask_name":"External cube","gcode_start_time":123,"hms":[{"attr":768,"code":1}]}})).await;
    assert_eq!(c.status().await.state, PrinterState::Attention);
    let result = c.machine("stop", None).await.unwrap();
    assert_eq!(result["state"], "idle");
    assert_eq!(result["needs_clearing"], true);
    assert_eq!(c.fake.commands.lock().await.len(), 1);
    assert_eq!(c.fake.commands.lock().await[0]["print"]["command"], "stop");
    assert_eq!(c.fake.connections.load(Ordering::SeqCst), 1);
    assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_sd_card_blocks_upload_and_terminal_files_are_removed() {
    use dutchman_print::backend::JobState;
    let c = Control::new().await;
    c.report(json!({"print":{"sdcard":false}})).await;
    c.clear().await.unwrap();
    assert!(c.start().await.unwrap_err().contains("SD card"));
    assert_eq!(c.ftps.received.load(Ordering::SeqCst), 0);
    assert!(c.fake.commands.lock().await.is_empty());
    c.report(json!({"print":{"sdcard":true}})).await;
    c.clear_start().await;
    assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 0);
    c.report(json!({"print":{"gcode_state":"FINISH"}})).await;
    assert_eq!(c.safety.job("control").await.unwrap().state, JobState::Done);
    assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 1);
    assert_eq!(c.safety.job("control").await.unwrap().state, JobState::Done);
    assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_cancelled_failed_and_uploaded_ready_files() {
    use dutchman_print::backend::JobState;
    for cancelled in [false, true] {
        let c = Control::new().await;
        c.clear_start().await;
        let job = if cancelled {
            c.action("cancel").await.unwrap()
        } else {
            c.report(json!({"print":{"gcode_state":"FAILED","print_error":42}}))
                .await;
            c.safety.job("control").await.unwrap()
        };
        assert_eq!(
            job.state,
            if cancelled {
                JobState::Cancelled
            } else {
                JobState::Failed
            }
        );
        assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 1);
    }
    for forget in [false, true] {
        let c = Control::new().await;
        c.ftps.block_upload.store(true, Ordering::SeqCst);
        let start = tokio::spawn({
            let b = c.backend.clone();
            async move { b.start("control").await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), c.ftps.entered.notified())
            .await
            .unwrap();
        c.report(json!({"print":{"gcode_state":"PAUSE"}})).await;
        c.ftps.release.notify_one();
        assert!(start.await.unwrap().is_err());
        assert_eq!(
            c.backend.job("control").await.unwrap().state,
            JobState::Ready
        );
        c.report(json!({"print":{"gcode_state":"IDLE"}})).await;
        if forget {
            c.backend.forget("control").await.unwrap();
        } else {
            assert_eq!(
                c.backend.cancel("control").await.unwrap().state,
                JobState::Cancelled
            );
        }
        assert_eq!(c.ftps.deleted.load(Ordering::SeqCst), 1);
        assert!(c.fake.commands.lock().await.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_controls_refuse_challenges_without_connecting() {
    let f = Fake::new().await;
    let config = Config {
        backend: dutchman_print::config::Backend {
            kind: "printers".into(),
            slice: false,
        },
        slicer: Some(dutchman_print::slicer::Config {
            kind: "orcaslicer".into(),
            path: None,
            datadir: None,
            timeout_seconds: 600,
        }),
        slicers: Default::default(),
        printers: vec![dutchman_print::config::Printer {
            kind: Some("bambu".into()),
            id: "bambu".into(),
            name: "Bambu".into(),
            model: "Synthetic".into(),
            host: Some("127.0.0.1".into()),
            port: Some(f.port),
            serial: Some("TESTSERIAL".into()),
            printer_profile: Some("Machine".into()),
            print_profile: Some("Draft".into()),
            filament_profile: Some("PLA".into()),
            access_code: Some("test-access".into()),
            cert_sha256: Some(dutchman_print::safety::hash(f.cert.as_ref())),
            ..Default::default()
        }],
    };
    let raw = Arc::new(Printers::new(config).unwrap());
    let (dir, app) = controls::app_backend(raw.clone());
    let saved = dir.path().join("restored.json");
    std::fs::write(
        &saved,
        json!({"restored": {
            "job": {"id":"restored", "printer":"bambu", "name":"Synthetic", "state":"ready",
                "estimate":{}, "progress":null, "seconds_left":null},
            "path":"synthetic.3mf", "digest":"synthetic", "info":null, "profiles":null,
            "remote":null, "attempted":true, "filename":"synthetic.gcode.3mf"
        }})
        .to_string(),
    )
    .unwrap();
    raw.restore(&saved).await.unwrap();
    let safety = Safety::new(raw.clone());
    assert!(safety
        .start("restored")
        .await
        .unwrap_err()
        .to_string()
        .contains("lan_control"));
    assert!(safety
        .cancel("restored")
        .await
        .unwrap_err()
        .to_string()
        .contains("lan_control"));
    raw.forget("restored").await.unwrap();
    controls::unsupported(&app, "bambu", controls::ALL).await;
    assert_eq!(f.connections.load(Ordering::SeqCst), 0);
    assert_eq!(f.requests.load(Ordering::SeqCst), 0);
    controls::listed(&app, &[]).await;
}

#[tokio::test]
async fn persistent_monitor_shutdown_sends_one_mqtt_disconnect() {
    let fake = Fake::new().await;
    let config = format!(
        r#"
[backend]
kind = "printers"
[slicer]
kind = "orcaslicer"
[[printers]]
kind = "bambu"
id = "bambu"
name = "Test Bambu"
model = "Synthetic"
printer_profile = "Machine"
print_profile = "Draft"
filament_profile = "PLA"
host = "127.0.0.1"
port = {}
serial = "TESTSERIAL"
access_code = "test-access"
cert_sha256 = "{}"
"#,
        fake.port,
        dutchman_print::safety::hash(fake.cert.as_ref())
    );
    let backend = Safety::new(Arc::new(
        Printers::new(toml::from_str(&config).unwrap()).unwrap(),
    ));
    assert_eq!(
        backend.printers().await.unwrap()[0].state,
        PrinterState::Idle
    );
    backend.shutdown().await.unwrap();
    Fake::wait(&fake.disconnects, 1).await;
    backend.shutdown().await.unwrap();
    assert_eq!(
        backend.printers().await.unwrap()[0].state,
        PrinterState::Offline
    );
    assert_eq!(fake.disconnects.load(Ordering::SeqCst), 1);
    assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    assert!(fake.commands.lock().await.is_empty());
}
