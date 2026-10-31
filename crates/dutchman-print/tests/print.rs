//! Synthetic 3MF files and simulated printers; no printer connections.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use base64::Engine as _;
use dutchman_print::backend::{Backend, Error, JobState, Options, PrinterState};
use dutchman_print::config::Config;
use dutchman_print::simulated::{Clock, Simulated};
use dutchman_print::{router, ServerConfig};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;

const CONFIG: &str = "[backend]\nkind = \"simulated\"\n[[printers]]\nid = \"demo\"\nname = \"Demo printer\"\nmodel = \"Example model\"\n";
const SECRET: &str = "private-test-value";
const BOUNDARY: &str = "print-tests-boundary";

#[derive(Default)]
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_secs(self.0.load(Ordering::SeqCst))
    }
}
impl TestClock {
    fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}

fn private_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}
fn simulation(clock: Arc<TestClock>) -> Arc<Simulated> {
    Arc::new(Simulated::new(toml::from_str(CONFIG).unwrap(), clock).unwrap())
}
fn settings(dir: &Path, clock: Arc<TestClock>) -> ServerConfig {
    let mut settings = ServerConfig {
        clock,
        ..Default::default()
    };
    settings.storage.data_dir = dir.to_path_buf();
    settings.storage.min_free = 0;
    settings
}
fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        out.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        out.write_all(bytes).unwrap();
    }
    out.finish().unwrap().into_inner()
}
fn model() -> Vec<u8> {
    zip(&[
        ("3D/demo.model", b"<model/>"),
        ("[Content_Types].xml", b"<Types/>"),
    ])
}
fn form(files: &[(&str, &[u8])], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (key, value) in fields {
        write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
        )
        .unwrap();
    }
    for (name, bytes) in files {
        write!(body, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n").unwrap();
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    write!(body, "--{BOUNDARY}--\r\n").unwrap();
    body
}
fn upload(files: &[(&str, &[u8])], fields: &[(&str, &str)]) -> Request<Body> {
    Request::post("/api/v1/jobs")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(form(files, fields)))
        .unwrap()
}
fn get(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}
fn action(path: &str, token: Option<&str>) -> Request<Body> {
    let mut request = Request::post(path);
    if let Some(token) = token {
        request = request.header("Plugin-Confirmation", token);
    }
    request.body(Body::empty()).unwrap()
}
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    (
        status,
        headers,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}
async fn accepted(app: &Router, request: Request<Body>, expected: u16) -> Value {
    let (status, _, body) = send(app, request).await;
    assert_eq!(
        status.as_u16(),
        expected,
        "{}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}
async fn challenge(app: &Router, action: &str, job: Option<&str>) -> Value {
    accepted(
        app,
        Request::post("/api/v1/confirmations")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"action":action,"printer":"demo","job":job}).to_string(),
            ))
            .unwrap(),
        201,
    )
    .await
}
async fn confirmed(app: &Router, path: &str, token: &str) -> Request<Body> {
    let parts: Vec<_> = path.split('/').collect();
    let act = parts.last().copied().unwrap();
    let job = if parts.get(3) == Some(&"jobs") {
        parts.get(4).copied()
    } else {
        None
    };
    let c = challenge(app, act, job).await;
    action(
        &format!("{path}?confirmation={}", c["id"].as_str().unwrap()),
        Some(token),
    )
}
async fn ready(app: &Router) -> Value {
    let request = confirmed(
        app,
        "/api/v1/printers/demo/clear",
        &dutchman_print::new_job_id().unwrap().repeat(2),
    )
    .await;
    assert_eq!(send(app, request).await.0, StatusCode::NO_CONTENT);
    accepted(
        app,
        upload(
            &[("bracket.3mf", &model())],
            &[
                ("printer", "demo"),
                ("name", "Bracket"),
                ("printer_profile", "Demo"),
                ("print_profile", "Fine"),
                ("filament_profile", "PLA"),
            ],
        ),
        201,
    )
    .await
}

#[tokio::test]
async fn simulator_printer_stop_ends_only_the_confirmed_live_job() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let token = || dutchman_print::new_job_id().unwrap().repeat(2);
    // Stop needs a live job to confirm against.
    let (status, _, _) = send(
        &app,
        Request::post("/api/v1/confirmations")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"action":"stop","printer":"demo"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let job = ready(&app).await;
    let id = job["id"].as_str().unwrap();
    let start = format!("/api/v1/jobs/{id}/start");
    accepted(&app, confirmed(&app, &start, &token()).await, 200).await;
    clock.advance(5);
    let stopped = accepted(
        &app,
        confirmed(&app, "/api/v1/printers/demo/stop", &token()).await,
        200,
    )
    .await;
    assert_eq!(stopped["state"], "idle");
    assert!(stopped.get("job_id").is_none_or(Value::is_null));
    // The Stop reply itself says the bed must be checked before the next start.
    assert_eq!(stopped["needs_clearing"], true);
    let job = accepted(&app, get(&format!("/api/v1/jobs/{id}")), 200).await;
    assert_eq!(job["state"], "cancelled");
    let printers = accepted(&app, get("/api/v1/printers"), 200).await;
    assert_eq!(printers[0]["needs_clearing"], true);
}

#[tokio::test]
async fn simulator_lifecycle_uses_clock_without_sleeping() {
    let clock = Arc::new(TestClock::default());
    let backend = simulation(clock.clone());
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("part.3mf");
    std::fs::write(&input, model()).unwrap();
    let estimate = backend
        .prepare("one", &input, "demo", Options::default())
        .await
        .unwrap();
    assert_eq!(
        estimate.seconds,
        Some(60 + std::fs::metadata(&input).unwrap().len() / 1024)
    );
    assert_eq!(backend.job("one").await.unwrap().state, JobState::Ready);
    let again = backend
        .prepare("two", &input, "demo", Options::default())
        .await
        .unwrap();
    assert_eq!(again.filament_mm, estimate.filament_mm);
    assert_eq!(again.layers, estimate.layers);
    backend.start("one").await.unwrap();
    assert!(matches!(backend.start("two").await, Err(Error::Busy)));
    clock.advance(20);
    let job = backend.job("one").await.unwrap();
    assert!(job.progress.unwrap() > 0.0 && job.progress.unwrap() < 1.0);
    let printers = backend.printers().await.unwrap();
    assert_eq!(printers[0].state, PrinterState::Printing);
    assert_eq!(printers[0].job_id.as_deref(), Some("one"));
    assert_eq!(printers[0].nozzle_temperature, Some(210.0));
    let paused = backend.pause("demo").await.unwrap();
    clock.advance(1000);
    assert_eq!(backend.job("one").await.unwrap().progress, paused.progress);
    assert_eq!(
        backend.printers().await.unwrap()[0].state,
        PrinterState::Paused
    );
    backend.resume("demo").await.unwrap();
    clock.advance(1000);
    assert_eq!(backend.job("one").await.unwrap().state, JobState::Done);
    assert_eq!(backend.job("one").await.unwrap().progress, Some(1.0));
    assert_eq!(
        backend.printers().await.unwrap()[0].state,
        PrinterState::Idle
    );
    backend.start("two").await.unwrap();
    backend.cancel("two").await.unwrap();
    clock.advance(1000);
    assert_eq!(backend.job("two").await.unwrap().state, JobState::Cancelled);
    assert!(matches!(backend.start("two").await, Err(Error::Refused(_))));
    assert!(matches!(backend.job("missing").await, Err(Error::NotFound)));
    assert!(matches!(
        backend
            .prepare("no", &input, "missing", Options::default())
            .await,
        Err(Error::NotFound)
    ));
    backend.forget("one").await.unwrap();
    assert!(matches!(backend.job("one").await, Err(Error::NotFound)));
}

