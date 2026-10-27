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
use tokio::sync::{Mutex, OwnedMutexGuard};

#[path = "printers_duet.rs"]
mod duet;
#[path = "printers_repetier.rs"]
mod repetier;
#[path = "printers_stop.rs"]
mod stop;

type MachineGuard = Option<OwnedMutexGuard<()>>;

#[derive(Clone, Serialize, Deserialize)]
struct Prepared {
    job: Job,
    path: PathBuf,
    digest: String,
    info: Value,
    remote: Option<String>,
    attempted: bool,
    filename: String,
    #[serde(default)]
    staged: Option<String>,
    profiles: Value,
    #[serde(default)]
    history: Value,
    #[serde(default)]
    cancelling: bool,
    #[serde(skip)]
    revision: u64,
    #[serde(skip)]
    incarnation: Arc<()>,
}
pub struct Printers {
    configs: Vec<crate::config::Printer>,
    clients: Vec<Option<Arc<Client>>>,
    bambu: BTreeMap<String, Arc<crate::bambu::Client>>,
    octoprint: BTreeMap<String, Arc<crate::octoprint::Client>>,
    moonraker: BTreeMap<String, Arc<crate::moonraker::Client>>,
    duet: BTreeMap<String, Arc<crate::duet::Client>>,
    repetier: BTreeMap<String, Arc<crate::repetier::Client>>,
    sdcp: BTreeMap<String, Arc<crate::elegoo_sdcp::Client>>,
    snapmaker: BTreeMap<String, Arc<crate::snapmaker::Client>>,
    slicers: BTreeMap<String, crate::slicer::Config>,
    jobs: Mutex<BTreeMap<String, Prepared>>,
    store: Option<PathBuf>,
    machines: BTreeMap<String, Arc<Mutex<()>>>,
    // Non-cancel operations remain ordered through their outcome waits. Stop bypasses
    // this gate and takes only the command mutex, which polls release.
    transitions: BTreeMap<String, Mutex<()>>,
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
                if matches!(
                    p.kind.as_deref(),
                    Some(
                        "bambu"
                            | "octoprint"
                            | "moonraker"
                            | "snapmaker-u1"
                            | "duet"
                            | "repetier"
                            | "elegoo-sdcp"
                            | "snapmaker-2"
                            | "snapmaker-j1"
                    )
                ) {
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
            .filter(|p| !crate::config::sliceless(p))
            .map(|p| config.slicer_for(p).map(|s| (p.id.clone(), s.clone())))
            .collect::<Result<_, _>>()?;
        let octoprint = config
            .printers
            .iter()
            .filter(|p| p.kind.as_deref() == Some("octoprint"))
            .map(|p| crate::octoprint::Client::new(p.clone()).map(|c| (p.id.clone(), Arc::new(c))))
            .collect::<Result<_, _>>()?;
        let moonraker = config
            .printers
            .iter()
            .filter(|p| matches!(p.kind.as_deref(), Some("moonraker" | "snapmaker-u1")))
            .map(|p| crate::moonraker::Client::new(p.clone()).map(|c| (p.id.clone(), Arc::new(c))))
            .collect::<Result<_, _>>()?;
        let duet = config
            .printers
            .iter()
            .filter(|p| p.kind.as_deref() == Some("duet"))
            .map(|p| crate::duet::Client::new(p.clone()).map(|c| (p.id.clone(), Arc::new(c))))
            .collect::<Result<_, _>>()?;
        let repetier = config
            .printers
            .iter()
            .filter(|p| p.kind.as_deref() == Some("repetier"))
            .map(|p| crate::repetier::Client::new(p.clone()).map(|c| (p.id.clone(), Arc::new(c))))
            .collect::<Result<_, _>>()?;
        let sdcp = config
            .printers
            .iter()
            .filter(|p| p.kind.as_deref() == Some("elegoo-sdcp"))
            .map(|p| {
                crate::elegoo_sdcp::Client::new(p.clone()).map(|c| (p.id.clone(), Arc::new(c)))
            })
            .collect::<Result<_, _>>()?;
        let snapmaker = config
            .printers
            .iter()
            .filter(|p| crate::snapmaker::is_kind(p.kind.as_deref()))
            .map(|p| crate::snapmaker::Client::new(p.clone()).map(|c| (p.id.clone(), Arc::new(c))))
            .collect::<Result<_, _>>()?;
        let machines = config
            .printers
            .iter()
            .map(|p| (p.id.clone(), Arc::new(Mutex::new(()))))
            .collect();
        let transitions = config
            .printers
            .iter()
            .map(|p| (p.id.clone(), Mutex::new(())))
            .collect();
        Ok(Self {
            machines,
            transitions,
            configs: config.printers,
            clients,
            bambu,
            octoprint,
            moonraker,
            duet,
            repetier,
            sdcp,
            snapmaker,
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
    async fn machine(&self, printer: &str) -> Result<MachineGuard, Error> {
        Ok(Some(
            self.machines
                .get(printer)
                .ok_or(Error::NotFound)?
                .clone()
                .lock_owned()
                .await,
        ))
    }
    async fn tracked(&self, id: &str) -> Result<Prepared, Error> {
        self.jobs
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or(Error::NotFound)
    }
    // A result belongs to the exact record read before the I/O. A concurrent observation,
    // cancellation, or replacement wins; never resurrect a forgotten job.
    async fn publish(&self, target: &mut Prepared) -> Result<(), Error> {
        let mut jobs = self.jobs.lock().await;
        let current = jobs.get_mut(&target.job.id).ok_or(Error::NotFound)?;
        if !Arc::ptr_eq(&current.incarnation, &target.incarnation) {
            return Err(Error::NotFound);
        }
        if current.revision == target.revision
            && (current.job.state != target.job.state
                || current.job.progress != target.job.progress
                || current.job.seconds_left != target.job.seconds_left
                || current.staged != target.staged
                || current.job.detail != target.job.detail
                || current.remote != target.remote
                || current.cancelling != target.cancelling
                || current.attempted != target.attempted
                || current.history != target.history)
        {
            target.revision = target.revision.wrapping_add(1);
            *current = target.clone();
            self.checkpoint(&jobs).await?;
        } else {
            *target = current.clone();
        }
        Ok(())
    }
    async fn attempted(&self, target: &mut Prepared) -> Result<(), Error> {
        let mut jobs = self.jobs.lock().await;
        let current = jobs.get_mut(&target.job.id).ok_or(Error::NotFound)?;
        if !Arc::ptr_eq(&current.incarnation, &target.incarnation) {
            return Err(Error::NotFound);
        }
        if current.attempted || current.job.state != JobState::Ready {
            return Err(refuse(
                "Start was already attempted or cancelled. Read status; do not repeat Start.",
            ));
        }
        target.attempted = true;
        target.revision = current.revision.wrapping_add(1);
        *current = target.clone();
        self.checkpoint(&jobs).await
    }
    async fn validate_control(&self, target: &Prepared) -> Result<(), Error> {
        let jobs = self.jobs.lock().await;
        let current = jobs.get(&target.job.id).ok_or(Error::NotFound)?;
        if !Arc::ptr_eq(&current.incarnation, &target.incarnation) {
            return Err(Error::NotFound);
        }
        if !matches!(
            current.job.state,
            JobState::Ready | JobState::Printing | JobState::Paused
        ) || (current.revision != target.revision
            && (current.job.state != target.job.state
                || current.remote != target.remote
                || current.cancelling != target.cancelling))
        {
            return Err(refuse(
                "The tracked job changed. Read fresh status before another action.",
            ));
        }
        Ok(())
    }
    async fn record_cancel(&self, target: &mut Prepared) -> Result<(), Error> {
        self.publish(target).await?;
        let mut jobs = self.jobs.lock().await;
        let current = jobs.get_mut(&target.job.id).ok_or(Error::NotFound)?;
        if !Arc::ptr_eq(&current.incarnation, &target.incarnation) {
            return Err(Error::NotFound);
        }
        current.cancelling = true;
        current.revision = current.revision.wrapping_add(1);
        *target = current.clone();
        self.checkpoint(&jobs).await
    }
    async fn poll_boundary(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<(), Error> {
        self.publish(target).await?;
        guard.take();
        Ok(())
    }
    async fn reconcile_job(&self, target: &mut Prepared) -> Result<(), Error> {
        if !matches!(
            target.job.state,
            JobState::Ready | JobState::Printing | JobState::Paused | JobState::Ended
        ) || self.sdcp.contains_key(&target.job.printer)
            || self.snapmaker.contains_key(&target.job.printer)
        {
            // The saved terminal result remains valid after its staged file is removed;
            // Elegoo SDCP and monitoring-only Snapmaker printers never have tracked jobs.
        } else if self.bambu.contains_key(&target.job.printer) {
            self.reconcile_bambu(target).await?;
        } else if self.octoprint.contains_key(&target.job.printer) {
            self.reconcile_octoprint(target).await?;
        } else if self.moonraker.contains_key(&target.job.printer) {
            self.reconcile_moonraker(target).await?;
        } else if self.duet.contains_key(&target.job.printer) {
            self.reconcile_duet(target).await?;
        } else if self.repetier.contains_key(&target.job.printer) {
            self.reconcile_repetier(target).await?;
        } else {
            self.reconcile(target).await?;
        }
        self.publish(target).await?;
        if !matches!(
            target.job.state,
            JobState::Ready | JobState::Printing | JobState::Paused
        ) && target.staged.is_some()
        {
            let _guard = self.machine(&target.job.printer).await?;
            *target = self.tracked(&target.job.id).await?;
            self.cleanup_staged(target).await;
            self.publish(target).await?;
        }
        Ok(())
    }
    pub async fn reconcile_all(&self) -> Result<(), String> {
        let targets: Vec<_> = self
            .jobs
            .lock()
            .await
            .values()
            .filter(|p| p.attempted)
            .cloned()
            .collect();
        for mut target in targets {
            // Offline printers remain tracked; controls still require live reconciliation.
            let _ = self.reconcile_job(&mut target).await;
        }
        self.checkpoint(&*self.jobs.lock().await)
            .await
            .map_err(|e| e.to_string())
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
    async fn reconcile_bambu(&self, target: &mut Prepared) -> Result<Printer, Error> {
        let client = self.bambu.get(&target.job.printer).ok_or(Error::NotFound)?;
        let live = client.snapshot().await;
        let name = target.filename.trim_end_matches(".gcode.3mf");
        if target.attempted
            && live.job_name.as_deref() == Some(name)
            && live.job_id.is_some()
            && (target.remote.is_none() || target.remote == live.job_id)
            && matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
            && live.state != crate::backend::PrinterState::Offline
        {
            // A terminal report can only finish an execution previously observed active.
            let active = matches!(target.job.state, JobState::Printing | JobState::Paused);
            target.job.state = match live.detail.as_deref().and_then(|d| d.split("; ").next()) {
                Some("RUNNING" | "PREPARE")
                    if live.state == crate::backend::PrinterState::Printing =>
                {
                    JobState::Printing
                }
                Some("PAUSE") => JobState::Paused,
                Some("FINISH") if active && live.state == crate::backend::PrinterState::Idle => {
                    JobState::Done
                }
                Some("FAILED") if active && target.cancelling => JobState::Cancelled,
                Some("FAILED") if active => JobState::Failed,
                _ => target.job.state,
            };
            if matches!(target.job.state, JobState::Printing | JobState::Paused) {
                target.remote = live.job_id.clone();
            }
            target.job.progress = live.progress;
            target.job.seconds_left = live.seconds_left;
        }
        Ok(live)
    }
    async fn bambu_control(
        &self,
        target: &mut Prepared,
        action: &str,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        let client = self.bambu.get(&target.job.printer).ok_or(Error::NotFound)?;
        client.check_control()?;
        let live = self.reconcile_bambu(target).await?;
        if !target.attempted
            || target.remote.is_none()
            || target.remote != live.job_id
            || live.state == crate::backend::PrinterState::Offline
            || !matches!(target.job.state, JobState::Printing | JobState::Paused)
            || (action == "resume"
                && live.detail.as_deref().and_then(|d| d.split("; ").next()) != Some("PAUSE"))
            || (action != "cancel"
                && !matches!(
                    (
                        action,
                        live.detail.as_deref().and_then(|d| d.split("; ").next())
                    ),
                    ("pause", Some("RUNNING")) | ("resume", Some("PAUSE"))
                ))
        {
            return Err(refuse(
                "No matching live job for this command. Read status to reconcile the printer.",
            ));
        }
        let (command, expected) = match action {
            "cancel" => ("stop", "FAILED"),
            "pause" => ("pause", "PAUSE"),
            _ => ("resume", "RUNNING"),
        };
        self.validate_control(target).await?;
        if action == "cancel" {
            self.record_cancel(target).await?;
        }
        self.publish(target).await?;
        let result = client
            .command(
                serde_json::json!({"print":{"command":command,"param":""}}),
                target.filename.trim_end_matches(".gcode.3mf"),
                expected,
                target.remote.as_deref(),
                guard,
            )
            .await;
        self.publish(target).await?;
        self.reconcile_bambu(target).await?;
        result?;
        if action == "cancel" {
            target.job.state = JobState::Cancelled;
        }
        Ok(target.job.clone())
    }
    async fn reconcile(&self, prepared: &mut Prepared) -> Result<Snapshot, Error> {
        let snapshot = self.client(&prepared.job.printer)?.snapshot().await?;
        self.observe_prusalink(prepared, &snapshot.printer);
        Ok(snapshot)
    }
    fn observe_prusalink(&self, prepared: &mut Prepared, live: &Printer) {
        if live.job_name.as_deref() == Some(prepared.filename.as_str())
            && prepared.attempted
            && matches!(
                prepared.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            if prepared.remote.is_none() {
                prepared.remote = live.job_id.clone();
            }
            if prepared.remote == live.job_id {
                prepared.job.state = match live.detail.as_deref() {
                    Some("PRINTING") => JobState::Printing,
                    Some("PAUSED") => JobState::Paused,
                    Some("FINISHED") => JobState::Done,
                    Some("STOPPED") => JobState::Cancelled,
                    Some("ERROR") => JobState::Failed,
                    _ => prepared.job.state,
                };
                prepared.job.progress = live.progress;
                prepared.job.seconds_left = live.seconds_left;
            }
        }
        if prepared.attempted
            && prepared.remote.is_some()
            && prepared.remote != live.job_id
            && live.state == crate::backend::PrinterState::Idle
            && matches!(
                prepared.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            prepared.job.state = match live.detail.as_deref() {
                Some("FINISHED") if live.job_id.is_none() => JobState::Done,
                Some("STOPPED") if live.job_id.is_none() => JobState::Cancelled,
                _ => JobState::Ended,
            };
            prepared.job.seconds_left = None;
        }
        if matches!(
            prepared.job.state,
            JobState::Ready | JobState::Printing | JobState::Paused | JobState::Ended
        ) {
            if let Some(outcome) = prepared.remote.as_deref().and_then(|id| {
                self.client(&prepared.job.printer)
                    .ok()?
                    .terminal(id, &prepared.filename)
            }) {
                prepared.job.state = outcome;
            }
        }
    }
    async fn control(&self, printer: &str, action: &str, expected: JobState) -> Result<Job, Error> {
        let _transition = self
            .transitions
            .get(printer)
            .ok_or(Error::NotFound)?
            .lock()
            .await;
        let mut guard = self.machine(printer).await?;
        let prusalink = self.client(printer).ok();
        let live_job = if let Some(client) = prusalink {
            client.snapshot().await?.printer.job_id
        } else {
            None
        };
        let mut target = self
            .jobs
            .lock()
            .await
            .values()
            .find(|p| {
                p.job.printer == printer
                    && p.attempted
                    && p.job.state == expected
                    && (prusalink.is_none() || p.remote.is_some() && p.remote == live_job)
            })
            .cloned()
            .ok_or_else(|| {
                refuse("No matching tracked job. Read status to reconcile the printer.")
            })?;
        let result = self.dispatch_control(&mut target, action, &mut guard).await;
        self.publish(&mut target).await?;
        if target.job.state == JobState::Cancelled {
            Ok(target.job)
        } else {
            result.map(|_| target.job)
        }
    }
    async fn dispatch_control(
        &self,
        target: &mut Prepared,
        action: &str,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        if let Some(client) = self.sdcp.get(&target.job.printer) {
            Err(client.refusal(action))
        } else if let Some(client) = self.snapmaker.get(&target.job.printer) {
            Err(client.refusal())
        } else if self.moonraker.contains_key(&target.job.printer) {
            self.moonraker_control(target, action, guard).await
        } else if self.duet.contains_key(&target.job.printer) {
            self.duet_control(target, action, guard).await
        } else if self.repetier.contains_key(&target.job.printer) {
            self.repetier_control(target, action).await
        } else if self.octoprint.contains_key(&target.job.printer) {
            self.octoprint_control(target, action, guard).await
        } else if self.bambu.contains_key(&target.job.printer) {
            self.bambu_control(target, action, guard).await
        } else {
            self.control_job(target, action, guard).await
        }
    }
    async fn control_job(
        &self,
        target: &mut Prepared,
        action: &str,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
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
        self.validate_control(target).await?;
        let response = client.request(method, &path, Bytes::new(), false).await;
        let (code, _) = response?;
        if code != StatusCode::NO_CONTENT {
            return Err(refuse(
                "Printer refused the job command. Read status before another action.",
            ));
        }
        if action == "cancel" {
            self.publish(target).await?;
        } else {
            self.poll_boundary(target, guard).await?;
        }
        self.wait_prusalink(target, action).await?;
        Ok(target.job.clone())
    }
}
impl Printers {
    async fn wait_prusalink(&self, target: &mut Prepared, action: &str) -> Result<(), Error> {
        use std::time::Duration;
        use tokio::time::{sleep, timeout_at, Instant};
        let deadline = Instant::now()
            + Duration::from_secs(if matches!(action, "start" | "cancel") {
                25
            } else {
                10
            });
        let expected = match action {
            "pause" => JobState::Paused,
            "cancel" => JobState::Cancelled,
            _ => JobState::Printing,
        };
        let mut pending = false;
        loop {
            if let Ok(Ok(live)) = timeout_at(deadline, self.reconcile(target)).await {
                self.publish(target).await?;
                if target.job.state == JobState::Cancelled {
                    return Ok(());
                }
                if target.job.state == expected
                    && target.remote.is_some()
                    && target.remote == live.printer.job_id
                {
                    return Ok(());
                }
                if live
                    .printer
                    .job_name
                    .as_deref()
                    .is_some_and(|name| name != target.filename)
                    || target.remote.is_some() && target.remote != live.printer.job_id
                {
                    return Err(refuse(
                        "Live job changed while the command was pending. Read fresh status.",
                    ));
                }
            }
            if !pending {
                eprintln!("PrusaLink {action} is pending. Waiting for fresh status; do not repeat the command.");
                pending = true;
            }
            if Instant::now() >= deadline {
                return Err(refuse("PrusaLink command is not confirmed after waiting. Read fresh status to reconcile the pending outcome; do not repeat the command."));
            }
            sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    async fn reconcile_octoprint(
        &self,
        target: &mut Prepared,
    ) -> Result<crate::octoprint::Snapshot, Error> {
        use crate::backend::PrinterState;
        let client = self
            .octoprint
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = client.snapshot().await?;
        let identity = format!("local/{}", target.filename);
        if target.attempted
            && live.printer.job_id.as_deref() == Some(identity.as_str())
            && live
                .job
                .pointer("/job/file/path")
                .and_then(Value::as_str)
                .is_none_or(|p| p == target.filename)
            && matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            match live.printer.state {
                PrinterState::Printing => {
                    target.job.state = JobState::Printing;
                    target.remote = Some(identity);
                }
                PrinterState::Paused => {
                    target.job.state = JobState::Paused;
                    target.remote = Some(identity);
                }
                PrinterState::Idle if target.remote.is_some() => {
                    let file = client
                        .json(&format!("/api/files/local/{}", target.filename))
                        .await?;
                    if file.get("name").and_then(Value::as_str) != Some(target.filename.as_str())
                        || file.get("origin").and_then(Value::as_str) != Some("local")
                    {
                        return Err(refuse(
                            "Stored file identity differs from the tracked job. Read fresh status.",
                        ));
                    }
                    let history = crate::octoprint::history(&file);
                    if crate::octoprint::history_date(&history).is_some_and(|date| {
                        crate::octoprint::history_date(&target.history).is_none_or(|old| date > old)
                    }) {
                        target.job.state = match history.get("success").and_then(Value::as_bool) {
                            Some(true) => JobState::Done,
                            Some(false) if target.cancelling => JobState::Cancelled,
                            Some(false) => JobState::Failed,
                            None => target.job.state,
                        };
                    }
                }
                // Error flags alone cannot attribute a terminal outcome to this execution.
                _ => {}
            }
            target.job.progress = live.printer.progress;
            target.job.seconds_left = live.printer.seconds_left;
        }
        Ok(live)
    }
    async fn octoprint_control(
        &self,
        target: &mut Prepared,
        action: &str,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        let client = self
            .octoprint
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = self.reconcile_octoprint(target).await?;
        if !live.control_ready
            || !target.attempted
            || target.remote.is_none()
            || target.remote != live.printer.job_id
            || live
                .job
                .pointer("/job/file/path")
                .and_then(Value::as_str)
                .is_some_and(|p| p != target.filename)
            || !matches!(target.job.state, JobState::Printing | JobState::Paused)
            || !matches!(
                (action, live.printer.detail.as_deref(), live.printer.state),
                (
                    "pause",
                    Some("Printing" | "Printing from SD"),
                    crate::backend::PrinterState::Printing
                ) | (
                    "resume",
                    Some("Paused"),
                    crate::backend::PrinterState::Paused
                ) | (
                    "cancel",
                    Some(
                        "Printing"
                            | "Printing from SD"
                            | "Starting"
                            | "Starting print from SD"
                            | "Resuming"
                            | "Finishing"
                            | "Pausing"
                            | "Paused"
                            | "Cancelling"
                    ),
                    crate::backend::PrinterState::Printing | crate::backend::PrinterState::Paused
                )
            )
        {
            return Err(refuse("No matching live job for this command or readiness is unknown. Read fresh status to reconcile the printer."));
        }
        let payload = if action == "cancel" {
            serde_json::json!({"command":"cancel"})
        } else {
            serde_json::json!({"command":"pause", "action": if action == "pause" {"pause"} else {"resume"}})
        };
        self.validate_control(target).await?;
        if action == "cancel" {
            self.record_cancel(target).await?;
        }
        let result = client.command("/api/job", payload).await;
        if result.is_err() {
            let _ = self.reconcile_octoprint(target).await;
            result?;
        }
        if action == "cancel" {
            self.publish(target).await?;
        } else {
            self.poll_boundary(target, guard).await?;
        }
        self.wait_octoprint(target, action).await?;
        Ok(target.job.clone())
    }
    async fn wait_octoprint(&self, target: &mut Prepared, action: &str) -> Result<(), Error> {
        use std::time::Duration;
        use tokio::time::{sleep, timeout_at, Instant};
        let budget = if matches!(action, "start" | "cancel") {
            25
        } else {
            10
        };
        let deadline = Instant::now() + Duration::from_secs(budget);
        let mut pending = false;
        loop {
            let fresh = timeout_at(deadline, self.reconcile_octoprint(target)).await;
            if let Ok(Ok(live)) = &fresh {
                self.publish(target).await?;
                if target.job.state == JobState::Cancelled {
                    return Ok(());
                }
                let matching = live.printer.job_id == target.remote
                    && live
                        .job
                        .pointer("/job/file/path")
                        .and_then(Value::as_str)
                        .is_none_or(|p| p == target.filename);
                let confirmed = matching
                    && match action {
                        "start" | "resume" => {
                            target.job.state == JobState::Printing
                                && matches!(
                                    live.printer.detail.as_deref(),
                                    Some("Printing" | "Printing from SD")
                                )
                                && live.printer.state == crate::backend::PrinterState::Printing
                        }
                        "pause" => {
                            target.job.state == JobState::Paused
                                && live.printer.state == crate::backend::PrinterState::Paused
                        }
                        _ => target.job.state == JobState::Cancelled,
                    };
                if confirmed {
                    return Ok(());
                }
                if live.printer.job_id.as_deref()
                    != Some(format!("local/{}", target.filename).as_str())
                {
                    return Err(refuse("Live file changed while the command was pending. Read fresh status to reconcile the job."));
                }
            }
            if !pending {
                eprintln!("OctoPrint {action} is pending. Waiting for fresh status; do not repeat the command.");
                pending = true;
            }
            if Instant::now() >= deadline {
                return Err(refuse("OctoPrint command is not confirmed after waiting. Read fresh status to reconcile the pending outcome; do not repeat the command."));
            }
            sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    async fn start_octoprint(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        if target.attempted || target.job.state != JobState::Ready {
            return Err(refuse("Start was already attempted. Read status to reconcile the intended file and job. Start will not be repeated."));
        }
        if self.profiles(&target.job.printer)? != target.profiles {
            return Err(refuse("Printer model, tooling, material, or plate profiles changed. Prepare and review again."));
        }
        let client = self
            .octoprint
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = client.snapshot().await?;
        crate::octoprint::ready(&live)?;
        let info = client.review(&live).await?;
        let bytes = file_bytes(&target.path).await?;
        check_nozzle(&bytes, &info)?;
        if info != target.info {
            return Err(refuse("OctoPrint connection, version, build volume, or tooling changed. Prepare and review again."));
        }
        if hash(&bytes) != target.digest {
            return Err(refuse("Prepared G-code changed. Prepare and review again."));
        }
        // Use a new remote name per preparation; refuse collisions rather than overwrite.
        let filename = target.filename.clone();
        let path = format!("/api/files/local/{filename}");
        let (code, _) = client
            .request(Method::GET, &path, Bytes::new(), "application/json", false)
            .await?;
        if code != StatusCode::NOT_FOUND {
            return Err(refuse("OctoPrint staging file already exists or storage cannot be checked. Prepare a new job after inspecting storage."));
        }
        let live = client.snapshot().await?;
        crate::octoprint::ready(&live)?;
        let info = client.review(&live).await?;
        check_nozzle(&bytes, &info)?;
        if info != target.info {
            return Err(refuse(
                "OctoPrint machine profile or connection changed. Prepare and review again.",
            ));
        }
        let upload = async {
            client.upload(&filename, &bytes).await?;
            let uploaded = client.json(&path).await?;
            if uploaded.get("name").and_then(Value::as_str) != Some(filename.as_str())
                || uploaded.get("origin").and_then(Value::as_str) != Some("local")
                || uploaded.get("type").and_then(Value::as_str) != Some("machinecode")
                || uploaded.get("size").and_then(Value::as_u64) != Some(bytes.len() as u64)
                || uploaded
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|p| p != filename)
            {
                return Err(refuse("Uploaded file validation failed. Inspect the stored name, origin, type, and size before starting."));
            }
            Ok::<_, Error>(uploaded)
        }
        .await;
        let uploaded = match upload {
            Ok(uploaded) => uploaded,
            Err(error) => {
                if client.delete(&filename).await.is_err() {
                    return Err(refuse("OctoPrint upload failed and remote cleanup is unconfirmed. Inspect storage and remove the staged file before preparing again."));
                }
                return Err(error);
            }
        };
        target.staged = Some(filename.clone());
        self.publish(target).await?;
        target.history = crate::octoprint::history(&uploaded);
        let live = client.snapshot().await?;
        crate::octoprint::ready(&live)?;
        if client.review(&live).await? != target.info {
            return Err(refuse("OctoPrint machine profile or connection changed during upload. Prepare and review again."));
        }
        // Selection does not move the machine. Reconcile it before crossing the start boundary.
        client
            .command(
                &path,
                serde_json::json!({"command":"select", "print":false}),
            )
            .await?;
        let selected = client.snapshot().await?;
        crate::octoprint::ready(&selected)?;
        if selected.printer.job_id.as_deref() != Some(format!("local/{filename}").as_str())
            || selected
                .job
                .pointer("/job/file/path")
                .and_then(Value::as_str)
                .is_some_and(|p| p != filename)
            || selected
                .job
                .pointer("/job/file/size")
                .and_then(Value::as_u64)
                != Some(bytes.len() as u64)
        {
            return Err(refuse("Selected file differs from the reviewed upload. Read status and review selection before starting."));
        }
        self.attempted(target).await?;
        let reply = client
            .command("/api/job", serde_json::json!({"command":"start"}))
            .await;
        if reply.is_err() {
            let _ = self.reconcile_octoprint(target).await;
        }
        match reply {
            Err(Error::Unavailable) => return Err(refuse("Start reply was lost. Read status to reconcile the intended file and job. Start will not be repeated.")),
            other => other?,
        }
        self.poll_boundary(target, guard).await?;
        self.wait_octoprint(target, "start").await?;
        Ok(target.job.clone())
    }
}
impl Printers {
    async fn reconcile_moonraker(
        &self,
        target: &mut Prepared,
    ) -> Result<crate::moonraker::Snapshot, Error> {
        let client = self
            .moonraker
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = client.snapshot().await?;
        let id = live.printer.job_id.as_deref();
        let baseline = target.history.get("job_id").and_then(Value::as_str);
        if target.attempted
            && id.is_some()
            && id != baseline
            && live.history.get("exists").and_then(Value::as_bool) == Some(true)
            && (target.remote.is_none() || target.remote.as_deref() == id)
            && live.printer.job_name.as_deref() == Some(target.filename.as_str())
            && live
                .history
                .pointer("/metadata/size")
                .and_then(Value::as_u64)
                == target.history.get("size").and_then(Value::as_u64)
            && matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            let outcome = match (
                live.history.get("status").and_then(Value::as_str),
                live.printer.state,
                live.printer.detail.as_deref(),
            ) {
                (Some("in_progress"), crate::backend::PrinterState::Printing, Some("printing")) => {
                    Some(JobState::Printing)
                }
                (Some("in_progress"), crate::backend::PrinterState::Paused, Some("paused")) => {
                    Some(JobState::Paused)
                }
                (Some("completed"), crate::backend::PrinterState::Idle, Some("complete")) => {
                    Some(JobState::Done)
                }
                (Some("cancelled"), crate::backend::PrinterState::Idle, Some("cancelled")) => {
                    Some(JobState::Cancelled)
                }
                (Some("error" | "klippy_shutdown" | "klippy_disconnect" | "interrupted"), _, _) => {
                    Some(JobState::Failed)
                }
                _ => None,
            };
            if let Some(outcome) = outcome {
                target.remote = live.printer.job_id.clone();
                target.job.state = outcome;
                target.job.progress = live.printer.progress;
                target.job.seconds_left = live.printer.seconds_left;
            }
        }
        Ok(live)
    }
    async fn moonraker_control(
        &self,
        target: &mut Prepared,
        action: &str,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        let client = self
            .moonraker
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = self.reconcile_moonraker(target).await?;
        if !target.attempted
            || !matches!(target.job.state, JobState::Printing | JobState::Paused)
            || target.remote.is_none()
            || target.remote != live.printer.job_id
            || live.printer.job_name.as_deref() != Some(target.filename.as_str())
            || live.history.get("exists").and_then(Value::as_bool) != Some(true)
            || live
                .history
                .pointer("/metadata/size")
                .and_then(Value::as_u64)
                != target.history.get("size").and_then(Value::as_u64)
            || live.history.get("status").and_then(Value::as_str) != Some("in_progress")
            || !matches!(
                (action, live.printer.state, live.printer.detail.as_deref()),
                (
                    "pause",
                    crate::backend::PrinterState::Printing,
                    Some("printing")
                ) | (
                    "resume",
                    crate::backend::PrinterState::Paused,
                    Some("paused")
                ) | (
                    "cancel",
                    crate::backend::PrinterState::Printing | crate::backend::PrinterState::Paused,
                    Some("printing" | "paused")
                )
            )
        {
            return Err(refuse("No matching live job or readiness is unknown. Read fresh status to reconcile the printer."));
        }
        if action == "resume" {
            if !client.resume_commissioned() {
                return Err(refuse("Resume behaviour has not been commissioned. Review the printer macros and set resume_commissioned before resuming."));
            }
            if client.review(&live).await? != target.info {
                return Err(refuse("Machine configuration or resume macros changed. Review the printer before resuming."));
            }
        }
        if live.objects.get("pause_resume").is_none() {
            return Err(refuse("Printer control capability is missing. Review the installed pause and cancel macros."));
        }
        self.validate_control(target).await?;
        if let Err(error) = client.command(&format!("/printer/print/{action}")).await {
            let _ = self.reconcile_moonraker(target).await;
            return Err(error);
        }
        if action == "cancel" {
            self.publish(target).await?;
        } else {
            self.poll_boundary(target, guard).await?;
        }
        self.wait_moonraker(target, action).await?;
        Ok(target.job.clone())
    }
    async fn wait_moonraker(&self, target: &mut Prepared, action: &str) -> Result<(), Error> {
        use std::time::Duration;
        use tokio::time::{sleep, timeout_at, Instant};
        let deadline = Instant::now()
            + Duration::from_secs(if matches!(action, "start" | "cancel") {
                25
            } else {
                10
            });
        let expected = match action {
            "pause" => JobState::Paused,
            "cancel" => JobState::Cancelled,
            _ => JobState::Printing,
        };
        let mut pending = false;
        loop {
            if let Ok(Ok(live)) = timeout_at(deadline, self.reconcile_moonraker(target)).await {
                self.publish(target).await?;
                if target.job.state == JobState::Cancelled {
                    return Ok(());
                }
                if target.job.state == expected
                    && target.remote.is_some()
                    && target.remote == live.printer.job_id
                {
                    return Ok(());
                }
                if live
                    .printer
                    .job_name
                    .as_deref()
                    .is_some_and(|name| name != target.filename)
                    || target.remote.is_some() && target.remote != live.printer.job_id
                {
                    return Err(refuse(
                        "Live job changed while the command was pending. Read fresh status.",
                    ));
                }
            }
            if !pending {
                eprintln!("Moonraker {action} is pending. Waiting for fresh status; do not repeat the command.");
                pending = true;
            }
            if Instant::now() >= deadline {
                return Err(refuse("Moonraker command is not confirmed after waiting. Read fresh status to reconcile the pending outcome; do not repeat the command."));
            }
            sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    async fn start_moonraker(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        if target.attempted || target.job.state != JobState::Ready {
            return Err(refuse("Start was already attempted. Read status to reconcile the intended file and job. Start will not be repeated."));
        }
        if self.profiles(&target.job.printer)? != target.profiles {
            return Err(refuse("Printer model, tooling, material, or plate profiles changed. Prepare and review again."));
        }
        let client = self
            .moonraker
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = client.snapshot().await?;
        crate::moonraker::ready(&live)?;
        let bytes = file_bytes(&target.path).await?;
        let info = client.review(&live).await?;
        check_nozzle(&bytes, &info)?;
        if info != target.info {
            return Err(refuse("Machine identity, firmware, build volume, or tooling changed. Prepare and review again."));
        }
        if hash(&bytes) != target.digest {
            return Err(refuse("Prepared G-code changed. Prepare and review again."));
        }
        let filename = target.filename.clone();
        let (code, _) = client
            .request(
                Method::GET,
                &format!("/server/files/metadata?filename={filename}"),
                Bytes::new(),
                "application/json",
                false,
            )
            .await?;
        if code != StatusCode::NOT_FOUND {
            return Err(refuse("Staging file already exists or storage cannot be checked. Prepare a new job after inspecting storage."));
        }
        let fresh = client.snapshot().await?;
        crate::moonraker::ready(&fresh)?;
        check_nozzle(&bytes, &client.review(&fresh).await?)?;
        let staged = async {
            client.upload(&filename, &bytes).await?;
            let metadata = client.metadata(&filename).await?;
            if metadata.get("filename").and_then(Value::as_str) != Some(filename.as_str())
                || metadata.get("size").and_then(Value::as_u64) != Some(bytes.len() as u64)
            {
                return Err(refuse(
                    "Uploaded file metadata differs from the reviewed name or size. Prepare again.",
                ));
            }
            Ok(metadata)
        }
        .await;
        let metadata = match staged {
            Ok(metadata) => metadata,
            Err(error) => {
                if client.delete(&filename).await.is_err() {
                    return Err(refuse("Upload failed and remote cleanup is not confirmed. Inspect Moonraker storage and remove the staged file before preparing again."));
                }
                return Err(error);
            }
        };
        target.staged = Some(filename.clone());
        self.publish(target).await?;
        target.history =
            serde_json::json!({"job_id":metadata.get("job_id"),"size":metadata.get("size")});
        let live = client.snapshot().await?;
        crate::moonraker::ready(&live)?;
        if client.review(&live).await? != target.info {
            return Err(refuse(
                "Machine configuration changed during upload. Prepare and review again.",
            ));
        }
        self.attempted(target).await?;
        let reply = client
            .command(&format!("/printer/print/start?filename={filename}"))
            .await;
        if let Err(error) = reply {
            let _ = self.reconcile_moonraker(target).await;
            return Err(if matches!(error, Error::Unavailable) {
                refuse("Start reply was lost. Read status to reconcile the intended file and job. Start will not be repeated.")
            } else {
                error
            });
        }
        self.poll_boundary(target, guard).await?;
        self.wait_moonraker(target, "start").await?;
        Ok(target.job.clone())
    }
}
impl Printers {
    async fn start_job(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        if let Some(client) = self.sdcp.get(&target.job.printer) {
            return Err(client.refusal("start"));
        }
        if let Some(client) = self.snapmaker.get(&target.job.printer) {
            return Err(client.refusal());
        }
        if self.moonraker.contains_key(&target.job.printer) {
            return self.start_moonraker(target, guard).await;
        }
        if self.duet.contains_key(&target.job.printer) {
            return self.start_duet(target, guard).await;
        }
        if self.repetier.contains_key(&target.job.printer) {
            return self.start_repetier(target, guard).await;
        }
        if self.octoprint.contains_key(&target.job.printer) {
            return self.start_octoprint(target, guard).await;
        }
        if let Some(client) = self.bambu.get(&target.job.printer) {
            client.check_control()?;
            if target.attempted || target.job.state != JobState::Ready {
                return Err(refuse("Start was already attempted. Read status to reconcile it; Start will not be repeated."));
            }
            if self.profiles(&target.job.printer)? != target.profiles {
                return Err(refuse(
                    "Printer profiles changed. Prepare and review again.",
                ));
            }
            client.ready().await?;
            let bytes = file_bytes(&target.path).await?;
            if hash(&bytes) != target.digest {
                return Err(refuse("Prepared 3MF changed. Prepare and review again."));
            }
            let profile = self
                .configs
                .iter()
                .find(|p| p.id == target.job.printer)
                .ok_or(Error::NotFound)?;
            let name = target.filename.trim_end_matches(".gcode.3mf").to_owned();
            let payload = crate::bambu::start_payload(profile, &target.filename, &name, &bytes)?;
            if target.info != payload {
                return Err(refuse(
                    "Start configuration changed. Prepare and review again.",
                ));
            }
            client.upload(&target.filename, &bytes).await?;
            target.staged = Some(target.filename.clone());
            self.publish(target).await?;
            client.ready().await?;
            self.attempted(target).await?;
            self.publish(target).await?;
            let result = client.command(payload, &name, "RUNNING", None, guard).await;
            self.publish(target).await?;
            self.reconcile_bambu(target).await?;
            result?;
            if target.job.state != JobState::Printing {
                return Err(refuse(
                    "Start is not confirmed. Read status to reconcile the job.",
                ));
            }
            return Ok(target.job.clone());
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
            return Err(refuse(
                "Machine identity, firmware, or tooling changed. Prepare and review the job again.",
            ));
        }
        let storage = client.json("/api/v1/storage").await?;
        let entry = storage
            .get("storage_list")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter().find(|s| {
                    s.get("available").and_then(Value::as_bool) == Some(true)
                        && s.get("read_only").and_then(Value::as_bool) == Some(false)
                        && s.get("free_space")
                            .and_then(Value::as_u64)
                            .is_some_and(|free| free >= bytes.len() as u64)
                })
            })
            .ok_or_else(|| {
                refuse(
                    "No writable storage with enough reported free space. Check printer storage.",
                )
            })?;
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
        let staged = async {
            let (code, _) = client
                .request(Method::PUT, &path, bytes.clone(), true)
                .await?;
            if code != StatusCode::CREATED {
                return Err(refuse(
                    "Printer refused the upload. Check storage before preparing again.",
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
                    "Uploaded file validation failed. Prepare and review again.",
                ));
            }
            Ok(())
        }
        .await;
        if let Err(error) = staged {
            let cleanup = client
                .request(Method::DELETE, &path, Bytes::new(), false)
                .await;
            if !cleanup.is_ok_and(|(code, _)| {
                matches!(code, StatusCode::NO_CONTENT | StatusCode::NOT_FOUND)
            }) {
                return Err(refuse("Upload failed and cleanup is not confirmed. Inspect printer storage and remove the staged file before preparing again. No print was started."));
            }
            return Err(error);
        }
        target.staged = Some(path.clone());
        self.publish(target).await?;
        ready(&client.snapshot().await?)?;
        if self.review_info(client).await? != target.info {
            return Err(refuse("Machine identity, firmware, or tooling changed during upload. Prepare and review again."));
        }
        ready(&client.snapshot().await?)?;
        self.attempted(target).await?;
        let reply = client
            .request(Method::POST, &path, Bytes::new(), false)
            .await;
        self.poll_boundary(target, guard).await?;
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
        let _ = fresh;
        self.poll_boundary(target, guard).await?;
        self.wait_prusalink(target, "start").await?;
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
            if action == "stop" {
                if let Some(client) = self.repetier.get(printer) {
                    return self.review_repetier_stop(client, printer).await;
                }
            }
            if matches!(action, "clear" | "stop") {
                return Ok(());
            }
            self.check_control(printer)?;
            if let Some(client) = self.sdcp.get(printer) {
                return Err(client.refusal(action));
            }
            if let Some(client) = self.snapmaker.get(printer) {
                return Err(client.refusal());
            }
            if let Some(client) = self.bambu.get(printer) {
                let live = client.snapshot().await;
                let jobs = self.jobs.lock().await;
                let target = job
                    .and_then(|id| jobs.get(id))
                    .or_else(|| {
                        jobs.values().find(|p| {
                            p.job.printer == printer
                                && p.remote.is_some()
                                && p.remote == live.job_id
                        })
                    })
                    .ok_or_else(|| {
                        refuse("No matching tracked job. Read status to reconcile the printer.")
                    })?
                    .clone();
                drop(jobs);
                if action == "start" {
                    client.ready().await?;
                    if target.attempted || target.job.state != JobState::Ready {
                        return Err(refuse(
                            "Start was already attempted. Read status; do not repeat Start.",
                        ));
                    }
                    let bytes = file_bytes(&target.path).await?;
                    if hash(&bytes) != target.digest || self.profiles(printer)? != target.profiles {
                        return Err(refuse(
                            "Prepared file or printer changed. Prepare and review again.",
                        ));
                    }
                    client.check_materials(&bytes).await?;
                } else if target.attempted
                    && (target.remote.is_none() || target.remote != live.job_id)
                {
                    return Err(refuse(
                        "The live job differs from the reviewed job. No command was sent.",
                    ));
                }
                return Ok(());
            }
            if let Some(client) = self.moonraker.get(printer) {
                let live = client.snapshot().await?;
                let jobs = self.jobs.lock().await;
                let target = job
                    .and_then(|id| jobs.get(id))
                    .or_else(|| {
                        jobs.values().find(|p| {
                            p.job.printer == printer
                                && p.remote.is_some()
                                && p.remote == live.printer.job_id
                        })
                    })
                    .ok_or_else(|| {
                        refuse("No matching tracked job. Read status to reconcile the printer.")
                    })?
                    .clone();
                drop(jobs);
                if action == "start" {
                    crate::moonraker::ready(&live)?;
                    if target.attempted || target.job.state != JobState::Ready {
                        return Err(refuse(
                            "Start was already attempted. Read status; do not repeat Start.",
                        ));
                    }
                    let bytes = file_bytes(&target.path).await?;
                    let info = client.review(&live).await?;
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
                    if target.remote.is_none()
                        || target.remote != live.printer.job_id
                        || live.printer.job_name.as_deref() != Some(target.filename.as_str())
                        || !matches!(live.printer.detail.as_deref(), Some("printing" | "paused"))
                    {
                        return Err(refuse("The live job differs from the reviewed job or readiness is unknown. Read fresh status."));
                    }
                    if action == "resume"
                        && (!client.resume_commissioned()
                            || client.review(&live).await? != target.info)
                    {
                        return Err(refuse("Review the printer configuration and commission resume before resuming."));
                    }
                }
                return Ok(());
            }
            if let Some(client) = self.duet.get(printer) {
                let live = client.snapshot().await?;
                let jobs = self.jobs.lock().await;
                let target = job
                    .and_then(|id| jobs.get(id))
                    .or_else(|| {
                        jobs.values().find(|p| {
                            p.job.printer == printer && p.remote.is_some() && p.remote == live.file
                        })
                    })
                    .ok_or_else(|| {
                        refuse("No matching tracked job. Read status to reconcile the printer.")
                    })?
                    .clone();
                drop(jobs);
                if action == "start" {
                    crate::duet::ready(&live)?;
                    if target.attempted || target.job.state != JobState::Ready {
                        return Err(refuse(
                            "Start was already attempted. Read status; do not repeat Start.",
                        ));
                    }
                    let bytes = file_bytes(&target.path).await?;
                    let info = client.review().await?;
                    check_nozzle(&bytes, &info)?;
                    if hash(&bytes) != target.digest
                        || info != target.info
                        || self.profiles(printer)? != target.profiles
                    {
                        return Err(refuse(
                            "Prepared file or printer changed. Prepare and review again.",
                        ));
                    }
                } else if target.attempted
                    && (target.remote.is_none()
                        || target.remote != live.file
                        || !matches!(
                            live.status.as_deref(),
                            Some("processing" | "paused" | "pausing")
                        ))
                {
                    return Err(refuse("The live job differs from the reviewed job or readiness is unknown. Read fresh status."));
                }
                return Ok(());
            }
            if let Some(client) = self.repetier.get(printer) {
                return self.review_repetier(client, action, printer, job).await;
            }
            if let Some(client) = self.octoprint.get(printer) {
                let live = client.snapshot().await?;
                let jobs = self.jobs.lock().await;
                let target = job
                    .and_then(|id| jobs.get(id))
                    .or_else(|| {
                        jobs.values().find(|p| {
                            p.job.printer == printer
                                && p.remote.is_some()
                                && p.remote == live.printer.job_id
                        })
                    })
                    .ok_or_else(|| {
                        refuse("No matching tracked job. Read status to reconcile the printer.")
                    })?
                    .clone();
                drop(jobs);
                if action == "start" {
                    crate::octoprint::ready(&live)?;
                    if target.attempted || target.job.state != JobState::Ready {
                        return Err(refuse(
                            "Start was already attempted. Read status; do not repeat Start.",
                        ));
                    }
                    let bytes = file_bytes(&target.path).await?;
                    let info = client.review(&live).await?;
                    check_nozzle(&bytes, &info)?;
                    if hash(&bytes) != target.digest
                        || info != target.info
                        || self.profiles(printer)? != target.profiles
                    {
                        return Err(refuse(
                            "Prepared file or printer changed. Prepare and review again.",
                        ));
                    }
                } else if target.attempted
                    && (!live.control_ready
                        || target.remote.is_none()
                        || target.remote != live.printer.job_id
                        || live
                            .job
                            .pointer("/job/file/path")
                            .and_then(Value::as_str)
                            .is_some_and(|p| p != target.filename))
                {
                    return Err(refuse("The live job differs from the reviewed job or readiness is unknown. Read fresh status."));
                }
                return Ok(());
            }
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
            })?
            .clone();
            drop(jobs);
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
            if let Some(client) = self.sdcp.get(id) {
                return Ok(client.snapshot().await);
            }
            if let Some(client) = self.snapmaker.get(id) {
                return Ok(client.snapshot().await);
            }
            if let Some(client) = self.moonraker.get(id) {
                return Ok(match client.snapshot().await {
                    Ok(snapshot) => snapshot.printer,
                    Err(error) => client.offline(&error),
                });
            }
            if let Some(client) = self.duet.get(id) {
                return Ok(match client.snapshot().await {
                    Ok(snapshot) => snapshot.printer,
                    Err(error) => client.offline(&error),
                });
            }
            if let Some(client) = self.repetier.get(id) {
                return Ok(match client.snapshot().await {
                    Ok(snapshot) => snapshot.printer,
                    Err(error) => client.offline(&error),
                });
            }
            if let Some(client) = self.octoprint.get(id) {
                return Ok(match client.snapshot().await {
                    Ok(snapshot) => snapshot.printer,
                    Err(error) => client.offline(&error),
                });
            }
            let client = self.client(id)?;
            let targets: Vec<_> = self
                .jobs
                .lock()
                .await
                .values()
                .filter(|p| p.job.printer == id)
                .cloned()
                .collect();
            let live = match client.snapshot().await {
                Ok(snapshot) => snapshot.printer,
                Err(e) => client.offline(&e),
            };
            for mut target in targets {
                self.observe_prusalink(&mut target, &live);
                // A forgotten job does not make a printer status read fail.
                if let Err(error) = self.publish(&mut target).await {
                    if !matches!(error, Error::NotFound) {
                        return Err(error);
                    }
                }
            }
            Ok(live)
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
        if let Some(client) = self.bambu.get(printer) {
            client.check_control()
        } else if let Some(client) = self.sdcp.get(printer) {
            client.check_control()
        } else if let Some(client) = self.snapmaker.get(printer) {
            client.check_control()
        } else {
            Ok(())
        }
    }

    fn latches_terminal_reports(&self, printer: &str) -> bool {
        self.bambu.contains_key(printer)
            || self.octoprint.contains_key(printer)
            || self.moonraker.contains_key(printer)
            || self.duet.contains_key(printer)
            || self.repetier.contains_key(printer)
            || self.sdcp.contains_key(printer)
    }
    fn clear<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            if let Some(client) = self.bambu.get(id) {
                client.acknowledge();
            }
            if let Some(client) = self.moonraker.get(id) {
                client.acknowledge();
            }
            if let Some(client) = self.octoprint.get(id) {
                client.acknowledge();
            }
            if let Some(client) = self.duet.get(id) {
                client.acknowledge();
            }
            if let Some(client) = self.repetier.get(id) {
                client.acknowledge();
            }
            if let Some(client) = self.sdcp.get(id) {
                client.acknowledge();
            }
            Ok(())
        })
    }
    fn printers(&self) -> Reply<'_, Vec<Printer>> {
        Box::pin(async {
            let targets: Vec<_> = self.jobs.lock().await.values().cloned().collect();
            let mut tasks = tokio::task::JoinSet::new();
            for (index, config) in self.configs.iter().enumerate() {
                if let Some(client) = self.moonraker.get(&config.id).cloned() {
                    tasks.spawn(async move {
                        (
                            index,
                            match client.snapshot().await {
                                Ok(s) => s.printer,
                                Err(e) => client.offline(&e),
                            },
                        )
                    });
                } else if let Some(client) = self.duet.get(&config.id).cloned() {
                    tasks.spawn(async move {
                        (
                            index,
                            match client.snapshot().await {
                                Ok(s) => s.printer,
                                Err(e) => client.offline(&e),
                            },
                        )
                    });
                } else if let Some(client) = self.repetier.get(&config.id).cloned() {
                    tasks.spawn(async move {
                        (
                            index,
                            match client.snapshot().await {
                                Ok(s) => s.printer,
                                Err(e) => client.offline(&e),
                            },
                        )
                    });
                } else if let Some(client) = self.bambu.get(&config.id).cloned() {
                    tasks.spawn(async move { (index, client.snapshot().await) });
                } else if let Some(client) = self.sdcp.get(&config.id).cloned() {
                    tasks.spawn(async move { (index, client.snapshot().await) });
                } else if let Some(client) = self.snapmaker.get(&config.id).cloned() {
                    tasks.spawn(async move { (index, client.snapshot().await) });
                } else if let Some(client) = self.octoprint.get(&config.id).cloned() {
                    tasks.spawn(async move {
                        (
                            index,
                            match client.snapshot().await {
                                Ok(s) => s.printer,
                                Err(e) => client.offline(&e),
                            },
                        )
                    });
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
            for mut target in targets {
                if self.client(&target.job.printer).is_ok() {
                    if let Some(printer) = printers.values().find(|p| p.id == target.job.printer) {
                        self.observe_prusalink(&mut target, printer);
                        if let Err(error) = self.publish(&mut target).await {
                            if !matches!(error, Error::NotFound) {
                                return Err(error);
                            }
                        }
                    }
                }
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
            if let Some(client) = self.sdcp.get(printer) {
                return Err(client.refusal("prepare"));
            }
            if let Some(client) = self.snapmaker.get(printer) {
                return Err(client.refusal());
            }
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
                let filename = format!("dutchman-{}.gcode.3mf", hash(id.as_bytes()));
                // Monitor-only preparation remains available; executable review is required for control.
                let info = if profile.lan_control {
                    crate::bambu::start_payload(
                        profile,
                        &filename,
                        filename.trim_end_matches(".gcode.3mf"),
                        &bytes,
                    )?
                } else {
                    Value::Null
                };
                let mut jobs = self.jobs.lock().await;
                if jobs.len() >= 5000 || jobs.contains_key(id) {
                    return Err(refuse("Job storage is full or this id is already in use."));
                }
                jobs.insert(
                    id.into(),
                    Prepared {
                        job: Job {
                            detail: None,
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
                        info,
                        remote: None,
                        attempted: false,
                        filename,
                        profiles: self.profiles(printer)?,
                        history: Value::Null,
                        staged: None,
                        cancelling: false,
                        revision: 0,
                        incarnation: Arc::new(()),
                    },
                );
                return Ok(output.estimate);
            }
            let info = if let Some(client) = self.moonraker.get(printer) {
                let snapshot = client.snapshot().await?;
                crate::moonraker::ready(&snapshot)?;
                client.review(&snapshot).await?
            } else if let Some(client) = self.octoprint.get(printer) {
                let snapshot = client.snapshot().await?;
                crate::octoprint::ready(&snapshot)?;
                client.review(&snapshot).await?
            } else if let Some(client) = self.duet.get(printer) {
                crate::duet::ready(&client.snapshot().await?)?;
                client.review().await?
            } else if let Some(client) = self.repetier.get(printer) {
                crate::repetier::ready(&client.snapshot().await?)?;
                client.review().await?
            } else {
                let client = self.client(printer)?;
                client.snapshot().await?;
                let info = self.review_info(client).await?;
                if info.pointer("/info/mmu").and_then(Value::as_bool) != Some(false) {
                    return Err(refuse("Material slot assignments are not supported yet. Use a commissioned single-material printer."));
                }
                info
            };
            let slicer = crate::slicer::Slicer::new(
                self.slicers.get(printer).ok_or(Error::NotFound)?.clone(),
            )
            .map_err(|_| {
                Error::Failed("Cannot find the slicer. Set slicer.path to its executable.".into())
            })?;
            let output = slicer
                .prepare(path, profile, &options, crate::slicer::OutputKind::Gcode)
                .await
                .map_err(|_| {
                    Error::Failed(
                        "Cannot slice this job. Check the configured slicer profiles.".into(),
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
                detail: None,
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
                    history: Value::Null,
                    staged: None,
                    cancelling: false,
                    revision: 0,
                    incarnation: Arc::new(()),
                },
            );
            self.checkpoint(&jobs).await?;
            Ok(output.estimate)
        })
    }
    fn start<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let printer = self.tracked(id).await?.job.printer;
            let _transition = self
                .transitions
                .get(&printer)
                .ok_or(Error::NotFound)?
                .lock()
                .await;
            let mut guard = self.machine(&printer).await?;
            let mut target = self.tracked(id).await?;
            let result = self.start_job(&mut target, &mut guard).await;
            self.publish(&mut target).await?;
            if guard.is_none() {
                guard = self.machine(&printer).await?;
                target = self.tracked(id).await?;
            }
            self.cleanup_staged(&mut target).await;
            self.publish(&mut target).await?;
            drop(guard);
            if target.job.state == JobState::Cancelled {
                Ok(target.job)
            } else {
                result.map(|_| target.job)
            }
        })
    }
    fn cancel<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let printer = self.tracked(id).await?.job.printer;
            let mut guard = self.machine(&printer).await?;
            let mut target = self.tracked(id).await?;
            self.check_control(&printer)?;
            if target.job.state == JobState::Cancelled {
                return Ok(target.job);
            }
            let result = if !target.attempted && target.job.state == JobState::Ready {
                target.job.state = JobState::Cancelled;
                Ok(target.job.clone())
            } else {
                self.dispatch_control(&mut target, "cancel", &mut guard)
                    .await
            };
            self.publish(&mut target).await?;
            if guard.is_none() {
                guard = self.machine(&printer).await?;
                target = self.tracked(id).await?;
            }
            self.cleanup_staged(&mut target).await;
            self.publish(&mut target).await?;
            drop(guard);
            if target.job.state == JobState::Cancelled {
                Ok(target.job)
            } else {
                result.map(|_| target.job)
            }
        })
    }
    fn stop<'a>(
        &'a self,
        printer: &'a str,
        identity: &'a str,
        name: &'a str,
        path: Option<&'a str>,
    ) -> Reply<'a, Printer> {
        Box::pin(async move { self.stop_live(printer, identity, name, path).await })
    }
    fn pause<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            if let Some(client) = self.sdcp.get(printer) {
                return Err(client.refusal("pause"));
            }
            if let Some(client) = self.snapmaker.get(printer) {
                return Err(client.refusal());
            }
            if self.repetier.contains_key(printer) {
                return Err(refuse(crate::repetier::PAUSE));
            }
            self.control(printer, "pause", JobState::Printing).await
        })
    }
    fn resume<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            if let Some(client) = self.sdcp.get(printer) {
                return Err(client.refusal("resume"));
            }
            if let Some(client) = self.snapmaker.get(printer) {
                return Err(client.refusal());
            }
            if self.repetier.contains_key(printer) {
                return Err(refuse(crate::repetier::RESUME));
            }
            self.control(printer, "resume", JobState::Paused).await
        })
    }
    fn job<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut target = self.tracked(id).await?;
            if target.attempted || target.staged.is_some() && target.job.state != JobState::Ready {
                self.reconcile_job(&mut target).await?;
            }
            Ok(target.job)
        })
    }
    fn forget<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            let printer = self
                .jobs
                .lock()
                .await
                .get(id)
                .map(|p| p.job.printer.clone());
            let _guard = if let Some(printer) = printer {
                self.machine(&printer).await?
            } else {
                None
            };
            if let Ok(mut target) = self.tracked(id).await {
                if target.job.state == JobState::Ready && !target.attempted {
                    target.job.state = JobState::Cancelled;
                }
                self.cleanup_staged(&mut target).await;
            }
            let mut jobs = self.jobs.lock().await;
            jobs.remove(id);
            self.checkpoint(&jobs).await?;
            Ok(())
        })
    }
}

