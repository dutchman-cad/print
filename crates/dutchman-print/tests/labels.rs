//! Synthetic PNGs and TSPL printers on loopback only; tests follow refusal conditions.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use dutchman_print::{
    backend::{Backend, Options, PrinterState},
    config::Config,
    labels,
    printers::Printers,
    router, ServerConfig,
};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower::ServiceExt as _;

fn crc(bytes: &[u8]) -> u32 {
    let mut c = !0u32;
    for b in bytes {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb88320u32.wrapping_mul(c & 1));
        }
    }
    !c
}
fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let mut body = tag.to_vec();
    body.extend(data);
    out.extend(&body);
    out.extend(crc(&body).to_be_bytes());
}
fn png(width: u32, height: u32, pixels: &[u8], color: u8, filter: u8) -> Vec<u8> {
    use std::io::Write as _;
    let mut result = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend(height.to_be_bytes());
    header.extend([8, color, 0, 0, 0]);
    chunk(&mut result, b"IHDR", &header);
    let mut raw = vec![];
    if height > 0 {
        for line in pixels.chunks(
            pixels
                .len()
                .checked_div(height as usize)
                .unwrap_or(1)
                .max(1),
        ) {
            raw.push(filter);
            raw.extend(line);
        }
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw).unwrap();
    chunk(&mut result, b"IDAT", &encoder.finish().unwrap());
    chunk(&mut result, b"IEND", &[]);
    result
}
fn image() -> Vec<u8> {
    png(8, 1, &[0, 255, 0, 255, 0, 255, 0, 255], 0, 0)
}
fn config_text(port: u16) -> String {
    format!("[backend]\nkind=\"printers\"\n[[printers]]\nkind=\"label\"\nlanguage=\"tspl\"\nhost=\"127.0.0.1\"\nport={port}\nwidth_mm=1.0\nheight_mm=0.125\ngap_mm=0.0\ndpi=203\nid=\"label\"\nname=\"Label printer\"\nmodel=\"Synthetic\"\n")
}
#[derive(Default)]
struct Data {
    status: u8,
    jobs: Vec<Vec<u8>>,
    cancel: usize,
    lost: bool,
    silent: bool,
    received: Vec<u8>,
}
struct Fake {
    data: Arc<Mutex<Data>>,
    port: u16,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fake {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let data = Arc::new(Mutex::new(Data::default()));
        let state = data.clone();
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accept=listener.accept()=> {
                        let (mut socket,_)=accept.unwrap();let state=state.clone();
                        tasks.spawn(async move {
                            let mut buffer=Vec::new();
                            while let Ok(byte)=socket.read_u8().await {
                                buffer.push(byte);
                                if buffer.ends_with(b"\x1b!?") {
                                    let (status,silent)={let d=state.lock().unwrap();(d.status,d.silent)};
                                    buffer.truncate(buffer.len()-3);
                                    if !silent && socket.write_all(&[status]).await.is_err(){break;}
                                } else if buffer.ends_with(b"\x1b!.") {
                                    let mut d=state.lock().unwrap();d.cancel+=1;d.status=0;buffer.clear();
                                } else if buffer.starts_with(b"SIZE ") {
                                    if state.lock().unwrap().lost {state.lock().unwrap().received=buffer;break;}
                                    if buffer.ends_with(b"\r\n") && buffer.windows(8).any(|w|w==b"PRINT 1,") {
                                        let mut d=state.lock().unwrap();d.jobs.push(buffer.clone());d.status=0x20;buffer.clear();
                                    }
                                }
                            }
                        });
                    }
                    _=tasks.join_next(),if !tasks.is_empty()=>{}
                }
            }
        });
        Self { data, port, task }
    }
    fn backend(&self) -> Arc<Printers> {
        Arc::new(Printers::new(Config::from_text(&config_text(self.port), None).unwrap()).unwrap())
    }
    fn app(&self, path: &std::path::Path) -> Router {
        let mut settings = ServerConfig::default();
        settings.storage.data_dir = path.into();
        settings.storage.min_free = 0;
        router(
            Arc::new(
                Printers::new(Config::from_text(&config_text(self.port), None).unwrap())
                    .unwrap()
                    .with_store(path.join("jobs.json")),
            ),
            settings,
        )
    }
}
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    content: &str,
    body: impl Into<Body>,
    token: Option<char>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", content);
    if let Some(c) = token {
        req = req.header("Plugin-Confirmation", c.to_string().repeat(64));
    }
    let reply = app
        .clone()
        .oneshot(req.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = reply.status();
    let bytes = reply.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}