#[tokio::test]
async fn api_lifecycle_and_all_machine_routes_require_fresh_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let job = ready(&app).await;
    let id = job["id"].as_str().unwrap();
    assert_eq!(job["state"], "ready");
    assert_eq!(job["name"], "Bracket");
    assert_eq!(job["printer"], "demo");
    assert!(dir.path().join(id).join("input.3mf").is_file());
    assert!(!dir.path().join(id).join("check").exists());
    assert!(job["estimate"].get("preview").is_none());
    let paths = [
        format!("/api/v1/jobs/{id}/start"),
        format!("/api/v1/jobs/{id}/cancel"),
        "/api/v1/printers/demo/pause".into(),
        "/api/v1/printers/demo/resume".into(),
        "/api/v1/printers/demo/clear".into(),
    ];
    for path in &paths {
        for token in [
            None,
            Some("bad"),
            Some("A".repeat(64)).as_deref(),
            Some("g".repeat(64)).as_deref(),
            Some("a".repeat(63)).as_deref(),
            Some("a".repeat(65)).as_deref(),
        ] {
            let (status, _, body) = send(&app, action(path, token)).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(String::from_utf8_lossy(&body).contains("confirmation in SolidCraft"));
        }
    }
    let token = "a".repeat(64);
    let started = accepted(&app, confirmed(&app, &paths[0], &token).await, 200).await;
    assert_eq!(started["state"], "printing");
    clock.advance(10);
    let progress = accepted(&app, get(&format!("/api/v1/jobs/{id}")), 200).await;
    assert!(progress["progress"].as_f64().unwrap() > 0.0);
    let printers = accepted(&app, get("/api/v1/printers"), 200).await;
    assert_eq!(printers[0]["job_name"], "Bracket");
    assert_eq!(
        printers[0]["controls"],
        json!(["start", "pause", "resume", "cancel", "stop", "clear"])
    );
    for path in &paths {
        assert_eq!(
            send(&app, action(path, Some(&token))).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let paused = accepted(&app, confirmed(&app, &paths[2], &"b".repeat(64)).await, 200).await;
    assert_eq!(paused["state"], "paused");
    clock.advance(20);
    assert_eq!(
        accepted(&app, get(&format!("/api/v1/jobs/{id}")), 200).await["progress"],
        paused["progress"]
    );
    assert_eq!(
        accepted(&app, confirmed(&app, &paths[3], &"c".repeat(64)).await, 200).await["state"],
        "printing"
    );
    assert_eq!(
        accepted(&app, confirmed(&app, &paths[1], &"d".repeat(64)).await, 200).await["state"],
        "cancelled"
    );
    assert_eq!(
        send(&app, get(&format!("/api/v1/jobs/{id}/preview.png")))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // A confirmation is remembered for ten minutes, then may be accepted again.
    clock.advance(600);
    let next = ready(&app).await;
    let next_id = next["id"].as_str().unwrap();
    assert_eq!(
        accepted(
            &app,
            confirmed(&app, &format!("/api/v1/jobs/{next_id}/start"), &token).await,
            200
        )
        .await["state"],
        "printing"
    );
    clock.advance(100);
    assert_eq!(
        accepted(&app, get(&format!("/api/v1/jobs/{next_id}")), 200).await["state"],
        "done"
    );
    clock.advance(3601);
    assert_eq!(
        send(&app, get(&format!("/api/v1/jobs/{id}"))).await.0,
        StatusCode::NOT_FOUND
    );
    assert!(!dir.path().join(id).exists());
}

#[tokio::test]
async fn authentication_origin_and_security_headers_cover_routes() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let mut config = settings(dir.path(), clock.clone());
    config.login = Some(("user".into(), SECRET.into()));
    config.public_origin = vec!["https://cad.example.org".into()];
    let app = router(simulation(clock), config);
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("user:{SECRET}"))
    );
    for (method, path, auth, origin, site, expected) in [
        ("GET", "/healthz", "", "", "", 200),
        ("GET", "/api/v1/printers", "", "", "", 401),
        ("GET", "/api/v1/jobs/missing", "Basic broken", "", "", 401),
        ("GET", "/api/v1/jobs/missing/preview.png", "", "", "", 401),
        ("POST", "/api/v1/jobs", "", "", "", 401),
        ("GET", "/api/v1/printers", basic.as_str(), "", "", 200),
        (
            "POST",
            "/api/v1/jobs/missing/start",
            basic.as_str(),
            "",
            "cross-site",
            403,
        ),
        (
            "POST",
            "/api/v1/jobs/missing/cancel",
            basic.as_str(),
            "null",
            "",
            403,
        ),
        (
            "POST",
            "/api/v1/printers/demo/pause",
            basic.as_str(),
            "http://evil.example",
            "",
            403,
        ),
        (
            "POST",
            "/api/v1/printers/demo/resume",
            basic.as_str(),
            "https://cad.example.org/path",
            "",
            403,
        ),
        (
            "POST",
            "/api/v1/printers",
            basic.as_str(),
            "https://cad.example.org",
            "",
            405,
        ),
        (
            "POST",
            "/api/v1/printers",
            basic.as_str(),
            "http://127.0.0.1:8730",
            "",
            405,
        ),
        (
            "POST",
            "/api/v1/printers",
            basic.as_str(),
            "http://127.0.0.1:8731",
            "",
            403,
        ),
    ] {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:8730");
        if !auth.is_empty() {
            request = request.header(header::AUTHORIZATION, auth);
        }
        if !origin.is_empty() {
            request = request.header(header::ORIGIN, origin);
        }
        if !site.is_empty() {
            request = request.header("sec-fetch-site", site);
        }
        let (status, headers, body) = send(&app, request.body(Body::empty()).unwrap()).await;
        assert_eq!(status.as_u16(), expected, "{method} {path} {origin}");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY));
        assert!(!String::from_utf8_lossy(&body).contains(SECRET));
        if expected >= 400 {
            assert_eq!(headers[header::CONTENT_TYPE], "application/json");
            let error: Value = serde_json::from_slice(&body).unwrap();
            assert!(error["error"].as_str().is_some_and(|text| !text.is_empty()));
        }
    }
    let mut request = upload(&[("part.3mf", &model())], &[("printer", "demo")]);
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, basic.parse().unwrap());
    let job = accepted(&app, request, 201).await;
    let path = format!("/api/v1/jobs/{}/start", job["id"].as_str().unwrap());
    let auth_challenge = |act: &str, job: Option<&str>| {
        Request::post("/api/v1/confirmations")
            .header("content-type", "application/json")
            .header(header::AUTHORIZATION, basic.as_str())
            .body(Body::from(
                json!({"action":act,"printer":"demo","job":job}).to_string(),
            ))
            .unwrap()
    };
    let c = accepted(&app, auth_challenge("clear", None), 201).await;
    let mut clear = action(
        &format!(
            "/api/v1/printers/demo/clear?confirmation={}",
            c["id"].as_str().unwrap()
        ),
        Some(&"0".repeat(64)),
    );
    clear
        .headers_mut()
        .insert(header::AUTHORIZATION, basic.parse().unwrap());
    assert_eq!(send(&app, clear).await.0, StatusCode::NO_CONTENT);
    let token = "f".repeat(64);
    assert_eq!(
        send(&app, action(&path, Some(&token))).await.0,
        StatusCode::UNAUTHORIZED
    );
    let c = accepted(&app, auth_challenge("start", job["id"].as_str()), 201).await;
    let mut request = action(
        &format!("{path}?confirmation={}", c["id"].as_str().unwrap()),
        Some(&token),
    );
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, basic.parse().unwrap());
    assert_eq!(accepted(&app, request, 200).await["state"], "printing");
}

