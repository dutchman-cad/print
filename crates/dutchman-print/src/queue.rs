//! Durable FIFO proposals and explicitly confirmed dispatch. No background dispatch.
use super::*;
use crate::backend::{JobState, Printer, PrinterState};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Entry {
    pub id: String,
    pub job: String,
    pub target: String,
    pub profile: Option<String>,
    pub kind: Option<String>,
    pub reason: Option<String>,
    pub prepared: Option<String>,
    pub attempted: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Add {
    job: String,
    target: String,
    profile: Option<String>,
    kind: Option<String>,
}
#[derive(Serialize)]
pub(super) struct Proposal {
    id: String,
    printer: Option<String>,
    reason: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Move {
    position: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    printer: String,
}
fn reason(problem: &Problem) -> String {
    problem
        .1
        .char_indices()
        .take_while(|(offset, ch)| offset + ch.len_utf8() <= 1024)
        .map(|(_, ch)| ch)
        .collect()
}
fn missing() -> Problem {
    (
        StatusCode::NOT_FOUND,
        "Queue entry not found. List the queue again.".into(),
    )
}
fn refused(message: &str) -> Problem {
    (StatusCode::CONFLICT, message.into())
}
pub(super) async fn load(app: &App) -> Result<Vec<Entry>, Problem> {
    match crate::safety::read_state(&app.config.storage.data_dir.join("queue.json")).await {
        Ok(bytes) => {
            let entries: Vec<Entry> = serde_json::from_slice(&bytes).map_err(internal)?;
            if entries.len() > 5000 {
                return Err(internal("queue capacity"));
            }
            for entry in &entries {
                crate::validate_job_id(&entry.id).map_err(internal)?;
                crate::validate_job_id(&entry.job).map_err(internal)?;
                if let Some(id) = &entry.prepared {
                    crate::validate_job_id(id).map_err(internal)?;
                }
            }
            Ok(entries)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(internal(e)),
    }
}
async fn save(app: &App, entries: &[Entry]) -> Result<(), Problem> {
    let _write = app.actions.lock().await;
    let bytes = serde_json::to_vec(entries).map_err(internal)?;
    if bytes.len() > 16 << 20 {
        return Err(internal("queue state capacity"));
    }
    crate::safety::write_state(&app.config.storage.data_dir.join("queue.json"), &bytes)
        .await
        .map_err(internal)
}
pub(super) async fn list(State(app): State<Shared>) -> Json<Vec<Entry>> {
    Json(app.queue.lock().await.clone())
}
pub(super) async fn add(
    State(app): State<Shared>,
    Json(input): Json<Add>,
) -> Result<impl IntoResponse, Problem> {
    crate::validate_job_id(&input.job).map_err(bad)?;
    for text in [&input.job, &input.target]
        .into_iter()
        .chain(input.profile.iter())
        .chain(input.kind.iter())
    {
        if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
            return Err(bad(
                "Use nonempty queue fields of at most 256 plain text bytes.",
            ));
        }
    }
    let mut entries = app.queue.lock().await;
    if entries.len() >= 5000 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The queue is full. Remove entries first.".into(),
        ));
    }
    if entries
        .iter()
        .any(|e| e.job == input.job || e.prepared.as_ref() == Some(&input.job))
    {
        return Err(refused("This job is already queued."));
    }
    let job = app.backend.job(&input.job).await.map_err(backend_error)?;
    if job.state != JobState::Ready || !app.saved.lock().await.contains_key(&input.job) {
        return Err(refused("Queue a prepared, ready service job."));
    }
    if input.target != "any" {
        app.backend
            .check_action(&input.target, "start")
            .map_err(backend_error)?;
        if app.backend.queue_profile(&input.target).is_none() {
            return Err(bad("Choose a configured printer or any."));
        }
    }
    let entry = Entry {
        id: crate::new_job_id().map_err(internal)?,
        job: input.job,
        target: input.target,
        profile: input.profile,
        kind: input.kind,
        reason: None,
        prepared: None,
        attempted: false,
    };
    entries.push(entry.clone());
    if let Err(error) = save(&app, &entries).await {
        entries.pop();
        return Err(error);
    }
    Ok((StatusCode::CREATED, Json(entry)))
}
pub(super) async fn remove(
    State(app): State<Shared>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let mut entries = app.queue.lock().await;
    let index = entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(missing)?;
    let removed = entries.remove(index);
    if let Err(error) = save(&app, &entries).await {
        entries.insert(index, removed);
        return Err(error);
    }
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn move_entry(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Json(input): Json<Move>,
) -> Result<Json<Vec<Entry>>, Problem> {
    let mut entries = app.queue.lock().await;
    let index = entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(missing)?;
    if input.position >= entries.len() {
        return Err(bad("Use a zero-based position inside the queue."));
    }
    let previous = entries.clone();
    let entry = entries.remove(index);
    entries.insert(input.position, entry);
    if let Err(error) = save(&app, &entries).await {
        *entries = previous;
        return Err(error);
    }
    Ok(Json(entries.clone()))
}
async fn matches(app: &App, entry: &Entry, printer: &Printer) -> bool {
    if entry.attempted
        || printer.state != PrinterState::Idle
        || printer.needs_clearing != Some(false)
        || !printer.controls.iter().any(|c| c == "start")
        || (entry.target != "any" && entry.target != printer.id)
    {
        return false;
    }
    if let Some(bound) = &entry.prepared {
        if app.backend.job_printer(bound).await.ok().as_deref() != Some(&printer.id) {
            return false;
        }
    }
    let Ok(job) = app.backend.job(&entry.job).await else {
        return false;
    };
    if job.state != JobState::Ready {
        return false;
    }
    let Some((source_kind, source)) = app.backend.queue_profile(&job.printer) else {
        return false;
    };
    let Some((kind, target)) = app.backend.queue_profile(&printer.id) else {
        return false;
    };
    let saved = app.saved.lock().await;
    let Some(saved) = saved.get(&entry.job) else {
        return false;
    };
    let profile = saved
        .options
        .printer_profile
        .as_ref()
        .or(source.printer_profile.as_ref());
    source_kind == kind
        && profile == target.printer_profile.as_ref()
        && saved
            .options
            .print_profile
            .as_ref()
            .or(source.print_profile.as_ref())
            == target.print_profile.as_ref()
        && saved
            .options
            .filament_profile
            .as_ref()
            .or(source.filament_profile.as_ref())
            == target.filament_profile.as_ref()
        && entry.profile.as_ref().is_none_or(|p| Some(p) == profile)
        && entry.kind.as_ref().is_none_or(|k| k == &kind)
}
/// Callers hold the queue lock throughout matching, serializing queue requests while
/// status reads wait for their timeouts (within the overall matching deadline).
/// Stop and other printer routes never wait on this lock; expiry uses `try_lock`.
async fn propose(app: &App, entries: &[Entry]) -> Result<Vec<Proposal>, Problem> {
    tokio::time::timeout(Duration::from_secs(30), propose_in(app, entries))
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Queue matching timed out. Read printer status and try proposals again.".into(),
            )
        })?
}
async fn propose_in(app: &App, entries: &[Entry]) -> Result<Vec<Proposal>, Problem> {
    let printers = app.backend.printers().await.map_err(backend_error)?;
    let mut used = HashSet::new();
    // Reserve every attempted entry before FIFO matching, including entries moved
    // behind unattempted work. Idle telemetry can lag a Start already in flight.
    for entry in entries.iter().filter(|entry| entry.attempted) {
        if let Some(job) = &entry.prepared {
            used.insert(app.backend.job_printer(job).await.map_err(backend_error)?);
        }
    }
    let mut proposals = Vec::new();
    for entry in entries {
        let mut selected = None;
        for printer in &printers {
            if !used.contains(&printer.id) && matches(app, entry, printer).await {
                used.insert(printer.id.clone());
                selected = Some(printer.id.clone());
                break;
            }
        }
        proposals.push(Proposal {
            id: entry.id.clone(),
            reason: if selected.is_none() {
                Some(entry.reason.clone().unwrap_or_else(|| {
                    "No compatible idle printer with a cleared bed is available.".into()
                }))
            } else {
                None
            },
            printer: selected,
        });
    }
    Ok(proposals)
}
pub(super) async fn proposals(State(app): State<Shared>) -> Result<Json<Vec<Proposal>>, Problem> {
    let entries = app.queue.lock().await;
    Ok(Json(propose(&app, &entries).await?))
}
pub(super) async fn challenge(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Json(input): Json<Selection>,
) -> Result<impl IntoResponse, Problem> {
    let mut entries = app.queue.lock().await;
    if !propose(&app, &entries)
        .await?
        .iter()
        .any(|p| p.id == id && p.printer.as_ref() == Some(&input.printer))
    {
        return Err(refused(
            "Choose this entry's current proposed printer and confirm again.",
        ));
    }
    let entry = entries
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(missing)?;
    let source = app.backend.job(&entry.job).await.map_err(backend_error)?;
    let bound = if let Some(bound) = &entry.prepared {
        if app
            .backend
            .job_printer(bound)
            .await
            .map_err(backend_error)?
            != input.printer
        {
            return Err(refused("This entry is already reviewed for another printer. Remove it and queue the original job again."));
        }
        bound.clone()
    } else if source.printer == input.printer {
        entry.job.clone()
    } else {
        if app.saved.lock().await.len() >= 5000 {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many saved jobs. Remove entries and wait for jobs to expire.".into(),
            ));
        }
        let options = app
            .saved
            .lock()
            .await
            .get(&entry.job)
            .ok_or_else(missing)?
            .options
            .clone();
        let storage = app.config.storage.clone();
        tokio::task::spawn_blocking(move || hardening::check_storage(&storage.data_dir, storage.max_data, storage.min_free))
            .await.map_err(internal)?.map_err(|_| (
                StatusCode::SERVICE_UNAVAILABLE,
                "The server has too little storage. Free space before preparing for another printer.".into(),
            ))?;
        if app
            .backend
            .review_digest(Some(&entry.job))
            .await
            .map_err(backend_error)?
            .is_none()
        {
            return Err(refused("Prepare and review the original job again."));
        }
        let new_id = crate::new_job_id().map_err(internal)?;
        let dir = crate::job_dir(&app.config.storage.data_dir, &new_id).map_err(internal)?;
        let preparation = async {
            tokio::fs::create_dir(&dir).await.map_err(internal)?;
            let path = dir.join("input.3mf");
            tokio::fs::copy(
                crate::job_dir(&app.config.storage.data_dir, &entry.job)
                    .map_err(internal)?
                    .join("input.3mf"),
                &path,
            )
            .await
            .map_err(internal)?;
            let checked = path.clone();
            let scratch = dir.join("check");
            let limits = app.config.storage.zip;
            tokio::task::spawn_blocking(move || {
                crate::upload::check_3mf(&checked, &scratch, limits)
            })
            .await
            .map_err(internal)?
            .map_err(bad)?;
            let _run = app.runs.acquire().await.map_err(internal)?;
            app.backend
                .prepare(&new_id, &path, &input.printer, options.clone())
                .await
                .map_err(backend_error)?;
            Ok::<(), Problem>(())
        }
        .await;
        if let Err(error) = preparation {
            let _ = app.backend.forget(&new_id).await;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(error);
        }
        app.saved.lock().await.insert(
            new_id.clone(),
            Saved {
                options,
                created: app.config.clock.now(),
                preview: None,
            },
        );
        new_id
    };
    entry.prepared = Some(bound.clone());
    checkpoint(&app).await?;
    save(&app, &entries).await?;
    let challenge = app
        .confirmations
        .issue(
            &app.backend,
            Intent {
                action: "start".into(),
                printer: input.printer,
                job: Some(bound),
            },
            app.config.clock.now(),
            app.config.confirmation_ttl,
        )
        .await
        .map_err(backend_error)?;
    Ok((StatusCode::CREATED, Json(challenge)))
}
pub(super) async fn dispatch(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Job>, Problem> {
    let mut entries = app.queue.lock().await;
    let index = entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(missing)?;
    let entry = entries.get(index).ok_or_else(missing)?.clone();
    let checked = async {
        confirmation_header(&headers)?;
        if entry.attempted {
            return Err(refused("Dispatch was already attempted. Read the tracked job and reconcile its outcome; never retry automatically."));
        }
        let bound = entry.prepared.as_deref().ok_or_else(|| refused("Request a queue confirmation before dispatch."))?;
        let printer = app.backend.job_printer(bound).await.map_err(backend_error)?;
        if !propose(&app, &entries).await?.iter().any(|p| p.id == id && p.printer.as_ref() == Some(&printer)) {
            return Err(refused("The proposal changed. Read fresh status and confirm again."));
        }
        confirm(&app, &headers, request, "start", bound).await?;
        if let Some(saved) = app.saved.lock().await.get_mut(bound) {
            saved.created = app.config.clock.now();
        }
        checkpoint(&app).await?;
        if let Some(current) = entries.get_mut(index) { current.attempted = true; }
        // Fail closed before any backend attempt, including across process loss.
        save(&app, &entries).await?;
        Ok(bound.to_string())
    }.await;
    let bound = match checked {
        Ok(bound) => bound,
        Err(error) => {
            if let Some(current) = entries.get_mut(index) {
                current.reason = Some(reason(&error));
            }
            save(&app, &entries).await?;
            return Err(error);
        }
    };
    // Keep queue and command locks independent while a printer waits for its outcome.
    drop(entries);
    let result = app.backend.start(&bound).await;
    checkpoint(&app).await?;
    let mut entries = app.queue.lock().await;
    match result {
        Ok(job) => {
            entries.retain(|e| e.id != id);
            save(&app, &entries).await?;
            crate::log_action("start", &job);
            Ok(Json(job))
        }
        Err(error) => {
            let error = backend_error(error);
            if let Some(current) = entries.iter_mut().find(|e| e.id == id) {
                current.reason = Some(reason(&error));
            }
            save(&app, &entries).await?;
            Err(error)
        }
    }
}
