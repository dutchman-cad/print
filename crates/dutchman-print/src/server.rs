//! Private job storage and the plugin HTTP API.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dutchman_serve::{hardening, upload};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, Semaphore};

use crate::backend::{Backend, Error, Job, Options};
use crate::confirmation::{Confirmations, Intent};
use crate::simulated::{Clock, WallClock};

/// Upload, storage, expiry, and preparation limits.
#[derive(Clone, Debug)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
    pub max_upload: u64,
    pub uploads: usize,
    pub max_data: u64,
    pub min_free: u64,
    pub zip: upload::ZipLimits,
    pub keep: Duration,
    pub runs: usize,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: std::env::temp_dir().join("dutchman-print"),
            max_upload: 200 << 20,
            uploads: 4,
            max_data: 10 << 30,
            min_free: 1 << 30,
            zip: upload::ZipLimits {
                entries: 5_000,
                bytes: 2 << 30,
            },
            keep: Duration::from_secs(60 * 60),
            runs: 2,
        }
    }
}

pub struct ServerConfig {
    pub login: Option<(String, String)>,
    pub public_origin: Vec<String>,
    /// Upload, disk, expiry, and concurrency limits, with Dutchman Web's defaults.
    pub storage: StorageConfig,
    pub clock: Arc<dyn Clock>,
    pub clearance_interval: Duration,
    pub clearance_expiry: Duration,
    pub confirmation_ttl: Duration,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            login: None,
            public_origin: Vec::new(),
            storage: StorageConfig::default(),
            clock: Arc::new(WallClock),
            clearance_interval: Duration::from_secs(5),
            clearance_expiry: Duration::from_secs(900),
            confirmation_ttl: Duration::from_secs(60),
        }
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    created: Duration,
    preview: Option<PathBuf>,
}
struct App {
    backend: Arc<crate::safety::Safety>,
    raw: Arc<dyn Backend>,
    initialized: Mutex<bool>,
    actions: Mutex<()>,
    config: ServerConfig,
    confirmations: Confirmations,
    saved: Mutex<HashMap<String, Saved>>,
    uploads: Semaphore,
    runs: Semaphore,
}
type Shared = Arc<App>;
type Problem = (StatusCode, String);
fn bad(message: impl Into<String>) -> Problem {
    (StatusCode::BAD_REQUEST, message.into())
}
fn internal(_: impl std::fmt::Display) -> Problem {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Cannot keep the job files. Check the private data folder.".into(),
    )
}
fn backend_error(error: Error) -> Problem {
    let status = match &error {
        Error::NotFound => StatusCode::NOT_FOUND,
        Error::Busy | Error::Refused(_) => StatusCode::CONFLICT,
        Error::Unavailable | Error::Authentication => StatusCode::SERVICE_UNAVAILABLE,
        Error::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string())
}

/// The caller prepares the private data folder before serving.
pub fn router(backend: Arc<dyn Backend>, config: ServerConfig) -> Router {
    let limit = usize::try_from(config.storage.max_upload)
        .unwrap_or(usize::MAX)
        .saturating_add(1 << 20);
    let safety = Arc::new(
        crate::safety::Safety::new(backend.clone()).timing(
            config.clock.clone(),
            config
                .clearance_interval
                .clamp(Duration::from_secs(2), Duration::from_secs(30)),
            config
                .clearance_expiry
                .clamp(Duration::from_secs(60), Duration::from_secs(7200)),
        ),
    );
    safety.monitor();
    let confirmations =
        Confirmations::new(Some(config.storage.data_dir.join("confirmations.json")));
    let app = Arc::new(App {
        backend: safety,
        raw: backend,
        initialized: Mutex::new(false),
        actions: Mutex::new(()),
        uploads: Semaphore::new(config.storage.uploads.clamp(1, Semaphore::MAX_PERMITS)),
        runs: Semaphore::new(config.storage.runs.clamp(1, Semaphore::MAX_PERMITS)),
        config,
        confirmations,
        saved: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(serde_json::json!({"service": "dutchman-print", "api": 1})) }),
        )
        .route("/api/v1/printers", get(printers))
        .route(
            "/api/v1/confirmations",
            post(challenge).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/api/v1/jobs", post(create))
        .route("/api/v1/jobs/{id}", get(job))
        .route("/api/v1/jobs/{id}/preview.png", get(preview))
        .route("/api/v1/jobs/{id}/start", post(start))
        .route("/api/v1/jobs/{id}/cancel", post(cancel))
        .route("/api/v1/printers/{id}/stop", post(stop))
        .route("/api/v1/printers/{id}/pause", post(pause))
        .route("/api/v1/printers/{id}/resume", post(resume))
        .route("/api/v1/printers/{id}/clear", post(clear))
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .layer(middleware::from_fn(hardening::security_headers))
        .with_state(app)
}

