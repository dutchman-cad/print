//! Individually configured printers and their prepared jobs.

use crate::backend::{Backend, Error, Estimate, Job, JobState, Options, Printer, Reply};
use crate::prusalink::{Client, Snapshot};
use crate::safety::{file_bytes, hash};
use axum::body::Bytes;
use axum::http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Serialize, Deserialize)]
struct Prepared {
    job: Job,
    path: PathBuf,
    digest: String,
    info: Value,
    remote: Option<String>,
    attempted: bool,
    filename: String,
    profiles: Value,
}
pub struct Printers {
    configs: Vec<crate::config::Printer>,
    clients: Vec<Option<Arc<Client>>>,
    bambu: BTreeMap<String, Arc<crate::bambu::Client>>,
    slicers: BTreeMap<String, crate::slicer::Config>,
    jobs: Mutex<BTreeMap<String, Prepared>>,
    store: Option<PathBuf>,
}
fn refuse(message: &str) -> Error {
    Error::Refused(message.into())
}
fn ready(snapshot: &Snapshot) -> Result<(), Error> {
    if !matches!(snapshot.printer.detail.as_deref(), Some("IDLE" | "READY"))
        || snapshot.job.is_some()
        || snapshot
            .status
            .get("transfer")
            .is_some_and(|v| !v.is_null())
    {
        return Err(refuse("Printer is not idle and ready. Read status and resolve active work or attention before starting."));
    }
    Ok(())
}
impl Printers {
    pub fn new(config: crate::config::Config) -> Result<Self, String> {
        Self::build(config, false)
    }
    pub fn one_shot(config: crate::config::Config) -> Result<Self, String> {
        Self::build(config, true)
    }
    fn build(config: crate::config::Config, oneshot: bool) -> Result<Self, String> {
        config.validate()?;
        let clients = config
            .printers
            .iter()
            .cloned()
            .map(|p| {
                if p.kind.as_deref() == Some("bambu") {
                    Ok(None)
                } else {
                    Client::new(p).map(|c| Some(Arc::new(c)))
                }
            })
            .collect::<Result<_, _>>()?;
        let bambu = config
            .printers
            .iter()
            .filter(|p| p.kind.as_deref() == Some("bambu"))
            .map(|p| {
                crate::bambu::Client::new(p.clone(), oneshot).map(|c| (p.id.clone(), Arc::new(c)))
            })
            .collect::<Result<_, _>>()?;
        let slicers = config
            .printers
            .iter()
            .map(|p| config.slicer_for(p).map(|s| (p.id.clone(), s.clone())))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            configs: config.printers,
            clients,
            bambu,
            slicers,
            jobs: Mutex::new(BTreeMap::new()),
            store: None,
        })
    }
    pub fn with_store(mut self, path: PathBuf) -> Self {
        self.store = Some(path);
        self
    }
    pub async fn restore(&self, path: &Path) -> Result<(), String> {
        match crate::safety::read_state(path).await {
            Ok(bytes) if bytes.len() <= 16 << 20 => {
                let jobs: BTreeMap<String, Prepared> =
                    serde_json::from_slice(&bytes).map_err(|_| "Cannot read printer job state.")?;
                if jobs.len() > 5000 {
                    return Err("Too many saved jobs.".into());
                }
                *self.jobs.lock().await = jobs;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("Cannot read printer job state.".into()),
        }
    }
    pub async fn save(&self, path: &Path) -> Result<(), String> {
        let jobs = self.jobs.lock().await;
        let bytes = serde_json::to_vec(&*jobs).map_err(|_| "Cannot save printer jobs.")?;
        crate::safety::write_state(path, &bytes)
            .await
            .map_err(|_| "Cannot save printer jobs.".into())
    }
    async fn checkpoint(&self, jobs: &BTreeMap<String, Prepared>) -> Result<(), Error> {
        if let Some(path) = &self.store {
            let bytes = serde_json::to_vec(jobs).map_err(|_| Error::Unavailable)?;
            crate::safety::write_state(path, &bytes)
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        Ok(())
    }
    pub async fn reconcile_all(&self) -> Result<(), String> {
        let mut jobs = self.jobs.lock().await;
        for target in jobs.values_mut().filter(|p| p.attempted) {
            // Offline printers remain tracked; controls still require live reconciliation.
            let _ = self.reconcile(target).await;
        }
        self.checkpoint(&jobs).await.map_err(|e| e.to_string())
    }

    fn profiles(&self, printer: &str) -> Result<Value, Error> {
        let p = self
            .configs
            .iter()
            .find(|p| p.id == printer)
            .ok_or(Error::NotFound)?;
        Ok(
            serde_json::json!({"model":p.model,"printer":p.printer_profile,"print":p.print_profile,"material":p.filament_profile}),
        )
    }
    fn client(&self, printer: &str) -> Result<&Client, Error> {
        self.configs
            .iter()
            .position(|p| p.id == printer)
            .and_then(|i| self.clients.get(i))
            .and_then(Option::as_deref)
            .ok_or(Error::NotFound)
    }
    async fn review_info(&self, client: &Client) -> Result<Value, Error> {
        let info = client.json("/api/v1/info").await?;
        let version = client.json("/api/version").await?;
        if info
            .get("serial")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            || info
                .get("nozzle_diameter")
                .and_then(Value::as_f64)
                .is_none_or(|v| v <= 0.0)
            || version
                .get("firmware")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(refuse("Machine identity, firmware, or nozzle information is missing. Readiness cannot be reviewed for this firmware yet."));
        }
        Ok(serde_json::json!({"info":info,"version":version}))
    }
    async fn reconcile(&self, prepared: &mut Prepared) -> Result<Snapshot, Error> {
        let snapshot = self.client(&prepared.job.printer)?.snapshot().await?;
        let filename = snapshot
            .job
            .as_ref()
            .and_then(|j| {
                j.pointer("/file/display_name")
                    .or_else(|| j.pointer("/file/name"))
            })
            .and_then(Value::as_str);
        if filename == Some(prepared.filename.as_str()) && prepared.attempted {
            if prepared.remote.is_none() {
                prepared.remote = snapshot.printer.job_id.clone();
            }
            if prepared.remote == snapshot.printer.job_id {
                prepared.job.state = match snapshot.printer.detail.as_deref() {
                    Some("PRINTING" | "BUSY") => JobState::Printing,
                    Some("PAUSED" | "ATTENTION") => JobState::Paused,
                    Some("FINISHED") => JobState::Done,
                    Some("STOPPED") => JobState::Cancelled,
                    Some("ERROR") => JobState::Failed,
                    _ => prepared.job.state,
                };
                prepared.job.progress = snapshot.printer.progress;
                prepared.job.seconds_left = snapshot.printer.seconds_left;
            }
        }
        Ok(snapshot)
    }
    async fn control(&self, printer: &str, action: &str, expected: JobState) -> Result<Job, Error> {
        if self.bambu.contains_key(printer) {
            return Err(crate::bambu::unsupported());
        }
        let live = self.client(printer)?.snapshot().await?;
        let mut jobs = self.jobs.lock().await;
        let target = jobs
            .values_mut()
            .find(|p| {
                p.job.printer == printer
                    && p.job.state == expected
                    && p.remote == live.printer.job_id
                    && p.remote.is_some()
            })
            .ok_or_else(|| {
                refuse("No matching tracked job. Read status to reconcile the printer.")
            })?;
        self.control_job(target, action).await
    }
    async fn control_job(&self, target: &mut Prepared, action: &str) -> Result<Job, Error> {
        let snapshot = self.reconcile(target).await?;
        let id = snapshot
            .printer
            .job_id
            .as_deref()
            .ok_or_else(|| refuse("No live job is reported. Read status to reconcile it."))?;
        if target.remote.as_deref() != Some(id)
            || snapshot
                .job
                .as_ref()
                .and_then(|j| {
                    j.pointer("/file/display_name")
                        .or_else(|| j.pointer("/file/name"))
                })
                .and_then(Value::as_str)
                != Some(target.filename.as_str())
        {
            return Err(refuse(
                "The live job differs from the reviewed job. No command was sent.",
            ));
        }
        let expected = match action {
            "pause" => "PRINTING",
            "resume" => "PAUSED",
            _ => snapshot.printer.detail.as_deref().unwrap_or_default(),
        };
        if snapshot.printer.detail.as_deref() != Some(expected)
            || !matches!(expected, "PRINTING" | "PAUSED" | "ATTENTION" | "BUSY")
        {
            return Err(refuse("Printer readiness is unknown or this transition is unavailable. Read fresh status."));
        }
        let path = if action == "cancel" {
            format!("/api/v1/job/{id}")
        } else {
            format!("/api/v1/job/{id}/{action}")
        };
        let client = self.client(&target.job.printer)?;
        let method = if action == "cancel" {
            Method::DELETE
        } else {
            Method::PUT
        };
        let response = client.request(method, &path, Bytes::new(), false).await;
        let fresh = self.reconcile(target).await;
        let (code, _) = response?;
        if code != StatusCode::NO_CONTENT {
            return Err(refuse(
                "Printer refused the job command. Read status before another action.",
            ));
        }
        let fresh = fresh?;
        // A vanished job is not proof of cancellation.
        if action == "cancel"
            && (fresh.printer.job_id != target.remote || target.job.state != JobState::Cancelled)
        {
            return Err(refuse(
                "Cancellation is not confirmed. Read status to reconcile the job.",
            ));
        }
        let expected_state = match action {
            "pause" => JobState::Paused,
            "resume" => JobState::Printing,
            _ => JobState::Cancelled,
        };
        if target.job.state != expected_state {
            return Err(refuse(
                "Command outcome is not confirmed. Read fresh status.",
            ));
        }
        Ok(target.job.clone())
    }
}
impl Backend for Printers {
    fn review_action<'a>(
        &'a self,
        action: &'a str,
        printer: &'a str,
        job: Option<&'a str>,
    ) -> Reply<'a, ()> {
        Box::pin(async move {
            if action == "clear" {
                return Ok(());
            }
            self.check_control(printer)?;
            let snapshot = self.client(printer)?.snapshot().await?;
            let jobs = self.jobs.lock().await;
            let target = if let Some(id) = job {
                jobs.get(id)
            } else {
                jobs.values().find(|p| {
                    p.job.printer == printer
                        && p.remote.is_some()
                        && p.remote == snapshot.printer.job_id
                })
            }
            .ok_or_else(|| {
                refuse("No matching tracked job. Read status to reconcile the printer.")
            })?;
            if action == "start" {
                ready(&snapshot)?;
                if target.attempted || target.job.state != JobState::Ready {
                    return Err(refuse(
                        "Start was already attempted. Read status; do not repeat Start.",
                    ));
                }
                let bytes = file_bytes(&target.path).await?;
                let info = self.review_info(self.client(printer)?).await?;
                check_nozzle(&bytes, &info)?;
                if hash(&bytes) != target.digest
                    || info != target.info
                    || self.profiles(printer)? != target.profiles
                {
                    return Err(refuse(
                        "Prepared file or printer changed. Prepare and review again.",
                    ));
                }
            } else if target.attempted {
                let filename = snapshot
                    .job
                    .as_ref()
                    .and_then(|j| {
                        j.pointer("/file/display_name")
                            .or_else(|| j.pointer("/file/name"))
                    })
                    .and_then(Value::as_str);
                if target.remote.is_none()
                    || target.remote != snapshot.printer.job_id
                    || filename != Some(target.filename.as_str())
                {
                    return Err(refuse(
                        "The live job differs from the reviewed job. No command was sent.",
                    ));
                }
            }
            Ok(())
        })
    }
    fn printer<'a>(&'a self, id: &'a str) -> Reply<'a, Printer> {
        Box::pin(async move {
            if let Some(bambu) = self.bambu.get(id) {
                return Ok(bambu.snapshot().await);
            }
            let client = self.client(id)?;
            Ok(match client.snapshot().await {
                Ok(snapshot) => snapshot.printer,
                Err(e) => client.offline(&e),
            })
        })
    }
    fn restore_jobs<'a>(&'a self, path: &'a Path) -> Reply<'a, ()> {
        Box::pin(async move {
            self.restore(path).await.map_err(Error::Failed)?;
            self.reconcile_all().await.map_err(Error::Failed)?;
            Ok(())
        })
    }
    fn persist_jobs<'a>(&'a self, path: &'a Path) -> Reply<'a, ()> {
        Box::pin(async move { self.save(path).await.map_err(Error::Failed) })
    }
    fn bed_ready(&self, printer: &str, detail: Option<&str>) -> bool {
        self.client(printer).is_ok() && detail == Some("READY")
    }
    fn check_control(&self, printer: &str) -> Result<(), Error> {
        if self.bambu.contains_key(printer) {
            Err(crate::bambu::unsupported())
        } else {
            Ok(())
        }
    }

    fn latches_terminal_reports(&self, printer: &str) -> bool {
        self.bambu.contains_key(printer)
    }
    fn clear<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            if let Some(client) = self.bambu.get(id) {
                client.acknowledge();
            }
            Ok(())
        })
    }
    fn printers(&self) -> Reply<'_, Vec<Printer>> {
        Box::pin(async {
            let mut tasks = tokio::task::JoinSet::new();
            for (index, config) in self.configs.iter().enumerate() {
                if let Some(client) = self.bambu.get(&config.id).cloned() {
                    tasks.spawn(async move { (index, client.snapshot().await) });
                } else if let Some(client) = self.clients.get(index).and_then(Clone::clone) {
                    tasks.spawn(async move {
                        (
                            index,
                            match client.snapshot().await {
                                Ok(s) => s.printer,
                                Err(e) => client.offline(&e),
                            },
                        )
                    });
                }
            }
            let mut printers = BTreeMap::new();
            while let Some(result) = tasks.join_next().await {
                if let Ok((index, printer)) = result {
                    printers.insert(index, printer);
                }
            }
            if printers.len() != self.configs.len() {
                return Err(Error::Unavailable);
            }
            Ok(printers.into_values().collect())
        })
    }
    fn prepare<'a>(
        &'a self,
        id: &'a str,
        path: &'a Path,
        printer: &'a str,
        options: Options,
    ) -> Reply<'a, Estimate> {
        Box::pin(async move {
            let profile = self
                .configs
                .iter()
                .find(|p| p.id == printer)
                .ok_or(Error::NotFound)?;
            for (label, selected, configured) in [
                (
                    "printer",
                    &options.printer_profile,
                    &profile.printer_profile,
                ),
                ("print", &options.print_profile, &profile.print_profile),
                (
                    "material",
                    &options.filament_profile,
                    &profile.filament_profile,
                ),
            ] {
                if selected.is_some() && selected != configured {
                    return Err(Error::Refused(format!("The {label} profile differs from this printer's configured profile. Review its configuration first.")));
                }
            }
            if let Some(client) = self.bambu.get(printer) {
                let status = client.snapshot().await;
                if status.state == crate::backend::PrinterState::Offline {
                    return Err(Error::Unavailable);
                }
                let slicer = crate::slicer::Slicer::new(
                    self.slicers.get(printer).ok_or(Error::NotFound)?.clone(),
                )
                .map_err(|_| {
                    Error::Failed(
                        "Cannot find OrcaSlicer. Set slicer.path to its executable.".into(),
                    )
                })?;
                let output = slicer
                    .prepare(path, profile, &options, crate::slicer::OutputKind::ThreeMf)
                    .await
                    .map_err(|_| {
                        Error::Failed(
                            "Cannot slice this job. Check the configured OrcaSlicer profiles."
                                .into(),
                        )
                    })?;
                let bytes = file_bytes(&output.path).await?;
                let mut jobs = self.jobs.lock().await;
                if jobs.len() >= 5000 || jobs.contains_key(id) {
                    return Err(refuse("Job storage is full or this id is already in use."));
                }
                jobs.insert(
                    id.into(),
                    Prepared {
                        job: Job {
                            id: id.into(),
                            printer: printer.into(),
                            name: options.name.unwrap_or_else(|| "Print job".into()),
                            state: JobState::Ready,
                            estimate: output.estimate.clone(),
                            progress: None,
                            seconds_left: None,
                        },
                        path: output.path,
                        digest: hash(&bytes),
                        info: Value::Null,
                        remote: None,
                        attempted: false,
                        filename: "sliced.3mf".into(),
                        profiles: self.profiles(printer)?,
                    },
                );
                return Ok(output.estimate);
            }
            let client = self.client(printer)?;
            client.snapshot().await?;
            let info = self.review_info(client).await?;
            if info.pointer("/info/mmu").and_then(Value::as_bool) != Some(false) {
                return Err(refuse("Material slot assignments are not supported yet. Use a commissioned single-material printer."));
            }
            let slicer = crate::slicer::Slicer::new(
                self.slicers.get(printer).ok_or(Error::NotFound)?.clone(),
            )
            .map_err(|_| {
                Error::Failed("Cannot find PrusaSlicer. Set slicer.path to its executable.".into())
            })?;
            let output = slicer
                .prepare(path, profile, &options, crate::slicer::OutputKind::Gcode)
                .await
                .map_err(|_| {
                    Error::Failed(
                        "Cannot slice this job. Check the configured PrusaSlicer profiles.".into(),
                    )
                })?;
            let bytes = file_bytes(&output.path).await?;
            if bytes.starts_with(b"GCDE") || bytes.contains(&0) || !bytes.is_ascii() {
                return Err(refuse(
                    "Use text G-code. Binary G-code support is not established for this printer.",
                ));
            }
            check_nozzle(&bytes, &info)?;
            let filename = format!("dutchman-{}.gcode", hash(id.as_bytes()));
            let job = Job {
                id: id.into(),
                printer: printer.into(),
                name: options.name.unwrap_or_else(|| "Print job".into()),
                state: JobState::Ready,
                estimate: output.estimate.clone(),
                progress: None,
                seconds_left: None,
            };
            let mut jobs = self.jobs.lock().await;
            if jobs.len() >= 5000 || jobs.contains_key(id) {
                return Err(refuse("Job storage is full or this id is already in use."));
            }
            jobs.insert(
                id.into(),
                Prepared {
                    job,
                    path: output.path,
                    digest: hash(&bytes),
                    info,
                    remote: None,
                    attempted: false,
                    filename,
                    profiles: self.profiles(printer)?,
                },
            );
            self.checkpoint(&jobs).await?;
            Ok(output.estimate)
        })
    }
    fn start<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut jobs = self.jobs.lock().await;
            let target = jobs.get_mut(id).ok_or(Error::NotFound)?;
            if self.bambu.contains_key(&target.job.printer) {
                return Err(crate::bambu::unsupported());
            }
            if target.attempted || target.job.state != JobState::Ready {
                return Err(refuse("Start was already attempted. Read status and reconcile the intended file and job; do not repeat Start."));
            }
            let client = self.client(&target.job.printer)?;
            if self.profiles(&target.job.printer)? != target.profiles {
                return Err(refuse("Printer model, tooling, material, or plate profiles changed. Prepare and review again."));
            }
            ready(&client.snapshot().await?)?;
            let current_info = self.review_info(client).await?;
            let bytes = file_bytes(&target.path).await?;
            if hash(&bytes) != target.digest {
                return Err(refuse(
                    "Prepared G-code changed. Prepare and review the job again.",
                ));
            }
            check_nozzle(&bytes, &current_info)?;
            if current_info != target.info {
                return Err(refuse("Machine identity, firmware, or tooling changed. Prepare and review the job again."));
            }
            let storage = client.json("/api/v1/storage").await?;
            let entry = storage.get("storage_list").and_then(Value::as_array).and_then(|list| list.iter().find(|s| {
            s.get("available").and_then(Value::as_bool) == Some(true)
                && s.get("read_only").and_then(Value::as_bool) == Some(false)
                && s.get("free_space").and_then(Value::as_u64).is_some_and(|free| free >= bytes.len() as u64)
        })).ok_or_else(|| refuse("No writable storage with enough reported free space. Check printer storage."))?;
            let storage = entry
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_matches('/');
            if storage.is_empty()
                || !storage
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                return Err(refuse("Printer storage path is not supported yet."));
            }
            let path = format!("/api/v1/files/{storage}/{}", target.filename);
            ready(&client.snapshot().await?)?;
            let upload = client
                .request(Method::PUT, &path, bytes.clone(), true)
                .await;
            let (code, _) = match upload {
                Ok(v) => v,
                Err(e) => {
                    let _ = client.snapshot().await;
                    return Err(e);
                }
            };
            if code != StatusCode::CREATED {
                let _ = client.snapshot().await;
                let _ = client.json("/api/v1/storage").await;
                return Err(refuse(
                    "Printer refused the upload. Check storage and existing files before retrying.",
                ));
            }
            let uploaded = client.json(&path).await?;
            if uploaded.get("type").and_then(Value::as_str) != Some("PRINT_FILE")
                || uploaded.get("size").and_then(Value::as_u64) != Some(bytes.len() as u64)
                || uploaded
                    .get("display_name")
                    .or_else(|| uploaded.get("name"))
                    .and_then(Value::as_str)
                    != Some(target.filename.as_str())
            {
                return Err(refuse(
                    "Uploaded file validation failed. Review the stored file before starting.",
                ));
            }
            ready(&client.snapshot().await?)?;
            if self.review_info(client).await? != target.info {
                return Err(refuse("Machine identity, firmware, or tooling changed during upload. Prepare and review again."));
            }
            ready(&client.snapshot().await?)?;
            target.attempted = true;
            self.checkpoint(&jobs).await?;
            let target = jobs.get_mut(id).ok_or(Error::NotFound)?;
            let reply = client
                .request(Method::POST, &path, Bytes::new(), false)
                .await;
            let fresh = self.reconcile(target).await;
            let (code, _) = match reply {
                Ok(value) => value,
                Err(Error::Authentication) => return Err(Error::Authentication),
                Err(_) => return Err(refuse("Start reply was lost. Read status to reconcile the intended file and job. Start will not be repeated.")),
            };
            if code != StatusCode::NO_CONTENT {
                let _ = client.json("/api/v1/storage").await;
                return Err(refuse("Printer refused Start. Read status to reconcile the job. Start will not be repeated."));
            }
            fresh?;
            if target.job.state != JobState::Printing {
                return Err(refuse(
                    "Start is not confirmed. Read status to reconcile the intended file and job.",
                ));
            }
            Ok(target.job.clone())
        })
    }
    fn cancel<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut jobs = self.jobs.lock().await;
            let target = jobs.get_mut(id).ok_or(Error::NotFound)?;
            if self.bambu.contains_key(&target.job.printer) {
                return Err(crate::bambu::unsupported());
            }
            if !target.attempted && target.job.state == JobState::Ready {
                target.job.state = JobState::Cancelled;
                return Ok(target.job.clone());
            }
            self.control_job(target, "cancel").await
        })
    }
    fn pause<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move { self.control(printer, "pause", JobState::Printing).await })
    }
    fn resume<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move { self.control(printer, "resume", JobState::Paused).await })
    }
    fn job<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut jobs = self.jobs.lock().await;
            let target = jobs.get_mut(id).ok_or(Error::NotFound)?;
            if target.attempted {
                self.reconcile(target).await?;
            }
            Ok(target.job.clone())
        })
    }
    fn forget<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            self.jobs.lock().await.remove(id);
            Ok(())
        })
    }
}