#[tokio::test]
async fn uploads_are_checked_and_failed_jobs_leave_no_files() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let valid = model();
    for request in [
        upload(&[("part.3mf", b"not a zip")], &[("printer", "demo")]),
        upload(
            &[("part.3mf", &zip(&[("readme.txt", b"hi")]))],
            &[("printer", "demo")],
        ),
        upload(
            &[("part.3mf", &zip(&[("check/3D/a.model", b"x")]))],
            &[("printer", "demo")],
        ),
        upload(
            &[("part.3mf", &zip(&[("3D/a.model", b"x"), ("../bad", b"x")]))],
            &[("printer", "demo")],
        ),
        upload(&[("part.zip", &valid)], &[("printer", "demo")]),
        upload(
            &[("one.3mf", &valid), ("two.3mf", &valid)],
            &[("printer", "demo")],
        ),
        upload(&[], &[("printer", "demo")]),
        upload(&[("part.3mf", &valid)], &[]),
        upload(
            &[("part.3mf", &valid)],
            &[("printer", "demo"), ("printer", "demo")],
        ),
    ] {
        assert_eq!(send(&app, request).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    assert_eq!(
        send(
            &app,
            upload(&[("part.3mf", &valid)], &[("printer", "missing")])
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let mut config = settings(dir.path(), clock.clone());
    config.storage.max_upload = 10;
    let small = router(simulation(clock.clone()), config);
    assert_eq!(
        send(
            &small,
            upload(&[("part.3mf", &valid)], &[("printer", "demo")])
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut config = settings(dir.path(), clock.clone());
    config.storage.zip.entries = 1;
    assert_eq!(
        send(
            &router(simulation(clock.clone()), config),
            upload(&[("part.3mf", &valid)], &[("printer", "demo")])
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut config = settings(dir.path(), clock.clone());
    config.storage.zip.bytes = 1;
    assert_eq!(
        send(
            &router(simulation(clock.clone()), config),
            upload(&[("part.3mf", &valid)], &[("printer", "demo")])
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut config = settings(dir.path(), clock.clone());
    config.storage.max_data = 0;
    std::fs::write(dir.path().join("existing"), "x").unwrap();
    assert_eq!(
        send(
            &router(simulation(clock), config),
            upload(&[("part.3mf", &valid)], &[("printer", "demo")])
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

fn cli(path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
        .arg("--config")
        .arg(path)
        .arg("--data-dir")
        .arg(path.parent().unwrap().join("data"))
        .args(args)
        .env_remove("DUTCHMAN_PRINT_PASSWORD")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
#[test]
fn cli_output_confirmation_and_json_prepare_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = private_config(dir.path(), CONFIG);
    let listed = cli(&path, &["printers"]);
    assert!(listed.status.success());
    assert_eq!(listed.stdout, b"Demo printer: idle\n");
    let listed = cli(&path, &["printers", "--json"]);
    assert!(listed.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&listed.stdout).unwrap(),
        json!([{"id":"demo", "name":"Demo printer", "model":"Example model", "state":"idle", "needs_clearing":true, "controls":["start","pause","resume","cancel","stop","clear"]}])
    );
    let input = dir.path().join("part.3mf");
    std::fs::write(&input, model()).unwrap();
    let input = input.to_str().unwrap();
    let prepared = cli(
        &path,
        &[
            "print",
            input,
            "--printer",
            "demo",
            "--json",
            "--print-profile",
            "Fine",
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let ready: Value = serde_json::from_slice(&prepared.stdout).unwrap();
    assert_eq!(ready["state"], "ready");
    assert!(prepared.stderr.is_empty());
    let refused = cli(&path, &["print", input, "--printer", "demo"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Use --yes"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Estimate:"));
    assert!(cli(&path, &["clear", "demo", "--yes"]).status.success());
    let stale = cli(
        &path,
        &["print", input, "--printer", "demo", "--json", "--yes"],
    );
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("--bed-clear --yes"));
    let started = cli(
        &path,
        &[
            "print",
            input,
            "--printer",
            "demo",
            "--json",
            "--bed-clear",
            "--yes",
        ],
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let job: Value = serde_json::from_slice(&started.stdout).unwrap();
    assert_eq!(job["state"], "printing");
    let id = job["id"].as_str().unwrap();
    assert!(String::from_utf8_lossy(&started.stderr).contains("action=start"));
    for (args, state) in [
        (vec!["status", id], "printing"),
        (vec!["pause", "demo", "--yes"], "paused"),
        (vec!["resume", "demo", "--yes"], "printing"),
    ] {
        let output = cli(&path, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["state"],
            state
        );
    }
    let refused = cli(&path, &["cancel", id]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Use --yes"));
    let cancelled = cli(&path, &["cancel", id, "--yes"]);
    assert!(cancelled.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&cancelled.stdout).unwrap()["state"],
        "cancelled"
    );
    assert!(cli(&path, &["clear", "demo", "--yes"]).status.success());
    let plain = cli(
        &path,
        &["print", input, "--printer", "demo", "--bed-clear", "--yes"],
    );
    assert!(plain.status.success());
    assert!(String::from_utf8_lossy(&plain.stderr).contains("Estimate:"));
}

#[test]
fn config_errors_are_clear_without_echoing_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = private_config(dir.path(), &CONFIG.replace("simulated", "unknown-backend"));
    let error = Config::load(&path).err().unwrap();
    assert!(error.contains("unknown-backend"));
    for text in [
        format!("secret = \"{SECRET}\"\nmalformed TOML"),
        format!("{CONFIG}\nsecret = \"{SECRET}\""),
        CONFIG.replace("id = \"demo\"", "id = \"bad/id\""),
        CONFIG.replace("name = \"Demo printer\"", "name = \"\""),
        " ".repeat(1048577),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = Config::load(&path).err().unwrap();
        assert!(!error.contains(SECRET));
    }
    let mut many = String::from("[backend]\nkind = \"simulated\"\n");
    for i in 0..65 {
        many.push_str(&format!(
            "[[printers]]\nid = \"p{i}\"\nname = \"Demo\"\nmodel = \"Demo\"\n"
        ));
    }
    std::fs::write(&path, many).unwrap();
    assert!(Config::load(&path).err().unwrap().contains("64 printers"));
}
#[cfg(unix)]
#[test]
fn config_permissions_are_checked_before_parsing() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = private_config(dir.path(), &format!("malformed {SECRET}"));
    for mode in [0o640, 0o604, 0o666] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let output = cli(&path, &["printers"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("chmod 600"));
        assert!(!error.contains(SECRET));
    }
}
#[test]
fn config_environment_and_flag_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let path = private_config(dir.path(), CONFIG);
    for with_flag in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dutchman-print"));
        command
            .args(["printers", "--json"])
            .arg("--data-dir")
            .arg(dir.path().join("data"));
        if with_flag {
            command
                .arg("--config")
                .arg(&path)
                .env("DUTCHMAN_PRINT_CONFIG", dir.path().join("missing"));
        } else {
            command.env("DUTCHMAN_PRINT_CONFIG", &path);
        }
        assert!(command.output().unwrap().status.success());
    }
    assert!(!cli(&path, &["serve", "--password", SECRET])
        .status
        .success());
}

struct FixtureBackend {
    inner: Arc<Simulated>,
    gate: Arc<tokio::sync::Semaphore>,
    entered: Arc<tokio::sync::Notify>,
    preview: PathBuf,
}
macro_rules! forward_job {
    ($method:ident) => {
        fn $method<'a>(
            &'a self,
            id: &'a str,
        ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Job> {
            self.inner.$method(id)
        }
    };
}
impl Backend for FixtureBackend {
    fn printers(
        &self,
    ) -> dutchman_print::backend::Reply<'_, Vec<dutchman_print::backend::Printer>> {
        self.inner.printers()
    }
    fn prepare<'a>(
        &'a self,
        id: &'a str,
        path: &'a Path,
        printer: &'a str,
        options: Options,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Estimate> {
        Box::pin(async move {
            self.entered.notify_one();
            let permit = self.gate.acquire().await.map_err(|_| Error::Unavailable)?;
            permit.forget();
            let mut estimate = self.inner.prepare(id, path, printer, options).await?;
            estimate.preview = Some(self.preview.clone());
            Ok(estimate)
        })
    }
    forward_job!(start);
    forward_job!(cancel);
    forward_job!(pause);
    forward_job!(resume);
    forward_job!(job);
    fn forget<'a>(&'a self, id: &'a str) -> dutchman_print::backend::Reply<'a, ()> {
        self.inner.forget(id)
    }
}
#[tokio::test]
async fn in_flight_limit_and_optional_preview() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let preview = dir.path().join("preview.png");
    std::fs::write(&preview, b"\x89PNG\r\n\x1a\n").unwrap();
    let backend = Arc::new(FixtureBackend {
        inner: simulation(clock.clone()),
        gate: gate.clone(),
        entered: entered.clone(),
        preview,
    });
    let mut config = settings(dir.path(), clock);
    config.storage.uploads = 1;
    let app = router(backend, config);
    let first = app.clone();
    let waiting = tokio::spawn(async move { ready(&first).await });
    entered.notified().await;
    assert_eq!(
        send(
            &app,
            upload(&[("part.3mf", &model())], &[("printer", "demo")])
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    gate.add_permits(1);
    let job = waiting.await.unwrap();
    let id = job["id"].as_str().unwrap();
    let (status, headers, bytes) = send(&app, get(&format!("/api/v1/jobs/{id}/preview.png"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");
    assert!(!serde_json::to_string(&job)
        .unwrap()
        .contains(dir.path().to_str().unwrap()));
}

#[tokio::test]
async fn confirmation_is_atomic_and_duplicate_headers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(simulation(clock.clone()), settings(dir.path(), clock));
    let job = ready(&app).await;
    let path = format!("/api/v1/jobs/{}/start", job["id"].as_str().unwrap());
    let c = challenge(&app, "start", job["id"].as_str()).await;
    let path = format!("{path}?confirmation={}", c["id"].as_str().unwrap());
    let token = "e".repeat(64);
    let mut duplicate = action(&path, Some(&token));
    duplicate
        .headers_mut()
        .append("Plugin-Confirmation", token.parse().unwrap());
    assert_eq!(send(&app, duplicate).await.0, StatusCode::FORBIDDEN);
    let (first, second) = tokio::join!(
        send(&app, action(&path, Some(&token))),
        send(&app, action(&path, Some(&token)))
    );
    assert!(
        (first.0 == StatusCode::OK && second.0 == StatusCode::CONFLICT)
            || (second.0 == StatusCode::OK && first.0 == StatusCode::CONFLICT)
    );
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn serve_uses_environment_password_and_private_storage_on_loopback() {
    use std::io::{BufRead as _, Read as _};
    let dir = tempfile::tempdir().unwrap();
    let config = private_config(dir.path(), CONFIG);
    let data = dir.path().join("private-data");
    let mut child = Child(
        Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
            .arg("--config")
            .arg(config)
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(&data)
            .env("DUTCHMAN_PRINT_PASSWORD", SECRET)
            .env("DUTCHMAN_PRINT_USER", "test-user")
            .env_remove("DUTCHMAN_PRINT_PUBLIC_ORIGIN")
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    std::io::BufReader::new(child.0.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(!line.contains(SECRET));
    let address = line
        .trim()
        .strip_prefix("dutchman-print: listening on http://")
        .unwrap();
    assert!(address.starts_with("127.0.0.1:"));
    for (path, auth, expected) in [
        ("/healthz", "".to_owned(), "200 OK"),
        ("/api/v1/printers", "".to_owned(), "401 Unauthorized"),
        (
            "/api/v1/printers",
            format!(
                "Authorization: Basic {}\r\n",
                base64::engine::general_purpose::STANDARD.encode(format!("test-user:{SECRET}"))
            ),
            "200 OK",
        ),
    ] {
        let mut connection = std::net::TcpStream::connect(address).unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            connection,
            "GET {path} HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        connection.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected}")),
            "{response}"
        );
    }
    assert!(data.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&data).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn cli_snapshots_keep_clock_progress_and_expire_files() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let first = simulation(clock.clone());
    let input = dir.path().join("part.3mf");
    std::fs::write(&input, model()).unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    std::fs::create_dir(dir.path().join(id)).unwrap();
    first
        .prepare(id, &input, "demo", Options::default())
        .await
        .unwrap();
    first.start(id).await.unwrap();
    clock.advance(10);
    let saved = dir.path().join("simulation.json");
    first.save(&saved).unwrap();
    clock.advance(10);
    let second = simulation(clock.clone());
    second.restore(&saved).unwrap();
    let job = second.job(id).await.unwrap();
    assert_eq!(job.state, JobState::Printing);
    assert!(job.progress.unwrap() > 0.3);
    second.pause("demo").await.unwrap();
    second.save(&saved).unwrap();
    clock.advance(100);
    let third = simulation(clock.clone());
    third.restore(&saved).unwrap();
    assert_eq!(third.job(id).await.unwrap().progress, job.progress);
    clock.advance(3601);
    third.restore(&saved).unwrap();
    assert!(matches!(third.job(id).await, Err(Error::NotFound)));
    assert!(!dir.path().join(id).exists());
}

#[tokio::test]
async fn raw_3mf_uploads_use_query_fields_and_the_same_limits() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let backend = simulation(clock.clone());
    let mut config = settings(dir.path(), clock);
    config.storage.max_upload = model().len() as u64;
    let app = router(backend, config);
    for content_type in [
        "model/3mf",
        "application/vnd.ms-package.3dmanufacturing-3dmodel+xml",
    ] {
        let request = Request::post("/api/v1/jobs?printer=demo&name=Bracket&printer_profile=Demo&print_profile=Fine&filament_profile=PLA")
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(model())).unwrap();
        let job = accepted(&app, request, 201).await;
        assert_eq!(job["name"], "Bracket");
        assert_eq!(job["state"], "ready");
        let saved = dir
            .path()
            .join(job["id"].as_str().unwrap())
            .join("input.3mf");
        assert_eq!(std::fs::read(saved).unwrap(), model());
    }
    for (query, content_type, bytes, status) in [
        ("printer=demo", "text/plain", model(), 415),
        ("printer=demo", "model/3mf", vec![0; model().len() + 1], 413),
        ("printer=demo", "model/3mf", b"not a zip".to_vec(), 400),
        ("printer=demo&printer=demo", "model/3mf", model(), 400),
        ("printer=demo&unknown=x", "model/3mf", model(), 400),
        ("name=Bracket", "model/3mf", model(), 400),
    ] {
        let request = Request::post(format!("/api/v1/jobs?{query}"))
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(bytes))
            .unwrap();
        let error = accepted(&app, request, status).await;
        assert!(error["error"].as_str().is_some_and(|text| !text.is_empty()));
        if status == 415 {
            assert!(error["error"].as_str().unwrap().contains("model/3mf"));
        }
    }
    let request = Request::post("/api/v1/jobs?printer=demo")
        .header(header::CONTENT_TYPE, "model/3mf")
        .body(Body::from(model()))
        .unwrap();
    assert_eq!(accepted(&app, request, 201).await["name"], "input.3mf");
    assert_eq!(
        accepted(&app, get("/healthz"), 200).await,
        json!({"service":"dutchman-print", "api":1})
    );
    assert_eq!(send(&app, get("/printers")).await.0, 404);
}

#[tokio::test]
async fn raw_3mf_checks_zip_budgets_and_removes_failed_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    for (entries, bytes, body) in [
        (1, 2 << 30, model()),
        (5000, 1, model()),
        (
            5000,
            2 << 30,
            zip(&[("3D/a.model", b"x"), ("../bad", b"x")]),
        ),
    ] {
        let mut config = settings(dir.path(), clock.clone());
        config.storage.zip.entries = entries;
        config.storage.zip.bytes = bytes;
        let app = router(simulation(clock.clone()), config);
        let request = Request::post("/api/v1/jobs?printer=demo")
            .header(header::CONTENT_TYPE, "model/3mf")
            .body(Body::from(body))
            .unwrap();
        accepted(&app, request, 400).await;
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn option_like_profiles_are_refused_by_api_cli_and_config() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(simulation(clock.clone()), settings(dir.path(), clock));
    let input = dir.path().join("part.3mf");
    std::fs::write(&input, model()).unwrap();
    let path = private_config(dir.path(), CONFIG);
    let message = "Profile names can't start with '-'.";
    for field in ["printer_profile", "print_profile", "filament_profile"] {
        let error = accepted(
            &app,
            upload(
                &[("part.3mf", &model())],
                &[("printer", "demo"), (field, "--post-process=command")],
            ),
            400,
        )
        .await;
        assert_eq!(error["error"], message);
        let request = Request::post(format!(
            "/api/v1/jobs?printer=demo&{field}=--post-process%3Dcommand"
        ))
        .header(header::CONTENT_TYPE, "model/3mf")
        .body(Body::from(model()))
        .unwrap();
        assert_eq!(accepted(&app, request, 400).await["error"], message);
        let flag = format!("--{}=--post-process=command", field.replace('_', "-"));
        let output = cli(
            &path,
            &[
                "print",
                input.to_str().unwrap(),
                "--printer",
                "demo",
                "--json",
                &flag,
            ],
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        std::fs::write(
            &path,
            format!("{CONFIG}{field} = \"--post-process=command\"\n"),
        )
        .unwrap();
        assert_eq!(Config::load(&path).err().unwrap(), message);
        std::fs::write(&path, CONFIG).unwrap();
    }
}

#[test]
fn profile_validation_keeps_plain_text_and_length_limits() {
    for value in ["", " ", "a\nb", "a\0b", &"a".repeat(257)] {
        let options = Options {
            print_profile: Some(value.into()),
            ..Default::default()
        };
        assert!(options.validate().is_err(), "{value:?}");
    }
    for value in ["-preset", "--post-process=command"] {
        assert_eq!(
            Options {
                filament_profile: Some(value.into()),
                ..Default::default()
            }
            .validate(),
            Err("Profile names can't start with '-'.".into())
        );
    }
    for value in ["Fine - quality", "PLA; name=value", &"a".repeat(256)] {
        assert!(Options {
            printer_profile: Some(value.into()),
            ..Default::default()
        }
        .validate()
        .is_ok());
    }
}

#[tokio::test]
async fn challenges_bind_actions_observations_expiry_and_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let job = ready(&app).await;
    let id = job["id"].as_str().unwrap();
    let c = challenge(&app, "start", Some(id)).await;
    let query = format!("?confirmation={}", c["id"].as_str().unwrap());
    let token = "1".repeat(64);
    assert_eq!(
        send(
            &app,
            action(&format!("/api/v1/printers/demo/clear{query}"), Some(&token))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(
            &app,
            action(&format!("/api/v1/jobs/{id}/start{query}"), None)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // Another clear changes neither the job nor state; the start can still be used.
    assert_eq!(
        send(
            &app,
            action(&format!("/api/v1/jobs/{id}/start{query}"), Some(&token))
        )
        .await
        .0,
        StatusCode::OK
    );
    drop(app);
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    assert_eq!(
        accepted(&app, get(&format!("/api/v1/jobs/{id}")), 200).await["state"],
        "printing"
    );
    assert_eq!(
        send(
            &app,
            action(&format!("/api/v1/jobs/{id}/start{query}"), Some(&token))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let request = confirmed(&app, "/api/v1/printers/demo/pause", &token).await;
    assert_eq!(send(&app, request).await.0, StatusCode::CONFLICT); // durable header reuse
                                                                   // Progress moves while a print runs; a pause confirmed a moment ago still applies.
    let c = challenge(&app, "pause", None).await;
    let stale = challenge(&app, "pause", None).await;
    clock.advance(1);
    let request = action(
        &format!(
            "/api/v1/printers/demo/pause?confirmation={}",
            c["id"].as_str().unwrap()
        ),
        Some(&"2".repeat(64)),
    );
    assert_eq!(send(&app, request).await.0, StatusCode::OK);
    // A pause confirmed while the printer was printing no longer applies once it is paused.
    let request = action(
        &format!(
            "/api/v1/printers/demo/pause?confirmation={}",
            stale["id"].as_str().unwrap()
        ),
        Some(&"5".repeat(64)),
    );
    let result = send(&app, request).await;
    assert_eq!(result.0, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&result.2).contains("printer changed"));
    let c = challenge(&app, "resume", None).await;
    clock.advance(60);
    assert_eq!(
        send(
            &app,
            action(
                &format!(
                    "/api/v1/printers/demo/resume?confirmation={}",
                    c["id"].as_str().unwrap()
                ),
                Some(&"3".repeat(64))
            )
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let request = confirmed(&app, &format!("/api/v1/jobs/{id}/cancel"), &"4".repeat(64)).await;
    assert_eq!(send(&app, request).await.0, StatusCode::OK);
}

#[tokio::test]
async fn simulated_stop_matches_its_advertised_support() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(simulation(clock.clone()), settings(dir.path(), clock));
    let job = ready(&app).await;
    let id = job["id"].as_str().unwrap();
    let request = confirmed(&app, &format!("/api/v1/jobs/{id}/start"), &"e".repeat(64)).await;
    accepted(&app, request, 200).await;
    let request = confirmed(&app, "/api/v1/printers/demo/stop", &"f".repeat(64)).await;
    let printer = accepted(&app, request, 200).await;
    assert_eq!(printer["state"], "idle");
    assert_eq!(
        accepted(&app, get(&format!("/api/v1/jobs/{id}")), 200).await["state"],
        "cancelled"
    );
}

fn queue_json(path: &str, value: Value) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}
async fn queue_add(app: &Router, target: &str) -> Value {
    let job = accepted(
        app,
        upload(&[("part.3mf", &model())], &[("printer", "demo")]),
        201,
    )
    .await;
    accepted(
        app,
        queue_json("/api/v1/queue", json!({"job":job["id"],"target":target})),
        201,
    )
    .await
}
async fn queue_clear(app: &Router, printer: &str) {
    let c = accepted(
        app,
        queue_json(
            "/api/v1/confirmations",
            json!({"action":"clear","printer":printer}),
        ),
        201,
    )
    .await;
    let request = queue_json(
        &format!("/api/v1/printers/{printer}/clear"),
        json!({"confirmation":c["id"]}),
    );
    let (mut parts, body) = request.into_parts();
    parts.headers.insert(
        "Plugin-Confirmation",
        dutchman_print::new_job_id()
            .unwrap()
            .repeat(2)
            .parse()
            .unwrap(),
    );
    assert_eq!(
        send(app, Request::from_parts(parts, body)).await.0,
        StatusCode::NO_CONTENT
    );
}
fn queue_dispatch(id: &str, confirmation: &Value) -> Request<Body> {
    let request = queue_json(
        &format!("/api/v1/queue/{id}/dispatch"),
        json!({"confirmation":confirmation["id"]}),
    );
    let (mut parts, body) = request.into_parts();
    parts.headers.insert(
        "Plugin-Confirmation",
        dutchman_print::new_job_id()
            .unwrap()
            .repeat(2)
            .parse()
            .unwrap(),
    );
    Request::from_parts(parts, body)
}
#[tokio::test]
async fn farm_queue_fifo_moves_remove_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let first = queue_add(&app, "any").await;
    let second = queue_add(&app, "demo").await;
    let ids = |v: &Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&accepted(&app, get("/api/v1/queue"), 200).await),
        vec![first["id"].clone(), second["id"].clone()]
    );
    accepted(
        &app,
        queue_json(
            &format!("/api/v1/queue/{}/move", second["id"].as_str().unwrap()),
            json!({"position":0}),
        ),
        200,
    )
    .await;
    clock.advance(7200);
    drop(app);
    let app = router(simulation(clock.clone()), settings(dir.path(), clock));
    assert_eq!(
        ids(&accepted(&app, get("/api/v1/queue"), 200).await),
        vec![second["id"].clone(), first["id"].clone()]
    );
    let request = Request::delete(format!("/api/v1/queue/{}", first["id"].as_str().unwrap()))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, request).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        ids(&accepted(&app, get("/api/v1/queue"), 200).await),
        vec![second["id"].clone()]
    );
}
#[tokio::test]
async fn farm_queue_dispatch_requires_confirmation_and_keeps_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let entry = queue_add(&app, "any").await;
    let id = entry["id"].as_str().unwrap();
    // Uncleared beds cannot be proposed or challenged.
    let p = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert!(p[0]["printer"].is_null());
    assert_eq!(
        send(
            &app,
            queue_json(
                &format!("/api/v1/queue/{id}/confirmations"),
                json!({"printer":"demo"})
            )
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    queue_clear(&app, "demo").await;
    let c = accepted(
        &app,
        queue_json(
            &format!("/api/v1/queue/{id}/confirmations"),
            json!({"printer":"demo"}),
        ),
        201,
    )
    .await;
    assert_eq!(
        send(
            &app,
            queue_json(
                &format!("/api/v1/queue/{id}/dispatch"),
                json!({"confirmation":c["id"]})
            )
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        accepted(&app, get("/api/v1/queue"), 200)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    clock.advance(61);
    assert_eq!(
        send(&app, queue_dispatch(id, &c)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        accepted(&app, get("/api/v1/queue"), 200)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    queue_clear(&app, "demo").await;
    // Clearance is fresh again, but the previous Start challenge has expired.
    assert_eq!(
        send(&app, queue_dispatch(id, &c)).await.0,
        StatusCode::CONFLICT
    );
    let c = accepted(
        &app,
        queue_json(
            &format!("/api/v1/queue/{id}/confirmations"),
            json!({"printer":"demo"}),
        ),
        201,
    )
    .await;
    let job = accepted(&app, queue_dispatch(id, &c), 200).await;
    assert_eq!(job["state"], "printing");
    assert!(accepted(&app, get("/api/v1/queue"), 200)
        .await
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        send(&app, queue_dispatch(id, &c)).await.0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn farm_queue_one_printer_per_proposal_and_cross_printer_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let config = format!("{CONFIG}\n[[printers]]\nid='second'\nname='Second'\nmodel='Example model'\n[[printers]]\nid='incompatible'\nname='Other'\nmodel='Example model'\nprinter_profile='Other profile'\n");
    let backend =
        Arc::new(Simulated::new(toml::from_str(&config).unwrap(), clock.clone()).unwrap());
    let app = router(backend, settings(dir.path(), clock));
    let a = queue_add(&app, "any").await;
    let b = queue_add(&app, "any").await;
    queue_add(&app, "any").await;
    for p in ["demo", "second", "incompatible"] {
        queue_clear(&app, p).await;
    }
    let proposals = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert_eq!(proposals[0]["printer"], "demo");
    assert_eq!(proposals[1]["printer"], "second");
    assert!(proposals[2]["printer"].is_null());
    let id = b["id"].as_str().unwrap();
    let c = accepted(
        &app,
        queue_json(
            &format!("/api/v1/queue/{id}/confirmations"),
            json!({"printer":"second"}),
        ),
        201,
    )
    .await;
    let job = accepted(&app, queue_dispatch(id, &c), 200).await;
    assert_eq!(job["printer"], "second");
    assert_ne!(job["id"], b["job"]);
    let proposals = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert_eq!(proposals[0]["id"], a["id"]);
    assert_eq!(proposals[0]["printer"], "demo");
    assert!(proposals[1]["printer"].is_null()); // busy second and incompatible third are skipped
}

#[derive(Default)]
struct QueueStartGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail_lookup: AtomicBool,
}
struct QueueBackend {
    inner: Simulated,
    starts: AtomicU64,
    fail: bool,
    start_gate: Option<Arc<QueueStartGate>>,
}
impl Backend for QueueBackend {
    fn bed_ready(&self, printer: &str, detail: Option<&str>) -> bool {
        (printer == "monitor" && detail == Some("READY"))
            || (printer == "demo" && self.start_gate.is_some())
    }
    fn controls(&self, printer: &str) -> Vec<&'static str> {
        if printer == "monitor" {
            Vec::new()
        } else {
            dutchman_print::backend::ALL_CONTROLS.to_vec()
        }
    }
    fn queue_profile(&self, printer: &str) -> Option<(String, Options)> {
        self.inner.queue_profile(printer)
    }
    fn restore_jobs<'a>(&'a self, path: &'a Path) -> dutchman_print::backend::Reply<'a, ()> {
        self.inner.restore_jobs(path)
    }
    fn persist_jobs<'a>(&'a self, path: &'a Path) -> dutchman_print::backend::Reply<'a, ()> {
        self.inner.persist_jobs(path)
    }
    fn printers(
        &self,
    ) -> dutchman_print::backend::Reply<'_, Vec<dutchman_print::backend::Printer>> {
        Box::pin(async {
            let mut printers = self.inner.printers().await?;
            for p in &mut printers {
                p.controls = self.controls(&p.id).iter().map(|s| s.to_string()).collect();
                if p.id == "busy" {
                    p.state = PrinterState::Busy;
                }
                if p.id == "monitor" {
                    p.detail = Some("READY".into());
                }
            }
            Ok(printers)
        })
    }
    fn prepare<'a>(
        &'a self,
        id: &'a str,
        path: &'a Path,
        printer: &'a str,
        options: Options,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Estimate> {
        self.inner.prepare(id, path, printer, options)
    }
    fn start<'a>(
        &'a self,
        id: &'a str,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Job> {
        Box::pin(async move {
            let previous = self.starts.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.start_gate {
                if previous == 0 {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
            if self.fail {
                Err(Error::Refused(
                    "Start outcome is uncertain. Read job status.".into(),
                ))
            } else {
                self.inner.start(id).await
            }
        })
    }
    fn cancel<'a>(
        &'a self,
        id: &'a str,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Job> {
        self.inner.cancel(id)
    }
    fn pause<'a>(
        &'a self,
        id: &'a str,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Job> {
        self.inner.pause(id)
    }
    fn resume<'a>(
        &'a self,
        id: &'a str,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Job> {
        self.inner.resume(id)
    }
    fn job_printer<'a>(&'a self, id: &'a str) -> dutchman_print::backend::Reply<'a, String> {
        Box::pin(async move {
            if self
                .start_gate
                .as_ref()
                .is_some_and(|gate| gate.fail_lookup.load(Ordering::SeqCst))
            {
                return Err(Error::Unavailable);
            }
            self.inner.job_printer(id).await
        })
    }
    fn job<'a>(
        &'a self,
        id: &'a str,
    ) -> dutchman_print::backend::Reply<'a, dutchman_print::backend::Job> {
        self.inner.job(id)
    }
    fn forget<'a>(&'a self, id: &'a str) -> dutchman_print::backend::Reply<'a, ()> {
        self.inner.forget(id)
    }
}
#[tokio::test]
async fn farm_queue_uncertain_dispatch_is_never_retried_even_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let backend = Arc::new(QueueBackend {
        inner: Simulated::new(toml::from_str(CONFIG).unwrap(), clock.clone()).unwrap(),
        starts: AtomicU64::new(0),
        fail: true,
        start_gate: None,
    });
    let app = router(backend.clone(), settings(dir.path(), clock.clone()));
    let entry = queue_add(&app, "demo").await;
    let id = entry["id"].as_str().unwrap();
    queue_clear(&app, "demo").await;
    let c = accepted(
        &app,
        queue_json(
            &format!("/api/v1/queue/{id}/confirmations"),
            json!({"printer":"demo"}),
        ),
        201,
    )
    .await;
    assert_eq!(
        send(&app, queue_dispatch(id, &c)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
    let list = accepted(&app, get("/api/v1/queue"), 200).await;
    assert_eq!(list[0]["attempted"], true);
    assert!(list[0]["reason"].as_str().unwrap().contains("uncertain"));
    assert_eq!(
        send(&app, queue_dispatch(id, &c)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
    drop(app);
    let app = router(backend.clone(), settings(dir.path(), clock));
    assert_eq!(
        send(&app, queue_dispatch(id, &c)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn farm_queue_skips_monitoring_busy_and_filter_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let config = format!("{CONFIG}\n[[printers]]\nid='monitor'\nname='Monitor'\nmodel=''\n[[printers]]\nid='busy'\nname='Busy'\nmodel=''\n");
    let backend = Arc::new(QueueBackend {
        inner: Simulated::new(toml::from_str(&config).unwrap(), clock.clone()).unwrap(),
        starts: AtomicU64::new(0),
        fail: false,
        start_gate: None,
    });
    let app = router(backend, settings(dir.path(), clock));
    let first = queue_add(&app, "any").await;
    let second = queue_add(&app, "any").await;
    queue_clear(&app, "demo").await;
    let p = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert_eq!(p[0]["printer"], "demo");
    assert!(p[1]["printer"].is_null());
    let request = Request::delete(format!("/api/v1/queue/{}", second["id"].as_str().unwrap()))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, request).await.0, StatusCode::NO_CONTENT);
    let request = Request::delete(format!("/api/v1/queue/{}", first["id"].as_str().unwrap()))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, request).await.0, StatusCode::NO_CONTENT);
    accepted(
        &app,
        queue_json(
            "/api/v1/queue",
            json!({"job":second["job"],"target":"any","kind":"prusalink"}),
        ),
        201,
    )
    .await;
    let p = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert!(p[0]["printer"].is_null());
    accepted(
        &app,
        queue_json(
            "/api/v1/queue",
            json!({"job":first["job"],"target":"any","profile":"Different preset"}),
        ),
        201,
    )
    .await;
    let p = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert!(p[0]["printer"].is_null());
    assert!(p[1]["printer"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn farm_queue_cli_round_trip_uses_the_server_and_human_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(simulation(clock.clone()), settings(dir.path(), clock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let file = dir.path().join("test.3mf");
    std::fs::write(&file, model()).unwrap();
    async fn call(url: &str, args: Vec<String>) -> Output {
        let url = url.to_string();
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
                .args(["queue", "--server", &url])
                .args(args)
                .env_remove("DUTCHMAN_PRINT_PASSWORD")
                .stdin(Stdio::null())
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }
    let out = call(
        &url,
        vec![
            "add".into(),
            file.to_string_lossy().into_owned(),
            "--printer".into(),
            "demo".into(),
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let entry: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = entry["id"].as_str().unwrap().to_string();
    for args in [
        vec!["list".into()],
        vec!["proposals".into()],
        vec!["move".into(), id.clone(), "0".into()],
    ] {
        let out = call(&url, args).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = call(
        &url,
        vec![
            "dispatch".into(),
            id.clone(),
            "--printer".into(),
            "demo".into(),
            "--bed-clear".into(),
        ],
    )
    .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs confirmation"));
    let out = call(
        &url,
        vec![
            "dispatch".into(),
            id.clone(),
            "--printer".into(),
            "demo".into(),
            "--bed-clear".into(),
            "--yes".into(),
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let job: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(job["state"], "printing");
    let out = call(&url, vec!["list".into()]).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!([])
    );
    let out = call(
        &url,
        vec![
            "add".into(),
            file.to_string_lossy().into_owned(),
            "--printer".into(),
            "demo".into(),
        ],
    )
    .await;
    assert!(out.status.success());
    let entry: Value = serde_json::from_slice(&out.stdout).unwrap();
    let out = call(
        &url,
        vec!["remove".into(), entry["id"].as_str().unwrap().into()],
    )
    .await;
    assert!(out.status.success());
    server.abort();
}

#[tokio::test]
async fn farm_queue_in_flight_dispatch_reserves_sensor_ready_printer() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let gate = Arc::new(QueueStartGate::default());
    let backend = Arc::new(QueueBackend {
        inner: Simulated::new(toml::from_str(CONFIG).unwrap(), clock.clone()).unwrap(),
        starts: AtomicU64::new(0),
        fail: false,
        start_gate: Some(gate.clone()),
    });
    let app = router(backend.clone(), settings(dir.path(), clock.clone()));
    let first = queue_add(&app, "demo").await;
    let second = queue_add(&app, "demo").await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    let challenge_path = |id| format!("/api/v1/queue/{id}/confirmations");
    let move_path = |id| format!("/api/v1/queue/{id}/move");
    let first_challenge = accepted(
        &app,
        queue_json(&challenge_path(first_id), json!({"printer":"demo"})),
        201,
    )
    .await;
    // Obtain a valid, bound challenge for the second entry before either Start.
    accepted(
        &app,
        queue_json(&move_path(second_id), json!({"position":0})),
        200,
    )
    .await;
    let second_challenge = accepted(
        &app,
        queue_json(&challenge_path(second_id), json!({"printer":"demo"})),
        201,
    )
    .await;
    accepted(
        &app,
        queue_json(&move_path(first_id), json!({"position":0})),
        200,
    )
    .await;
    let request = queue_dispatch(first_id, &first_challenge);
    let dispatch_app = app.clone();
    let dispatch = tokio::spawn(async move { send(&dispatch_app, request).await });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    // The reservation must apply even when the attempted entry is now later in FIFO.
    accepted(
        &app,
        queue_json(&move_path(second_id), json!({"position":0})),
        200,
    )
    .await;
    let printers = accepted(&app, get("/api/v1/printers"), 200).await;
    assert_eq!(printers[0]["state"], "idle");
    assert_eq!(printers[0]["needs_clearing"], false);
    let proposals = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert_eq!(proposals[0]["id"], second["id"]);
    assert!(proposals
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["printer"].is_null()));
    assert_eq!(
        send(
            &app,
            queue_json(&challenge_path(second_id), json!({"printer":"demo"}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(&app, queue_dispatch(second_id, &second_challenge))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
    // A missing binding lookup must not silently release the unknown reservation.
    gate.fail_lookup.store(true, Ordering::SeqCst);
    assert_eq!(
        send(&app, get("/api/v1/queue/proposals")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    gate.fail_lookup.store(false, Ordering::SeqCst);
    gate.release.notify_one();
    let (status, _, _) = tokio::time::timeout(Duration::from_secs(5), dispatch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    let entries = accepted(&app, get("/api/v1/queue"), 200).await;
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["id"], second["id"]);
    // After the first job completes, the remaining entry can be proposed and started.
    clock.advance(120);
    let proposals = accepted(&app, get("/api/v1/queue/proposals"), 200).await;
    assert_eq!(proposals[0]["printer"], "demo");
    let challenge = accepted(
        &app,
        queue_json(&challenge_path(second_id), json!({"printer":"demo"})),
        201,
    )
    .await;
    accepted(&app, queue_dispatch(second_id, &challenge), 200).await;
    assert_eq!(backend.starts.load(Ordering::SeqCst), 2);
}

#[test]
fn persistent_history_cli_filters_and_preserves_input_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = private_config(dir.path(), CONFIG);
    let input = dir.path().join("original-part.3mf");
    std::fs::write(&input, model()).unwrap();
    let prepared = cli(
        &path,
        &[
            "print",
            input.to_str().unwrap(),
            "--printer",
            "demo",
            "--json",
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let job: Value = serde_json::from_slice(&prepared.stdout).unwrap();
    let cancelled = cli(&path, &["cancel", job["id"].as_str().unwrap(), "--yes"]);
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    for _ in 0..2 {
        let result = cli(
            &path,
            &[
                "history",
                "--printer",
                "demo",
                "--outcome",
                "cancelled",
                "--since",
                "2020-01-01T00:00:00Z",
                "--limit",
                "1",
                "--json",
            ],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rows: Vec<Value> = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["file_name"], "original-part.3mf");
        assert_eq!(rows[0]["source"], "original-part.3mf");
        assert_eq!(rows[0]["file_size"], model().len());
        assert_eq!(rows[0]["started_at"], Value::Null);
        assert_eq!(rows[0]["duration_seconds"], Value::Null);
    }
    let text = cli(&path, &["history"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("demo cancelled original-part.3mf"));
    assert!(!cli(&path, &["history", "--since", "yesterday"])
        .status
        .success());
    assert!(!cli(&path, &["history", "--limit", "10001"])
        .status
        .success());
    std::fs::write(
        dir.path().join("data/history.jsonl"),
        b"corrupt complete line\n",
    )
    .unwrap();
    let prepared = cli(
        &path,
        &[
            "print",
            input.to_str().unwrap(),
            "--printer",
            "demo",
            "--json",
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let job: Value = serde_json::from_slice(&prepared.stdout).unwrap();
    let cancelled = cli(&path, &["cancel", job["id"].as_str().unwrap(), "--yes"]);
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    assert!(!cli(&path, &["history", "--json"]).status.success());
}

#[tokio::test]
async fn persistent_history_api_archives_before_expiry_and_keeps_confirmation_checks() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::default());
    let app = router(
        simulation(clock.clone()),
        settings(dir.path(), clock.clone()),
    );
    let job = ready(&app).await;
    let id = job["id"].as_str().unwrap();
    let start = format!("/api/v1/jobs/{id}/start");
    assert_eq!(
        send(&app, action(&start, None)).await.0,
        StatusCode::FORBIDDEN
    );
    let token = || dutchman_print::new_job_id().unwrap().repeat(2);
    accepted(&app, confirmed(&app, &start, &token()).await, 200).await;
    accepted(
        &app,
        confirmed(&app, "/api/v1/printers/demo/pause", &token()).await,
        200,
    )
    .await;
    clock.advance(3601);
    // A paused job stays tracked after the ordinary one-hour expiry.
    let paused = accepted(&app, get(&format!("/api/v1/jobs/{id}")), 200).await;
    assert_eq!(paused["state"], "paused");
    accepted(
        &app,
        confirmed(&app, "/api/v1/printers/demo/resume", &token()).await,
        200,
    )
    .await;
    clock.advance(120);
    assert_eq!(
        send(&app, get(&format!("/api/v1/jobs/{id}"))).await.0,
        StatusCode::NOT_FOUND
    );
    let rows = accepted(&app, get("/api/v1/history?outcome=done"), 200).await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["job_id"], id);
    assert_eq!(rows[0]["file_name"], "bracket.3mf");
    assert_eq!(rows[0]["source"], "bracket.3mf");
    assert_eq!(rows[0]["job_name"], "Bracket");
    assert_eq!(rows[0]["printer_kind"], "simulated");
    drop(app);
    let restarted = router(simulation(clock.clone()), settings(dir.path(), clock));
    let again = accepted(&restarted, get("/api/v1/history?outcome=done"), 200).await;
    assert_eq!(again, rows);
    let clear = accepted(&restarted, get("/api/v1/history?outcome=bed_clear"), 200).await;
    assert_eq!(clear.as_array().unwrap().len(), 1);
    std::fs::write(dir.path().join("history.jsonl"), b"corrupt complete line\n").unwrap();
    let job = ready(&restarted).await;
    let id = job["id"].as_str().unwrap();
    let cancelled = accepted(
        &restarted,
        confirmed(&restarted, &format!("/api/v1/jobs/{id}/cancel"), &token()).await,
        200,
    )
    .await;
    assert_eq!(cancelled["state"], "cancelled");
    let status = accepted(&restarted, get(&format!("/api/v1/jobs/{id}")), 200).await;
    assert_eq!(status["state"], "cancelled");
    assert_eq!(
        send(&restarted, get("/api/v1/history")).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