async fn guard(State(app): State<Shared>, request: Request, next: Next) -> Response {
    let mut response = guard_in(&app, request, next).await;
    if response.status().is_client_error() || response.status().is_server_error() {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 64 << 10)
            .await
            .unwrap_or_default();
        let message = String::from_utf8_lossy(&bytes);
        let message = if message.is_empty() {
            parts.status.canonical_reason().unwrap_or("Request failed")
        } else {
            &message
        };
        response = (parts.status, Json(serde_json::json!({"error": message}))).into_response();
        for name in [header::WWW_AUTHENTICATE, header::ALLOW] {
            if let Some(value) = parts.headers.get(&name) {
                response.headers_mut().insert(name, value.clone());
            }
        }
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
async fn guard_in(app: &App, request: Request, next: Next) -> Response {
    if request.uri().path() == "/healthz" && request.method() == Method::GET {
        return next.run(request).await;
    }
    if request.method() == Method::POST
        && !hardening::allowed_origin(request.headers(), &app.config.public_origin)
    {
        return (
            StatusCode::FORBIDDEN,
            "Use this server's origin or a configured public origin.",
        )
            .into_response();
    }
    if let Some((user, password)) = &app.config.login {
        if !hardening::credentials(request.headers()).is_some_and(|(u, p)| {
            hardening::same(u.as_bytes(), user.as_bytes())
                & hardening::same(p.as_bytes(), password.as_bytes())
        }) {
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    "Basic realm=\"Dutchman Print\", charset=\"UTF-8\"",
                )],
                "Sign in to use Dutchman Print.",
            )
                .into_response();
        }
    }
    if let Err(problem) = initialize(app).await {
        return problem.into_response();
    }
    if let Err(problem) = expire(app).await {
        return problem.into_response();
    }
    next.run(request).await
}

async fn initialize(app: &App) -> Result<(), Problem> {
    let mut initialized = app.initialized.lock().await;
    if !*initialized {
        app.raw
            .restore_jobs(&app.config.storage.data_dir.join("jobs.json"))
            .await
            .map_err(backend_error)?;
        app.backend
            .restore(&app.config.storage.data_dir.join("safety.json"))
            .await
            .map_err(internal)?;
        match crate::safety::read_state(&app.config.storage.data_dir.join("saved.json")).await {
            Ok(bytes) => {
                *app.saved.lock().await = serde_json::from_slice(&bytes).map_err(internal)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(internal(e)),
        }
        *initialized = true;
    }
    Ok(())
}
async fn checkpoint(app: &App) -> Result<(), Problem> {
    let _write = app.actions.lock().await;
    app.raw
        .persist_jobs(&app.config.storage.data_dir.join("jobs.json"))
        .await
        .map_err(backend_error)?;
    app.backend
        .save(&app.config.storage.data_dir.join("safety.json"))
        .await
        .map_err(internal)?;
    let saved = app.saved.lock().await;
    let bytes = serde_json::to_vec(&*saved).map_err(internal)?;
    let temporary = app.config.storage.data_dir.join("saved.new");
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(internal)?;
    tokio::fs::rename(temporary, app.config.storage.data_dir.join("saved.json"))
        .await
        .map_err(internal)
}
async fn challenge(
    State(app): State<Shared>,
    Json(intent): Json<Intent>,
) -> Result<impl IntoResponse, Problem> {
    let challenge = app
        .confirmations
        .issue(
            &app.backend,
            intent,
            app.config.clock.now(),
            app.config.confirmation_ttl,
        )
        .await
        .map_err(backend_error)?;
    Ok((StatusCode::CREATED, Json(challenge)))
}

async fn expire(app: &App) -> Result<(), Problem> {
    let now = app.config.clock.now();
    let mut saved = app.saved.lock().await;
    let ids: Vec<_> = saved
        .iter()
        .filter(|(_, value)| now.saturating_sub(value.created) > app.config.storage.keep)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        app.backend.forget(&id).await.map_err(backend_error)?;
        let _ = tokio::fs::remove_dir_all(app.config.storage.data_dir.join(&id)).await;
        saved.remove(&id);
    }
    Ok(())
}