// PrusaSlicer's text output includes the resolved printer setting in its comments.
fn check_nozzle(bytes: &[u8], info: &Value) -> Result<(), Error> {
    let mut values = Vec::new();
    for region in [
        bytes.get(..bytes.len().min(256 << 10)),
        bytes.get(bytes.len().saturating_sub(256 << 10)..),
    ]
    .into_iter()
    .flatten()
    {
        let text = String::from_utf8_lossy(region);
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("; nozzle_diameter =") {
                let value = value.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0)
                    .ok_or_else(|| refuse("Cannot verify the sliced nozzle diameter. Use a single-nozzle PrusaSlicer profile."))?;
                values.push(value);
            }
        }
    }
    let sliced = values.first().copied().ok_or_else(|| refuse("Sliced nozzle diameter is missing. Export PrusaSlicer text G-code with its settings comments."))?;
    let installed = info
        .pointer("/info/nozzle_diameter")
        .and_then(Value::as_f64)
        .ok_or(Error::Unavailable)?;
    if values.iter().any(|v| (*v - sliced).abs() > 0.0001) || (installed - sliced).abs() > 0.0001 {
        return Err(Error::Refused(format!("Installed nozzle is {installed} mm; sliced profile requires {sliced} mm. Fit the matching nozzle or slice with the matching profile.")));
    }
    Ok(())
}
