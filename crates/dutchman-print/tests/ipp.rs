//! Synthetic PDF documents and an RFC 8010 printer on loopback only.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use dutchman_print::{
    backend::{Backend, JobState, Options, PrinterState},
    config::Config,
    ipp::{Attribute as A, Group, Message},
    printers::Printers,
    router, ServerConfig,
};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt as _;
const PDF: &[u8] = b"%PDF-1.7\n% synthetic drawing\n%%EOF\n";
#[derive(Clone)]
struct Fake {
    state: Arc<Mutex<Data>>,
    url: String,
    task: Arc<tokio::task::JoinHandle<()>>,
}
struct Data {
    state: i32,
    accepting: bool,
    reasons: Vec<String>,
    formats: Vec<String>,
    jobs: Vec<(i32, String, i32)>,
    calls: Vec<u16>,
    cancel_ids: Vec<i32>,
    lost: bool,
    retain_submission: bool,
    submitted_state: i32,
    reject_validate: bool,
    foreign: bool,
}
impl Drop for Fake {
    fn drop(&mut self) {
        if Arc::strong_count(&self.task) == 1 {
            self.task.abort();
        }
    }
}
fn op() -> Group {
    Group {
        tag: 1,
        attributes: vec![
            A::text("attributes-charset", 0x47, "utf-8"),
            A::text("attributes-natural-language", 0x48, "en"),
        ],
    }
}
fn texts(name: &str, tag: u8, values: &[String]) -> A {
    A {
        name: name.into(),
        values: values
            .iter()
            .map(|v| (tag, v.as_bytes().to_vec()))
            .collect(),
    }
}
fn job(id: i32, name: &str, state: i32) -> Group {
    Group {
        tag: 2,
        attributes: vec![
            A::integer("job-id", 0x21, id),
            A::text("job-name", 0x42, name),
            A::integer("job-state", 0x23, state),
        ],
    }
}
async fn handle(State(state): State<Arc<Mutex<Data>>>, body: Bytes) -> Response {
    let (request, end) = Message::decode(&body).unwrap();
    let attrs = &request.groups[0];
    assert_eq!(attrs.attributes[0].name, "attributes-charset");
    assert_eq!(attrs.attributes[1].name, "attributes-natural-language");
    assert!(
        attrs.get("printer-uri").unwrap().texts(0x45).unwrap()[0].starts_with("ipp://127.0.0.1:")
    );
    let mut data = state.lock().unwrap();
    data.calls.push(request.code);
    let mut reply = Message {
        code: 0,
        request_id: request.request_id,
        groups: vec![op()],
    };
    match request.code {
        0xb => {
            let mut range = 1i32.to_be_bytes().to_vec();
            range.extend(99i32.to_be_bytes());
            reply.groups.push(Group {
                tag: 4,
                attributes: vec![
                    A::integer("printer-state", 0x23, data.state),
                    A::boolean("printer-is-accepting-jobs", data.accepting),
                    texts("printer-state-reasons", 0x44, &data.reasons),
                    texts("document-format-supported", 0x49, &data.formats),
                    A::text("media-ready", 0x44, "iso_a4_210x297mm"),
                    A {
                        name: "copies-supported".into(),
                        values: vec![(0x33, range)],
                    },
                    A::text("sides-supported", 0x44, "one-sided"),
                    A::text("media-supported", 0x44, "iso_a4_210x297mm"),
                ],
            });
        }
        4 => {
            assert_eq!(end, body.len());
            assert_eq!(
                attrs.get("ipp-attribute-fidelity").unwrap().values[0].1,
                vec![1]
            );
            assert_eq!(
                attrs.get("document-format").unwrap().texts(0x49).unwrap(),
                vec!["application/pdf"]
            );
            if data.reject_validate {
                reply.code = 0x040b;
            }
        }
        2 => {
            assert_eq!(&body[end..], PDF);
            assert_eq!(data.calls.iter().filter(|c| **c == 4).count(), 1);
            assert_eq!(
                request.groups[1]
                    .get("copies")
                    .unwrap()
                    .number(0x21)
                    .unwrap(),
                2
            );
            let name = attrs.get("job-name").unwrap().texts(0x42).unwrap()[0].to_string();
            if data.retain_submission {
                let submitted = data.submitted_state;
                data.jobs.push((42, name.clone(), submitted));
                data.state = if submitted >= 7 { 3 } else { 4 };
                reply.groups.push(job(42, &name, submitted));
            }
            if data.lost {
                return Response::builder()
                    .header("content-type", "application/ipp")
                    .body(Body::from_stream(futures_util::stream::once(async {
                        Err::<Bytes, _>(std::io::Error::other("reply lost"))
                    })))
                    .unwrap();
            }
        }
        0xa => {
            let which = attrs.get("which-jobs").unwrap().texts(0x44).unwrap()[0];
            for (id, name, state) in &data.jobs {
                if (which == "completed") == (*state >= 7) {
                    reply.groups.push(job(*id, name, *state));
                }
            }
        }
        9 => {
            let id = attrs.get("job-id").unwrap().number(0x21).unwrap();
            let (id, name, state) = data.jobs.iter().find(|(j, _, _)| *j == id).unwrap();
            reply.groups.push(job(
                *id,
                if data.foreign { "external" } else { name },
                *state,
            ));
        }
        8 => {
            let id = attrs.get("job-id").unwrap().number(0x21).unwrap();
            data.cancel_ids.push(id);
            assert_eq!(id, 42);
            data.jobs.iter_mut().find(|(j, _, _)| *j == id).unwrap().2 = 7;
            data.state = 3;
        }
        _ => panic!("unexpected operation"),
    }
    // Separate handwritten wire writer: verify the production encoder with a fake that
    // does not use that encoder for its replies.
    let mut bytes = vec![1, 1];
    bytes.extend(reply.code.to_be_bytes());
    bytes.extend(reply.request_id.to_be_bytes());
    for group in reply.groups {
        bytes.push(group.tag);
        for a in group.attributes {
            for (i, (tag, v)) in a.values.into_iter().enumerate() {
                bytes.push(tag);
                let name = if i == 0 { a.name.as_bytes() } else { b"" };
                bytes.extend((name.len() as u16).to_be_bytes());
                bytes.extend(name);
                bytes.extend((v.len() as u16).to_be_bytes());
                bytes.extend(v);
            }
        }
    }
    bytes.push(3);
    ([("content-type", "application/ipp")], bytes).into_response()
}
impl Fake {
    async fn new() -> Self {
        let state = Arc::new(Mutex::new(Data {
            state: 3,
            accepting: true,
            reasons: vec!["none".into()],
            formats: vec!["application/pdf".into()],
            jobs: vec![(999, "external".into(), 5)],
            calls: vec![],
            cancel_ids: vec![],
            lost: false,
            retain_submission: true,
            submitted_state: 5,
            reject_validate: false,
            foreign: false,
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ipp://{}/ipp/print", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/ipp/print", post(handle))
            .with_state(state.clone());
        let task = Arc::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self { state, url, task }
    }
    fn config(&self) -> Config {
        Config::from_text(&format!("[backend]\nkind=\"printers\"\n[[printers]]\nkind=\"ipp\"\nid=\"paper\"\nname=\"Drawing printer\"\nmodel=\"Synthetic\"\nurl={:?}\n",self.url),None).unwrap()
    }
    fn backend(&self) -> Arc<Printers> {
        Arc::new(Printers::new(self.config()).unwrap())
    }
    fn app(&self, dir: &std::path::Path) -> Router {
        let mut config = ServerConfig::default();
        config.storage.data_dir = dir.into();
        config.storage.min_free = 0;
        router(
            Arc::new(
                Printers::new(self.config())
                    .unwrap()
                    .with_store(dir.join("jobs.json")),
            ),
            config,
        )
    }
}
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    content: &str,
    body: impl Into<Body>,
    header: Option<char>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", content);
    if let Some(h) = header {
        request = request.header("Plugin-Confirmation", h.to_string().repeat(64));
    }
    let response = app
        .clone()
        .oneshot(request.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}
async fn prepare(app: &Router) -> String {
    let (status, job) = send(
        app,
        "POST",
        "/api/v1/jobs?printer=paper&copies=2&sides=one-sided&media=iso_a4_210x297mm",
        "application/pdf",
        PDF,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(job["estimate"], json!({}));
    job["id"].as_str().unwrap().into()
}
async fn challenge(app: &Router, action: &str, id: &str) -> Value {
    let (status, challenge) = send(
        app,
        "POST",
        "/api/v1/confirmations",
        "application/json",
        json!({"action":action,"printer":"paper","job":id}).to_string(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{challenge}");
    challenge
}
async fn start(app: &Router, id: &str) -> (StatusCode, Value) {
    let challenge = challenge(app, "start", id).await;
    assert!(challenge["summary"]
        .as_str()
        .unwrap()
        .contains("input.pdf' (2 copies) on Drawing printer"));
    assert!(!challenge["summary"].as_str().unwrap().contains("bed"));
    send(
        app,
        "POST",
        &format!("/api/v1/jobs/{id}/start"),
        "application/json",
        json!({"confirmation":challenge["id"]}).to_string(),
        Some('a'),
    )
    .await
}
#[tokio::test]
async fn confirmed_start_without_bed_clearance_cancel_only_our_job_and_terminal_states() {
    for (state, expected) in [(7, "cancelled"), (8, "failed"), (9, "done")] {
        let fake = Fake::new().await;
        let dir = tempfile::tempdir().unwrap();
        let app = fake.app(dir.path());
        let (_, printers) = send(
            &app,
            "GET",
            "/api/v1/printers",
            "application/json",
            "",
            None,
        )
        .await;
        assert_eq!(printers[0]["controls"], json!(["start", "cancel"]));
        assert_eq!(printers[0]["needs_clearing"], false);
        let id = prepare(&app).await;
        let (status, _) = send(
            &app,
            "POST",
            &format!("/api/v1/jobs/{id}/start"),
            "application/json",
            "{}",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, result) = start(&app, &id).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["state"], "printing");
        if state == 7 {
            let c = challenge(&app, "cancel", &id).await;
            let (status, result) = send(
                &app,
                "POST",
                &format!("/api/v1/jobs/{id}/cancel"),
                "application/json",
                json!({"confirmation":c["id"]}).to_string(),
                Some('b'),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(fake.state.lock().unwrap().cancel_ids, vec![42]);
        } else {
            fake.state
                .lock()
                .unwrap()
                .jobs
                .iter_mut()
                .find(|(j, _, _)| *j == 42)
                .unwrap()
                .2 = state;
        }
        let (_, result) = send(
            &app,
            "GET",
            &format!("/api/v1/jobs/{id}"),
            "application/json",
            "",
            None,
        )
        .await;
        assert_eq!(result["state"], expected);
        assert_eq!(
            fake.state
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| **c == 2)
                .count(),
            1
        );
        assert_eq!(fake.state.lock().unwrap().jobs[0].2, 5);
    }
}
#[tokio::test]
async fn lost_print_reply_reconciles_by_name_across_restart_without_resending() {
    let fake = Fake::new().await;
    fake.state.lock().unwrap().lost = true;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app).await;
    let (status, result) = start(&app, &id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    let app = fake.app(dir.path());
    let (status, job) = send(
        &app,
        "GET",
        &format!("/api/v1/jobs/{id}"),
        "application/json",
        "",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["state"], "printing");
    let (status, _) = send(
        &app,
        "POST",
        "/api/v1/confirmations",
        "application/json",
        json!({"action":"start","printer":"paper","job":id}).to_string(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let data = fake.state.lock().unwrap();
    assert_eq!(data.calls.iter().filter(|c| **c == 2).count(), 1);
    assert!(data.calls.iter().filter(|c| **c == 0xa).count() >= 2);
}
#[tokio::test]
async fn prepare_refusals_and_validate_job_refusal_send_no_print() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    for (content, body) in [
        ("model/3mf", b"synthetic 3mf".as_slice()),
        ("application/pdf", b"not PDF".as_slice()),
    ] {
        let (status, result) = send(
            &app,
            "POST",
            "/api/v1/jobs?printer=paper",
            content,
            body,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    }
    for query in [
        "copies=0",
        "copies=100",
        "copies=no",
        "sides=two-sided-long-edge",
        "media=unknown",
        "printer_profile=Fine",
    ] {
        let (status, _) = send(
            &app,
            "POST",
            &format!("/api/v1/jobs?printer=paper&{query}"),
            "application/pdf",
            PDF,
            None,
        )
        .await;
        assert!(status.is_client_error());
    }
    fake.state.lock().unwrap().formats = vec!["image/pwg-raster".into()];
    let (status, result) = send(
        &app,
        "POST",
        "/api/v1/jobs?printer=paper",
        "application/pdf",
        PDF,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("image/pwg-raster"));
    fake.state.lock().unwrap().formats = vec!["application/pdf".into()];
    let id = prepare(&app).await;
    fake.state.lock().unwrap().reject_validate = true;
    let (status, _) = start(&app, &id).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!fake.state.lock().unwrap().calls.contains(&2));
    let mut config = ServerConfig::default();
    config.storage.data_dir = dir.path().join("small");
    config.storage.min_free = 0;
    config.storage.max_upload = 8;
    std::fs::create_dir(&config.storage.data_dir).unwrap();
    let app = router(fake.backend(), config);
    let (status, _) = send(
        &app,
        "POST",
        "/api/v1/jobs?printer=paper",
        "application/pdf",
        PDF,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
#[tokio::test]
async fn status_mapping_reasons_capabilities_unknown_and_offline() {
    let fake = Fake::new().await;
    let backend = fake.backend();
    for (raw, reasons, expected) in [
        (3, vec!["none"], PrinterState::Idle),
        (4, vec!["none"], PrinterState::Printing),
        (5, vec!["none"], PrinterState::Error),
        (5, vec!["fuser-over-temp-error"], PrinterState::Error),
        (5, vec!["media-empty", "other"], PrinterState::Error),
        (99, vec!["none"], PrinterState::Error),
    ] {
        {
            let mut d = fake.state.lock().unwrap();
            d.state = raw;
            d.reasons = reasons.iter().map(|s| s.to_string()).collect();
        }
        let p = backend.printer("paper").await.unwrap();
        assert_eq!(p.state, expected);
        assert!(p.detail.unwrap().contains("media-ready: iso_a4_210x297mm"));
    }
    for reason in [
        "media-empty",
        "media-jam",
        "media-needed",
        "door-open",
        "cover-open",
        "interlock-open",
        "marker-supply-empty",
        "marker-waste-full",
        "toner-empty",
        "developer-empty",
        "output-area-full",
        "input-tray-missing",
        "output-tray-missing",
    ] {
        let reason = format!("{reason}-error");
        {
            let mut d = fake.state.lock().unwrap();
            d.state = 5;
            d.reasons = vec![reason.clone()];
        }
        let p = backend.printer("paper").await.unwrap();
        assert_eq!(p.state, PrinterState::Attention);
        assert!(p.detail.unwrap().contains(&reason));
    }
    {
        let mut d = fake.state.lock().unwrap();
        d.accepting = false;
    }
    assert_eq!(
        backend.printer("paper").await.unwrap().state,
        PrinterState::Busy
    );
    {
        let mut d = fake.state.lock().unwrap();
        d.accepting = true;
        d.reasons = vec!["malicious\nhttp://private:631/".repeat(100)];
    }
    let p = backend.printer("paper").await.unwrap();
    assert!(p.detail.unwrap().len() < 512);
    fake.state.lock().unwrap().reasons = vec!["127.0.0.1".into()];
    let detail = backend.printer("paper").await.unwrap().detail.unwrap();
    assert!(!detail.contains("127.0.0.1"));
    assert!(detail.contains("[redacted]"));
    fake.task.abort();
    assert_eq!(
        backend.printer("paper").await.unwrap().state,
        PrinterState::Offline
    );
}
#[tokio::test]
async fn changed_remote_job_name_refuses_cancel() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app).await;
    assert_eq!(start(&app, &id).await.0, StatusCode::OK);
    fake.state.lock().unwrap().foreign = true;
    let (status, _) = send(
        &app,
        "POST",
        "/api/v1/confirmations",
        "application/json",
        json!({"action":"cancel","printer":"paper","job":id}).to_string(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(fake.state.lock().unwrap().cancel_ids.is_empty());
}
#[test]
fn codec_roundtrips_every_used_type_and_rejects_truncation_and_malformed_values() {
    let mut range = 1i32.to_be_bytes().to_vec();
    range.extend(5i32.to_be_bytes());
    let message = Message {
        code: 2,
        request_id: 17,
        groups: vec![
            op(),
            Group {
                tag: 2,
                attributes: vec![
                    A::integer("copies", 0x21, 2),
                    A::integer("job-state", 0x23, 5),
                    A::boolean("ipp-attribute-fidelity", true),
                    A::text("job-name", 0x42, "Drawing"),
                    A::text("printer-uri", 0x45, "ipp://127.0.0.1/ipp/print"),
                    A::text("document-format", 0x49, "application/pdf"),
                    texts(
                        "sides",
                        0x44,
                        &["one-sided".into(), "two-sided-long-edge".into()],
                    ),
                    A {
                        name: "copies-supported".into(),
                        values: vec![(0x33, range)],
                    },
                    A {
                        name: "unknown-value".into(),
                        values: vec![(0x12, vec![])],
                    },
                ],
            },
        ],
    };
    let bytes = message.encode().unwrap();
    assert_eq!(Message::decode(&bytes).unwrap(), (message, bytes.len()));
    assert_eq!(&bytes[..8], &[1, 1, 0, 2, 0, 0, 0, 17]);
    for end in 0..bytes.len() {
        assert!(Message::decode(&bytes[..end]).is_err(), "{end}");
    }
    for (tag, value) in [
        (0x21, vec![0]),
        (0x23, vec![0, 0]),
        (0x22, vec![2]),
        (0x33, vec![0; 7]),
        (0x12, vec![1]),
        (0x47, vec![255]),
        (0x36, vec![0, 2, b'e', b'n', 0, 9, b'a']),
    ] {
        let mut wire = vec![1, 1, 0, 0, 0, 0, 0, 1, 1, tag, 0, 1, b'x'];
        wire.extend((value.len() as u16).to_be_bytes());
        wire.extend(value);
        wire.push(3);
        assert!(Message::decode(&wire).is_err());
    }
    for wire in [
        vec![1, 1, 0, 0, 0, 0, 0, 1, 3],
        vec![1, 1, 0, 0, 0, 0, 0, 1, 1, 0x44, 0, 0, 0, 1, b'a', 3],
        vec![1, 1, 0, 0, 0, 0, 0, 1, 1, 0x44, 255, 255],
    ] {
        assert!(Message::decode(&wire).is_err());
    }
}
#[test]
fn config_tls_and_credentials_are_checked_without_network() {
    for url in [
        "ipp://127.0.0.1:631/ipp/print",
        "ipps://127.0.0.1/ipp/print",
        "http://127.0.0.1:631/ipp/print",
        "https://127.0.0.1:631/ipp/print",
    ] {
        let text=format!("[backend]\nkind=\"printers\"\n[[printers]]\nkind=\"ipp\"\nid=\"paper\"\nname=\"Paper\"\nmodel=\"Fake\"\nurl={url:?}\n");
        assert!(Config::from_text(&text, None).is_ok());
        let auth = format!("{text}username=\"test\"\npassword=\"secret\"\n");
        assert_eq!(
            Config::from_text(&auth, None).is_ok(),
            url.starts_with("ipps") || url.starts_with("https")
        );
    }
}
#[tokio::test]
async fn direct_pdf_prepare_needs_no_slicer_and_tracks_ready_job() {
    let fake = Fake::new().await;
    let backend = fake.backend();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("drawing.pdf");
    std::fs::write(&input, PDF).unwrap();
    backend
        .prepare("one", &input, "paper", Options::default())
        .await
        .unwrap();
    assert_eq!(backend.job("one").await.unwrap().state, JobState::Ready);
}

#[tokio::test]
async fn pdf_to_3d_printer_is_refused_before_slicing() {
    let dir = tempfile::tempdir().unwrap();
    let config=Config::from_text("[backend]\nkind=\"simulated\"\n[[printers]]\nid=\"demo\"\nname=\"Demo\"\nmodel=\"Synthetic\"",None).unwrap();
    let backend = Arc::new(
        dutchman_print::simulated::Simulated::new(
            config,
            Arc::new(dutchman_print::simulated::SystemClock::default()),
        )
        .unwrap(),
    );
    let mut settings = ServerConfig::default();
    settings.storage.data_dir = dir.path().into();
    settings.storage.min_free = 0;
    let app = router(backend, settings);
    let (status, error) = send(
        &app,
        "POST",
        "/api/v1/jobs?printer=demo",
        "application/pdf",
        PDF,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"].as_str().unwrap().contains("3D printer"));
}

#[tokio::test]
async fn cli_pdf_start_and_prepare_only_need_no_bed_flag() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!("[backend]\nkind=\"printers\"\n[[printers]]\nkind=\"ipp\"\nid=\"paper\"\nname=\"Drawing printer\"\nmodel=\"Synthetic\"\nurl={:?}\n",fake.url)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let input = dir.path().join("drawing.pdf");
    std::fs::write(&input, PDF).unwrap();
    for yes in [false, true] {
        let config = config.clone();
        let input = input.clone();
        let data = dir.path().join(if yes { "start" } else { "prepare" });
        let output = tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dutchman-print"));
            command
                .arg("--config")
                .arg(config)
                .arg("--data-dir")
                .arg(data)
                .arg("print")
                .arg(input)
                .args(["--printer", "paper", "--copies", "2", "--json"]);
            if yes {
                command.arg("--yes");
            }
            command.stdin(std::process::Stdio::null()).output().unwrap()
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
    assert_eq!(
        fake.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| **c == 2)
            .count(),
        1
    );
}

#[tokio::test]
async fn multipart_document_summary_keeps_filename_when_display_name_is_different() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let mut body=b"--test\r\nContent-Disposition: form-data; name=\"printer\"\r\n\r\npaper\r\n--test\r\nContent-Disposition: form-data; name=\"copies\"\r\n\r\n2\r\n--test\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nDisplay name\r\n--test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"drawing.pdf\"\r\nContent-Type: application/pdf\r\n\r\n".to_vec();
    body.extend(PDF);
    body.extend(b"\r\n--test--\r\n");
    let (status, result) = send(
        &app,
        "POST",
        "/api/v1/jobs",
        "multipart/form-data; boundary=test",
        body,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    assert_eq!(result["name"], "Display name");
    let c = challenge(&app, "start", result["id"].as_str().unwrap()).await;
    assert!(c["summary"]
        .as_str()
        .unwrap()
        .contains("drawing.pdf' (2 copies)"));
}

#[test]
fn codec_localised_strings_out_of_band_and_fixed_width_values() {
    let mut language = vec![0, 2, b'e', b'n', 0, 7];
    language.extend(b"Drawing");
    let attrs = vec![
        A {
            name: "localized-name".into(),
            values: vec![(0x36, language.clone())],
        },
        A {
            name: "localized-text".into(),
            values: vec![(0x35, language)],
        },
        A::text("text", 0x41, "Drawing"),
        A {
            name: "no-value".into(),
            values: vec![(0x13, vec![])],
        },
        A {
            name: "unsupported".into(),
            values: vec![(0x10, vec![])],
        },
        A {
            name: "date".into(),
            values: vec![(0x31, vec![0; 11])],
        },
        A {
            name: "resolution".into(),
            values: vec![(0x32, vec![0; 9])],
        },
    ];
    let message = Message {
        code: 0,
        request_id: 1,
        groups: vec![
            op(),
            Group {
                tag: 4,
                attributes: attrs,
            },
        ],
    };
    let wire = message.encode().unwrap();
    let (decoded, _) = Message::decode(&wire).unwrap();
    assert_eq!(decoded, message);
    assert_eq!(
        decoded.groups[1]
            .get("localized-name")
            .unwrap()
            .texts(0x42)
            .unwrap(),
        vec!["Drawing"]
    );
    assert_eq!(
        decoded.groups[1]
            .get("localized-text")
            .unwrap()
            .texts(0x41)
            .unwrap(),
        vec!["Drawing"]
    );
}

#[tokio::test]
async fn named_media_must_match_the_advertised_name_and_tag() {
    let fake = Fake::new().await;
    let client = dutchman_print::ipp::Client::new(fake.config().printers.remove(0)).unwrap();
    let mut attributes = client.snapshot().await.unwrap().attributes;
    *attributes
        .attributes
        .iter_mut()
        .find(|a| a.name == "media-supported")
        .unwrap() = A {
        name: "media-supported".into(),
        values: vec![
            (0x44, b"iso_a4_210x297mm".to_vec()),
            (0x42, b"Drawing stock".to_vec()),
        ],
    };
    let message = Message {
        code: 0,
        request_id: 1,
        groups: vec![op(), attributes.clone()],
    };
    let wire = message.encode().unwrap();
    assert_eq!(Message::decode(&wire).unwrap().0, message);
    let options = Options {
        copies: Some(2),
        media: Some("Drawing stock".into()),
        ..Default::default()
    };
    options.validate().unwrap();
    let template = client.template(&attributes, &options).unwrap();
    assert_eq!(
        template.iter().find(|a| a.name == "media").unwrap().values[0].0,
        0x42
    );
    assert!(client
        .template(
            &attributes,
            &Options {
                media: Some("Other stock".into()),
                ..Default::default()
            }
        )
        .is_err());
}

#[tokio::test]
async fn lost_reply_reconciles_retained_completion_and_never_restarts_missing_history() {
    for retained in [true, false] {
        let fake = Fake::new().await;
        {
            let mut data = fake.state.lock().unwrap();
            data.lost = true;
            data.submitted_state = 9;
            data.retain_submission = retained;
        }
        let dir = tempfile::tempdir().unwrap();
        let app = fake.app(dir.path());
        let id = prepare(&app).await;
        assert_eq!(start(&app, &id).await.0, StatusCode::CONFLICT);
        let app = fake.app(dir.path());
        let (status, result) = send(
            &app,
            "GET",
            &format!("/api/v1/jobs/{id}"),
            "application/json",
            "",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["state"], if retained { "done" } else { "ready" });
        let (status, _) = send(
            &app,
            "POST",
            "/api/v1/confirmations",
            "application/json",
            json!({"action":"start","printer":"paper","job":id}).to_string(),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            fake.state
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| **c == 2)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn changed_pdf_invalidates_its_fresh_challenge_without_submission() {
    let fake = Fake::new().await;
    let dir = tempfile::tempdir().unwrap();
    let app = fake.app(dir.path());
    let id = prepare(&app).await;
    let c = challenge(&app, "start", &id).await;
    std::fs::write(dir.path().join(&id).join("input.pdf"), b"%PDF-1.7\nchanged").unwrap();
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/jobs/{id}/start"),
        "application/json",
        json!({"confirmation":c["id"]}).to_string(),
        Some('a'),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!fake.state.lock().unwrap().calls.contains(&2));
}