async fn printers(State(app): State<Shared>) -> Result<impl IntoResponse, Problem> {
    Ok(Json(app.backend.printers().await.map_err(backend_error)?))
}
async fn job(State(app): State<Shared>, Path(id): Path<String>) -> Result<Json<Job>, Problem> {
    Ok(Json(app.backend.job(&id).await.map_err(backend_error)?))
}
async fn create(State(app): State<Shared>, request: Request) -> Result<impl IntoResponse, Problem> {
    if app.saved.lock().await.len() >= 5000 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many saved jobs. Wait for jobs to expire.".into(),
        ));
    }
    let _upload = app.uploads.try_acquire().map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The server is receiving too many uploads. Try again later.".into(),
        )
    })?;
    let storage = app.config.storage.clone();
    tokio::task::spawn_blocking(move || {
        hardening::check_storage(&storage.data_dir, storage.max_data, storage.min_free)
    })
    .await
    .map_err(internal)?
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The server has too little storage for a new job. Try again after saved jobs expire."
                .into(),
        )
    })?;
    let id = crate::new_job_id().map_err(internal)?;
    let dir = app.config.storage.data_dir.join(&id);
    tokio::fs::create_dir(&dir).await.map_err(internal)?;
    let result = create_in(&app, &id, &dir, request).await;
    if result.is_err() {
        let _ = app.backend.forget(&id).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    result.map(|job| (StatusCode::CREATED, Json(job)))
}
fn too_big() -> Problem {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        "The upload is too big. Choose a smaller 3MF file.".into(),
    )
}

fn job_field(
    fields: &mut HashMap<String, String>,
    key: String,
    value: String,
) -> Result<(), Problem> {
    if !matches!(
        key.as_str(),
        "printer" | "printer_profile" | "print_profile" | "filament_profile" | "name"
    ) || fields.contains_key(&key)
    {
        return Err(bad("Use each documented job field once."));
    }
    if value.len() > 256 {
        return Err(bad("Job fields are limited to 256 bytes of plain text."));
    }
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(bad("Use nonempty plain text job fields."));
    }
    if matches!(
        key.as_str(),
        "printer_profile" | "print_profile" | "filament_profile"
    ) {
        crate::backend::validate_profile_name(&value).map_err(bad)?;
    }
    fields.insert(key, value);
    Ok(())
}