async fn prepare(app: &Router, copies: u32) -> String {
    let (status, body) = send(
        app,
        "POST",
        &format!("/api/v1/jobs?printer=label&copies={copies}"),
        "image/png",
        image(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().into()
}
async fn challenge(app: &Router, action: &str, id: &str) -> Value {
    let (status, body) = send(
        app,
        "POST",
        "/api/v1/confirmations",
        "application/json",
        json!({"action":action,"printer":"label","job":id}).to_string(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}
async fn action(app: &Router, action: &str, id: &str, token: char) -> (StatusCode, Value) {
    let c = challenge(app, action, id).await;
    send(
        app,
        "POST",
        &format!("/api/v1/jobs/{id}/{action}"),
        "application/json",
        json!({"confirmation":c["id"]}).to_string(),
        Some(token),
    )
    .await
}
#[test]
fn malformed_and_oversize_pngs_are_refused_before_allocation() {
    for bytes in [
        vec![],
        b"%PDF-1.7".to_vec(),
        b"PK synthetic 3mf".to_vec(),
        image()[..20].to_vec(),
        vec![0; labels::MAX_PNG + 1],
        png(12001, 1, &[0], 0, 0),
        png(1, 1, &[0], 3, 0),
        png(1, 1, &[0], 0, 5),
    ] {
        assert!(labels::decode(&bytes).is_err());
    }
    let mut bad = image();
    bad[30] ^= 1;
    assert!(labels::decode(&bad).is_err());
    let mut trailing = image();
    trailing.push(0);
    assert!(labels::decode(&trailing).is_err());
}
#[test]
fn png_filters_alpha_threshold_and_exact_golden_job() {
    let p = Config::from_text(&config_text(9100), None)
        .unwrap()
        .printers
        .remove(0);
    assert_eq!(labels::decode(&image()).unwrap().bytes, vec![0x55]);
    assert_eq!(
        labels::job(&p, &image(), 1).unwrap(),
        b"SIZE 1 mm,0.125 mm\r\nGAP 0 mm,0 mm\r\nCLS\r\nBITMAP 0,0,1,1,0,\x55\r\nPRINT 1,1\r\n"
    );
    assert!(labels::job(&p, &image(), 999_999_999)
        .unwrap()
        .ends_with(b"PRINT 1,999999999\r\n"));
    assert!(labels::job(&p, &image(), 1_000_000_000).is_err());
    assert_eq!(
        labels::decode(&png(4, 1, &[127, 128, 0, 255], 0, 0))
            .unwrap()
            .bytes,
        vec![0x5f]
    );
    assert_eq!(
        labels::decode(&png(2, 1, &[0, 0, 0, 0, 0, 0, 0, 255], 6, 0))
            .unwrap()
            .bytes,
        vec![0xbf]
    );
    assert_eq!(
        labels::decode(&png(2, 1, &[0, 0, 0, 255, 255, 255], 2, 0))
            .unwrap()
            .bytes,
        vec![0x7f]
    );
    for filter in 0..=4 {
        // The first pixel is independent of neighbors for every filter.
        assert_eq!(
            labels::decode(&png(1, 1, &[0], 0, filter)).unwrap().bytes,
            vec![0x7f]
        );
    }
    let error = labels::job(&p, &png(10, 1, &[255; 10], 0, 0), 1)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("10 × 1") && error.contains("8 × 1"),
        "{error}"
    );
    assert!(labels::job(&p, &png(9, 1, &[255; 9], 0, 0), 1).is_ok());
}
#[test]
fn config_requires_language_stock_and_explicit_resolution() {
    let good = config_text(9100);
    for field in [
        "language=\"tspl\"\n",
        "width_mm=1.0\n",
        "height_mm=0.125\n",
        "gap_mm=0.0\n",
        "dpi=203\n",
        "host=\"127.0.0.1\"\n",
    ] {
        assert!(
            Config::from_text(&good.replace(field, ""), None).is_err(),
            "{field}"
        );
    }
    for (from, to) in [
        ("203", "600"),
        ("203", "0"),
        ("tspl", "zpl"),
        ("gap_mm=0.0", "gap_mm=-1"),
        ("width_mm=1.0", "width_mm=nan"),
        ("port=9100", "port=0"),
    ] {
        assert!(Config::from_text(&good.replace(from, to), None).is_err());
    }
    assert!(Config::from_text(&good.replace("port=9100\n", ""), None).is_ok());
    assert!(Config::from_text(&good.replace("dpi=203", "dpi=300"), None).is_ok());
}
#[tokio::test]
async fn status_bits_combinations_and_no_answer() {
    let fake = Fake::new().await;
    let backend = fake.backend();
    for (code, state, reason) in [
        (0, PrinterState::Idle, "Ready"),
        (1, PrinterState::Attention, "Head open"),
        (2, PrinterState::Attention, "Paper jam"),
        (4, PrinterState::Attention, "Paper empty"),
        (8, PrinterState::Attention, "Ribbon empty"),
        (0x10, PrinterState::Paused, "Paused"),
        (0x20, PrinterState::Printing, "Printing"),
        (0x80, PrinterState::Error, "Other error"),
        (0x40, PrinterState::Error, "Other error"),
        (
            0x0b,
            PrinterState::Attention,
            "Head open; Paper jam; Ribbon empty",
        ),
        (0x21, PrinterState::Attention, "Head open"),
    ] {
        fake.data.lock().unwrap().status = code;
        let p = backend.printer("label").await.unwrap();
        assert_eq!(p.state, state);
        assert!(p.detail.unwrap().contains(reason));
        assert_eq!(p.needs_clearing, Some(false));
        assert_eq!(p.controls, ["start", "cancel"]);
    }
    fake.data.lock().unwrap().silent = true;
    assert_eq!(
        backend.printer("label").await.unwrap().state,
        PrinterState::Offline
    );
    fake.task.abort();
    assert_eq!(
        backend.printer("label").await.unwrap().state,
        PrinterState::Offline
    );
}
#[tokio::test]
async fn wrong_job_types_and_options_are_refused_without_transfer() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    for (content, body) in [
        ("application/pdf", b"%PDF-1.7".to_vec()),
        ("model/3mf", b"PK synthetic".to_vec()),
    ] {
        let (status, error) = send(
            &app,
            "POST",
            "/api/v1/jobs?printer=label",
            content,
            body,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"].as_str().unwrap().contains("PNG"));
    }
    for query in [
        "sides=one-sided",
        "media=stock",
        "printer_profile=Fine",
        "copies=0",
        "copies=1000000000",
    ] {
        assert!(!send(
            &app,
            "POST",
            &format!("/api/v1/jobs?printer=label&{query}"),
            "image/png",
            image(),
            None
        )
        .await
        .0
        .is_success());
    }
    let input = dir.path().join("input.png");
    std::fs::write(&input, image()).unwrap();
    let ipp_text=format!("[backend]\nkind=\"printers\"\n[[printers]]\nkind=\"ipp\"\nid=\"paper\"\nname=\"Paper\"\nmodel=\"Synthetic\"\nurl=\"ipp://127.0.0.1:{}/ipp/print\"\n",fake.port);
    let ipp = Printers::new(Config::from_text(&ipp_text, None).unwrap()).unwrap();
    assert!(ipp
        .prepare("bad", &input, "paper", Options::default())
        .await
        .is_err());
    let mut settings = ServerConfig::default();
    settings.storage.data_dir = dir.path().join("ipp");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    let ipp_app = router(Arc::new(ipp), settings);
    assert_eq!(
        send(
            &ipp_app,
            "POST",
            "/api/v1/jobs?printer=paper",
            "image/png",
            image(),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let sim=Config::from_text("[backend]\nkind=\"simulated\"\n[[printers]]\nid=\"demo\"\nname=\"Demo\"\nmodel=\"Synthetic\"",None).unwrap();
    let mut settings = ServerConfig::default();
    settings.storage.data_dir = dir.path().join("sim");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    let sim_app = router(
        Arc::new(
            dutchman_print::simulated::Simulated::new(
                sim,
                Arc::new(dutchman_print::simulated::SystemClock::default()),
            )
            .unwrap(),
        ),
        settings,
    );
    assert_eq!(
        send(
            &sim_app,
            "POST",
            "/api/v1/jobs?printer=demo",
            "image/png",
            image(),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert!(fake.data.lock().unwrap().jobs.is_empty());
}
#[tokio::test]
async fn start_requires_single_use_challenge_without_bed_then_printing_idle_completes() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app, 3).await;
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/api/v1/jobs/{id}/start"),
            "application/json",
            "{}",
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let c = challenge(&app, "start", &id).await;
    let summary = c["summary"].as_str().unwrap();
    assert!(
        summary.contains("3 copies") && summary.contains("input.png") && !summary.contains("bed")
    );
    let body = json!({"confirmation":c["id"]}).to_string();
    let path = format!("/api/v1/jobs/{id}/start");
    let (status, job) = send(
        &app,
        "POST",
        &path,
        "application/json",
        body.clone(),
        Some('a'),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["state"], "printing");
    assert_eq!(
        fake.data.lock().unwrap().jobs,
        vec![
            b"SIZE 1 mm,0.125 mm\r\nGAP 0 mm,0 mm\r\nCLS\r\nBITMAP 0,0,1,1,0,\x55\r\nPRINT 1,3\r\n"
                .to_vec()
        ]
    );
    assert_eq!(
        send(&app, "POST", &path, "application/json", body, Some('b'))
            .await
            .0,
        StatusCode::CONFLICT
    );
    fake.data.lock().unwrap().status = 0;
    let (_, job) = send(
        &app,
        "GET",
        &format!("/api/v1/jobs/{id}"),
        "application/json",
        "",
        None,
    )
    .await;
    assert_eq!(job["state"], "done");
    assert_eq!(fake.data.lock().unwrap().jobs.len(), 1);
}
#[tokio::test]
async fn stale_status_and_changed_image_refuse_start_without_transfer() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app, 1).await;
    let c = challenge(&app, "start", &id).await;
    fake.data.lock().unwrap().status = 1;
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/api/v1/jobs/{id}/start"),
            "application/json",
            json!({"confirmation":c["id"]}).to_string(),
            Some('a')
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    fake.data.lock().unwrap().status = 0;
    let c = challenge(&app, "start", &id).await;
    std::fs::write(
        dir.path().join(&id).join("input.png"),
        png(8, 1, &[255; 8], 0, 0),
    )
    .unwrap();
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/api/v1/jobs/{id}/start"),
            "application/json",
            json!({"confirmation":c["id"]}).to_string(),
            Some('b')
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(fake.data.lock().unwrap().jobs.is_empty());
}
#[tokio::test]
async fn lost_connection_stays_uncertain_after_restart_and_never_resends() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app, 1).await;
    fake.data.lock().unwrap().lost = true;
    let (status, error) = action(&app, "start", &id, 'a').await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert!(error["error"].as_str().unwrap().contains("uncertain"));
    let app = fake.app(dir.path());
    for state in [0x20, 0] {
        fake.data.lock().unwrap().status = state;
        let (_, job) = send(
            &app,
            "GET",
            &format!("/api/v1/jobs/{id}"),
            "application/json",
            "",
            None,
        )
        .await;
        assert_eq!(job["state"], "ready");
        assert_eq!(
            send(
                &app,
                "POST",
                "/api/v1/confirmations",
                "application/json",
                json!({"action":"start","printer":"label","job":id}).to_string(),
                None
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/jobs?printer=label",
            "image/png",
            image(),
            None
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(fake.data.lock().unwrap().jobs.is_empty());
    assert_eq!(fake.data.lock().unwrap().received, b"SIZE ");
}
#[tokio::test]
async fn sent_label_is_never_cancelled_with_the_printer_wide_command() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app, 1).await;
    assert_eq!(action(&app, "start", &id, 'a').await.0, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/api/v1/jobs/{id}/cancel"),
            "application/json",
            "{}",
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // TSPL can only cancel every job on the printer, so a sent label is never cancelled here.
    let (status, reply) = send(
        &app,
        "POST",
        "/api/v1/confirmations",
        "application/json",
        json!({"action":"cancel","printer":"label","job":id}).to_string(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply.to_string().contains("printer's own cancel"),
        "{reply}"
    );
    assert_eq!(fake.data.lock().unwrap().cancel, 0);
}
#[test]
fn init_label_writes_stock_without_contacting_printer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("label.toml");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_dutchman-print"))
        .args([
            "init",
            "--kind",
            "label",
            "--host",
            "127.0.0.1",
            "--port",
            &listener.local_addr().unwrap().port().to_string(),
            "--language",
            "tspl",
            "--width-mm",
            "50",
            "--height-mm",
            "25",
            "--gap-mm",
            "2",
            "--dpi",
            "300",
            "--output",
        ])
        .arg(&output)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let config = Config::load(&output).unwrap();
    let p = &config.printers[0];
    assert_eq!(p.language.as_deref(), Some("tspl"));
    assert_eq!(p.width_mm, Some(50.0));
    assert_eq!(p.height_mm, Some(25.0));
    assert_eq!(p.gap_mm, Some(2.0));
    assert_eq!(p.dpi, Some(300));
    assert!(config.slicer.is_none());
    assert!(listener.accept().is_err());
}

#[tokio::test]
async fn multipart_and_cli_png_keep_filename_copies_and_no_bed_step() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let mut form=b"--label-boundary\r\nContent-Disposition: form-data; name=\"printer\"\r\n\r\nlabel\r\n--label-boundary\r\nContent-Disposition: form-data; name=\"copies\"\r\n\r\n2\r\n--label-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sticker.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    form.extend(image());
    form.extend(b"\r\n--label-boundary--\r\n");
    let (status, job) = send(
        &app,
        "POST",
        "/api/v1/jobs",
        "multipart/form-data; boundary=label-boundary",
        form,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    let id = job["id"].as_str().unwrap();
    let c = challenge(&app, "start", id).await;
    assert!(c["summary"].as_str().unwrap().contains("sticker.png"));
    assert_eq!(action(&app, "cancel", id, 'a').await.0, StatusCode::OK);
    assert_eq!(fake.data.lock().unwrap().cancel, 0);
    let config = dir.path().join("config.toml");
    std::fs::write(&config, config_text(fake.port)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let input = dir.path().join("cli.png");
    std::fs::write(&input, image()).unwrap();
    for yes in [false, true] {
        let (config, input, data) = (
            config.clone(),
            input.clone(),
            dir.path()
                .join(if yes { "cli-start" } else { "cli-prepare" }),
        );
        let output = tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_dutchman-print"));
            cmd.arg("--config")
                .arg(config)
                .arg("--data-dir")
                .arg(data)
                .arg("print")
                .arg(input)
                .args(["--printer", "label", "--copies", "2", "--json"]);
            if yes {
                cmd.arg("--yes");
            }
            cmd.stdin(std::process::Stdio::null()).output().unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let job: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(job["state"], if yes { "printing" } else { "ready" });
    }
    let d = fake.data.lock().unwrap();
    assert_eq!(d.jobs.len(), 1);
    assert!(d.jobs[0].ends_with(b"PRINT 1,2\r\n"));
}

#[tokio::test]
async fn changed_stock_after_restart_refuses_prepared_job() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.png");
    std::fs::write(&input, image()).unwrap();
    let store = dir.path().join("jobs.json");
    let backend = Printers::new(Config::from_text(&config_text(fake.port), None).unwrap())
        .unwrap()
        .with_store(store.clone());
    backend
        .prepare("stock", &input, "label", Options::default())
        .await
        .unwrap();
    let config = Config::from_text(
        &config_text(fake.port).replace("gap_mm=0.0", "gap_mm=1.0"),
        None,
    )
    .unwrap();
    let backend = Printers::new(config).unwrap();
    backend.restore(&store).await.unwrap();
    assert!(backend
        .review_action("start", "label", Some("stock"))
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("changed"));
    assert!(backend.start("stock").await.is_err());
    assert!(fake.data.lock().unwrap().jobs.is_empty());
}

#[test]
fn init_requires_stock_and_resolution_and_rejects_label_fields_for_other_kinds() {
    use dutchman_print::init::{build, Flags};
    let mut flags = Flags {
        kind: Some("label".into()),
        host: Some("127.0.0.1".into()),
        ..Default::default()
    };
    assert!(build(&flags, None).err().unwrap().contains("--width-mm"));
    flags.width_mm = Some(50.0);
    assert!(build(&flags, None).err().unwrap().contains("--height-mm"));
    flags.height_mm = Some(25.0);
    assert!(build(&flags, None).err().unwrap().contains("--gap-mm"));
    flags.gap_mm = Some(2.0);
    assert!(build(&flags, None).err().unwrap().contains("--dpi"));
    flags.dpi = Some(300);
    let setup = build(&flags, None).unwrap();
    assert!(setup.text.contains("port = 9100"));
    assert!(setup.text.contains("language = \"tspl\""));
    flags.kind = Some("simulated".into());
    flags.host = None;
    assert!(build(&flags, None).is_err());
}