// PrusaSlicer's text output includes the resolved printer setting in its comments.
fn check_nozzle(bytes: &[u8], info: &Value) -> Result<(), Error> {
    if let Some(tools) = info.pointer("/info/tools").and_then(Value::as_array) {
        return crate::moonraker::check_tools(bytes, tools);
    }
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
        .or_else(|| info.pointer("/profile/extruder/nozzleDiameter"))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| refuse("Printer nozzle diameter is unknown. Check the selected printer profile before preparing again."))?;
    if values.iter().any(|v| (*v - sliced).abs() > 0.0001) || (installed - sliced).abs() > 0.0001 {
        return Err(Error::Refused(format!("Installed nozzle is {installed} mm; sliced profile requires {sliced} mm. Fit the matching nozzle or slice with the matching profile.")));
    }
    Ok(())
}

#[cfg(test)]
mod locking_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    async fn fixture(path: PathBuf) -> Printers {
        let config = crate::config::Config {
            backend: crate::config::Backend {
                kind: "printers".into(),
                slice: false,
            },
            slicer: None,
            slicers: BTreeMap::new(),
            printers: Vec::new(),
        };
        let backend = Printers::new(config).unwrap().with_store(path);
        let target: Prepared = serde_json::from_value(serde_json::json!({
            "job":{"id":"one","printer":"test","name":"Synthetic","state":"ready","estimate":{},"progress":null,"seconds_left":null},
            "path":"synthetic.gcode","digest":"synthetic","info":null,"remote":null,
            "attempted":false,"filename":"synthetic.gcode","profiles":null
        })).unwrap();
        backend.jobs.lock().await.insert("one".into(), target);
        backend
    }

    #[tokio::test]
    async fn concurrent_attempts_checkpoint_exactly_one_start_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let backend = fixture(path.clone()).await;
        let mut first = backend.tracked("one").await.unwrap();
        let mut second = first.clone();
        let (a, b) = tokio::join!(
            backend.attempted(&mut first),
            backend.attempted(&mut second)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let saved: Value =
            serde_json::from_slice(&crate::safety::read_state(&path).await.unwrap()).unwrap();
        assert_eq!(saved.pointer("/one/attempted"), Some(&Value::Bool(true)));
        assert!(backend.attempted(&mut second).await.is_err());
    }

    #[tokio::test]
    async fn failed_checkpoint_never_authorizes_start_or_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let backend = fixture(dir.path().join("missing/jobs.json")).await;
        let mut target = backend.tracked("one").await.unwrap();
        assert!(matches!(
            backend.attempted(&mut target).await,
            Err(Error::Unavailable)
        ));
        assert!(backend.attempted(&mut target).await.is_err());
    }

    #[tokio::test]
    async fn stale_results_cannot_overwrite_cancelled_forgotten_or_replaced_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let backend = fixture(path.clone()).await;
        let mut late = backend.tracked("one").await.unwrap();
        let mut current = late.clone();
        current.job.state = JobState::Cancelled;
        backend.publish(&mut current).await.unwrap();
        late.job.state = JobState::Printing;
        backend.publish(&mut late).await.unwrap();
        assert_eq!(late.job.state, JobState::Cancelled);
        backend.jobs.lock().await.remove("one");
        assert!(matches!(
            backend.publish(&mut late).await,
            Err(Error::NotFound)
        ));
        let replacement = fixture(path).await.tracked("one").await.unwrap();
        backend.jobs.lock().await.insert("one".into(), replacement);
        assert!(matches!(
            backend.publish(&mut late).await,
            Err(Error::NotFound)
        ));
        assert_eq!(
            backend.tracked("one").await.unwrap().job.state,
            JobState::Ready
        );
    }
}