async fn create_in(
    app: &App,
    id: &str,
    dir: &std::path::Path,
    request: Request,
) -> Result<Job, Problem> {
    let path = dir.join("input.3mf");
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let raw = matches!(
        content_type.as_str(),
        "model/3mf" | "application/vnd.ms-package.3dmanufacturing-3dmodel+xml"
    );
    let mut fields = HashMap::new();
    let mut filename = None;
    if raw {
        let Query(query) = Query::<Vec<(String, String)>>::try_from_uri(request.uri())
            .map_err(|_| bad("Cannot read the job query fields."))?;
        for (key, value) in query {
            job_field(&mut fields, key, value)?;
        }
        let mut body = request.into_body();
        use http_body_util::BodyExt as _;
        let mut file = tokio::fs::File::create(&path).await.map_err(internal)?;
        let mut total = 0u64;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| bad("Cannot read the file upload."))?;
            if let Ok(chunk) = frame.into_data() {
                total = total.saturating_add(chunk.len() as u64);
                if total > app.config.storage.max_upload {
                    return Err(too_big());
                }
                file.write_all(&chunk).await.map_err(internal)?;
            }
        }
        file.flush().await.map_err(internal)?;
        filename = Some("input.3mf".to_owned());
    } else {
        if content_type != "multipart/form-data" {
            return Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Send a 3MF body with Content-Type: model/3mf, or use multipart/form-data.".into(),
            ));
        }
        let mut form = Multipart::from_request(request, &())
            .await
            .map_err(|_| bad("Cannot read the multipart upload."))?;
        let mut total = 0u64;
        while let Some(mut field) = form
            .next_field()
            .await
            .map_err(|err| (err.status(), "Cannot read the multipart upload.".into()))?
        {
            let key = field.name().unwrap_or_default().to_owned();
            if key == "file" {
                if filename.is_some() {
                    return Err(bad("Choose exactly one .3mf file."));
                }
                let name = upload::safe_name(
                    field
                        .file_name()
                        .ok_or_else(|| bad("Choose one .3mf file."))?,
                );
                if !std::path::Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("3mf"))
                {
                    return Err(bad("Choose one .3mf file."));
                }
                filename = Some(name);
                let mut file = tokio::fs::File::create(&path).await.map_err(internal)?;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|err| (err.status(), "Cannot read the file upload.".into()))?
                {
                    total = total.saturating_add(chunk.len() as u64);
                    if total > app.config.storage.max_upload {
                        return Err(too_big());
                    }
                    file.write_all(&chunk).await.map_err(internal)?;
                }
                file.flush().await.map_err(internal)?;
            } else {
                let mut bytes = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| bad("Cannot read a job field."))?
                {
                    if bytes.len().saturating_add(chunk.len()) > 256 {
                        return Err(bad("Job fields are limited to 256 bytes of plain text."));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let value =
                    String::from_utf8(bytes).map_err(|_| bad("Use plain UTF-8 job fields."))?;
                job_field(&mut fields, key, value)?;
            }
        }
    }
    let name = fields
        .remove("name")
        .or(filename)
        .ok_or_else(|| bad("Choose one .3mf file."))?;
    if !path.is_file() {
        return Err(bad("Choose one .3mf file."));
    }
    let printer = fields
        .remove("printer")
        .ok_or_else(|| bad("Choose a printer."))?;
    let checked = path.clone();
    let scratch = dir.join("check");
    let limits = app.config.storage.zip;
    tokio::task::spawn_blocking(move || crate::upload::check_3mf(&checked, &scratch, limits))
        .await
        .map_err(internal)?
        .map_err(bad)?;
    let options = Options {
        name: Some(name),
        printer_profile: fields.remove("printer_profile"),
        print_profile: fields.remove("print_profile"),
        filament_profile: fields.remove("filament_profile"),
    };
    let _run = app.runs.acquire().await.map_err(internal)?;
    let estimate = app
        .backend
        .prepare(id, &path, &printer, options)
        .await
        .map_err(backend_error)?;
    let preview = if let Some(source) = estimate.preview {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        tokio::fs::File::open(source)
            .await
            .map_err(internal)?
            .take((16 << 20) + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(internal)?;
        if bytes.len() > 16 << 20 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(internal("preview must be a bounded PNG"));
        }
        let target = dir.join("preview.png");
        tokio::fs::write(&target, bytes).await.map_err(internal)?;
        Some(target)
    } else {
        None
    };

    let mut saved = app.saved.lock().await;
    if saved.len() >= 5000 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many saved jobs. Wait for jobs to expire.".into(),
        ));
    }
    saved.insert(
        id.into(),
        Saved {
            created: app.config.clock.now(),
            preview,
        },
    );
    drop(saved);
    checkpoint(app).await?;
    app.backend.job(id).await.map_err(backend_error)
}

async fn preview(State(app): State<Shared>, Path(id): Path<String>) -> Result<Response, Problem> {
    let saved = app.saved.lock().await;
    let path = saved
        .get(&id)
        .and_then(|saved| saved.preview.as_ref())
        .ok_or_else(|| (StatusCode::NOT_FOUND, "This job has no preview.".into()))?;
    let metadata = tokio::fs::metadata(path).await.map_err(internal)?;
    if metadata.len() > 16 << 20 {
        return Err(internal("preview too big"));
    }
    let bytes = tokio::fs::read(path).await.map_err(internal)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], bytes).into_response())
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    confirmation: Option<String>,
}
async fn confirm(
    app: &App,
    headers: &HeaderMap,
    request: Request,
    action: &str,
    id: &str,
) -> Result<crate::confirmation::Observation, Problem> {
    let target = if matches!(action, "start" | "cancel") {
        app.backend
            .job_printer(id)
            .await
            .map(|printer| (printer, Some(id.into())))
    } else {
        Ok((id.into(), None))
    };
    if let Ok((printer, _)) = &target {
        app.backend
            .check_action(printer, action)
            .map_err(backend_error)?;
    }
    let error = || {
        (
            StatusCode::FORBIDDEN,
            "This action needs confirmation in SolidCraft (or --yes on the command line).".into(),
        )
    };
    let values: Vec<_> = headers.get_all("Plugin-Confirmation").iter().collect();
    if values.len() != 1 {
        return Err(error());
    }
    let token = values
        .first()
        .and_then(|v| v.to_str().ok())
        .ok_or_else(error)?;
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error());
    }
    let Query(query) = Query::<Confirmation>::try_from_uri(request.uri())
        .map_err(|_| bad("Use a confirmation id."))?;
    let bytes = axum::body::to_bytes(request.into_body(), 4096)
        .await
        .map_err(|_| bad("Use a small confirmation body."))?;
    let body: Confirmation = if bytes.is_empty() {
        Confirmation::default()
    } else {
        serde_json::from_slice(&bytes).map_err(|_| bad("Use a confirmation id in JSON."))?
    };
    if query.confirmation.is_some() && body.confirmation.is_some() {
        return Err(bad("Send confirmation once."));
    }
    let challenge = query.confirmation.or(body.confirmation).ok_or_else(error)?;
    let (printer, job) = target.map_err(backend_error)?;
    app.confirmations
        .consume(
            &app.backend,
            &challenge,
            &Intent {
                action: action.into(),
                printer,
                job,
            },
            Some(token),
            app.config.clock.now(),
        )
        .await
        .map_err(backend_error)
}
async fn action(
    app: &App,
    id: &str,
    headers: HeaderMap,
    request: Request,
    action: &str,
) -> Result<Json<Job>, Problem> {
    confirm(app, &headers, request, action, id).await?;
    let result = match action {
        "start" => app.backend.start(id).await,
        "cancel" => app.backend.cancel(id).await,
        "pause" => app.backend.pause(id).await,
        _ => app.backend.resume(id).await,
    };
    checkpoint(app).await?;
    let job = result.map_err(backend_error)?;
    crate::log_action(action, &job);
    Ok(Json(job))
}
async fn start(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Job>, Problem> {
    action(&app, &id, headers, request, "start").await
}
async fn cancel(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Job>, Problem> {
    action(&app, &id, headers, request, "cancel").await
}
async fn pause(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Job>, Problem> {
    action(&app, &id, headers, request, "pause").await
}
async fn resume(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Job>, Problem> {
    action(&app, &id, headers, request, "resume").await
}
async fn clear(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<StatusCode, Problem> {
    confirm(&app, &headers, request, "clear", &id).await?;
    app.backend.clear(&id).await.map_err(backend_error)?;
    checkpoint(&app).await?;
    crate::log_clear(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn stop(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<crate::backend::Printer>, Problem> {
    let observed = confirm(&app, &headers, request, "stop", &id).await?;
    let identity = observed
        .job_id
        .as_deref()
        .ok_or_else(|| bad("Read a live job before stopping."))?;
    let name = observed
        .job_name
        .as_deref()
        .ok_or_else(|| bad("Read a live job before stopping."))?;
    let result = app
        .backend
        .stop(&id, identity, name, observed.job_path.as_deref())
        .await;
    checkpoint(&app).await?;
    Ok(Json(result.map_err(backend_error)?))
}
