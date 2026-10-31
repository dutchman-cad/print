//! Shared readiness, review, and bed clearance policy for every backend.

use crate::backend::{
    Backend, Error, Estimate, Job, JobState, Options, Printer, PrinterState, Reply,
};
use axum::body::Bytes;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt as _;
use tokio::sync::Mutex;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    #[serde(skip)]
    cleared: HashMap<String, bool>,
    #[serde(skip)]
    clearance: HashMap<String, (std::time::Duration, std::time::Duration)>,
    reviews: HashMap<String, Review>,
    #[serde(default)]
    job_names: HashMap<String, Option<String>>,
    #[serde(default)]
    terminal_seen: HashMap<String, bool>,
    #[serde(default)]
    observed: HashMap<String, (Option<String>, Option<String>)>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Review {
    printer: String,
    path: std::path::PathBuf,
    hash: String,
}

enum HistoryTask {
    Job(Job, Option<&'static str>),
    Clear(crate::history::Record),
    Stop(Vec<String>),
    Poll(bool, Vec<crate::history::Record>),
    Started(String, String),
}

#[derive(Clone)]
pub struct Safety {
    inner: Arc<dyn Backend>,
    state: Arc<Mutex<State>>,
    stops: Arc<Mutex<HashMap<String, bool>>>,
    // Only Start takes these locks. Stop, cancel, pause, resume, clear, status,
    // and history must never wait on a pending Start.
    starts: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    history_pending: Arc<crate::history_pending::Pending>,
    history_poll: Arc<Mutex<()>>,
    history_tasks: Arc<tokio::sync::Semaphore>,
    store: Option<std::path::PathBuf>,
    history_root: Option<std::path::PathBuf>,
    clock: Arc<dyn crate::simulated::Clock>,
    interval: std::time::Duration,
    expiry: std::time::Duration,
}
pub async fn file_bytes(path: &Path) -> Result<Bytes, Error> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await
        .map_err(|_| {
            Error::Refused("Prepared file is missing. Prepare and review it again.".into())
        })?
        .take((200 << 20) + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Error::Unavailable)?;
    if bytes.len() > 200 << 20 {
        return Err(Error::Refused(
            "Prepared file exceeds 200 MiB. Prepare a smaller job.".into(),
        ));
    }
    Ok(bytes.into())
}
pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) async fn read_state(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take((16 << 20) + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > 16 << 20 {
        return Err(std::io::Error::other("State exceeds 16 MiB"));
    }
    Ok(bytes)
}
/// Replace private state only after its bytes have reached the filesystem.
pub(crate) async fn write_state(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let temporary = path.with_extension("new");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, path).await?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        tokio::fs::File::open(parent).await?.sync_all().await?;
    }
    Ok(())
}
impl Safety {
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(State::default())),
            stops: Arc::new(Mutex::new(HashMap::new())),
            starts: Arc::new(Mutex::new(HashMap::new())),
            history_pending: Arc::new(crate::history_pending::Pending::default()),
            history_poll: Arc::new(Mutex::new(())),
            history_tasks: Arc::new(tokio::sync::Semaphore::new(64)),
            store: None,
            history_root: None,
            clock: Arc::new(crate::simulated::SystemClock::default()),
            interval: std::time::Duration::from_secs(5),
            expiry: std::time::Duration::from_secs(900),
        }
    }
    pub fn with_history(mut self, root: std::path::PathBuf) -> Self {
        self.history_root = Some(root);
        self
    }
    fn queue_history(&self, task: HistoryTask) {
        if self.history_root.is_none() {
            return;
        }
        let Ok(permit) = self.history_tasks.clone().try_acquire_owned() else {
            crate::history::log_error();
            return;
        };
        let safety = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result: Result<(), Error> = async {
                match task {
                    HistoryTask::Job(job, outcome) => safety.record_job(&job, outcome).await,
                    HistoryTask::Clear(record) => {
                        if let Some(root) = &safety.history_root {
                            crate::history::append(root, &record).await?;
                        }
                        Ok(())
                    }
                    HistoryTask::Stop(ids) => {
                        for id in ids {
                            let job = safety.inner.job(&id).await?;
                            safety.record_job(&job, None).await?;
                        }
                        Ok(())
                    }
                    HistoryTask::Started(id, time) => {
                        let mut records = safety.history_pending.records.lock().await;
                        if let Some(record) = records.get_mut(&id) {
                            record.started_at = Some(time);
                            safety.checkpoint_history(&records);
                        }
                        Ok(())
                    }
                    HistoryTask::Poll(restarting, records) => {
                        safety.poll_history_records(restarting, records).await
                    }
                }
            }
            .await;
            if result.is_err() {
                crate::history::log_error();
            }
        });
    }
    fn record_job_best_effort(&self, job: &Job) {
        if matches!(
            job.state,
            JobState::Done | JobState::Cancelled | JobState::Failed | JobState::Ended
        ) {
            self.queue_history(HistoryTask::Job(job.clone(), None));
        }
    }
    pub async fn poll_history_best_effort(&self, restarting: bool) {
        let records = self.history_records(restarting).await;
        self.queue_history(HistoryTask::Poll(restarting, records));
    }
    /// Waits for in-flight history writes. Only history reads and CLI shutdown
    /// call this barrier; machine actions and status reads never call it.
    pub async fn flush_history(&self) {
        let permits = self.history_tasks.clone().acquire_many_owned(64).await;
        drop(permits);
        self.history_pending.flush().await;
    }
    fn history_pending_path(&self) -> Option<std::path::PathBuf> {
        let root = self.history_root.as_ref()?;
        match &self.store {
            Some(store) if store.file_name().is_some_and(|name| name != "safety.json") => {
                Some(store.with_extension("history-pending.json"))
            }
            _ => Some(root.join("history-pending.json")),
        }
    }
    fn checkpoint_history(&self, records: &crate::history_pending::Records) {
        if let Some(path) = self.history_pending_path() {
            self.history_pending.changed(&path, records);
        }
    }
    #[cfg(test)]
    pub(crate) fn history_write_hook(&self) -> Arc<crate::history_pending::WriteHook> {
        self.history_pending.hook.clone()
    }
    async fn record_job(&self, job: &Job, outcome: Option<&str>) -> Result<(), Error> {
        let Some(root) = &self.history_root else {
            return Ok(());
        };
        let Some(mut record) = self
            .history_pending
            .records
            .lock()
            .await
            .get(&job.id)
            .cloned()
        else {
            return Ok(());
        };
        let terminal = match job.state {
            JobState::Done => "done",
            JobState::Cancelled => "cancelled",
            JobState::Failed => "failed",
            JobState::Ended => "ended",
            _ => return Ok(()),
        };
        let stopped = terminal == "cancelled" && record.outcome == "stopped";
        record.outcome = outcome
            .unwrap_or(if stopped { "stopped" } else { terminal })
            .into();
        record.ended_at = crate::history::now();
        record.duration_seconds = record.started_at.as_deref().and_then(|start| {
            let start = crate::history::parse_time(start).ok()?;
            let end = crate::history::parse_time(&record.ended_at).ok()?;
            u64::try_from((end - start).whole_seconds()).ok()
        });
        crate::history::append(root, &record).await?;
        let mut records = self.history_pending.records.lock().await;
        // An older result must not remove a new execution of the same job id.
        if records
            .get(&job.id)
            .is_some_and(|pending| pending.started_at == record.started_at)
        {
            records.remove(&job.id);
            self.checkpoint_history(&records);
        }
        Ok(())
    }
    /// Polling only reads backend jobs; it never sends machine commands.
    pub async fn poll_history(&self, restarting: bool) -> Result<(), Error> {
        if self.history_root.is_none() {
            return Ok(());
        }
        let records = self.history_records(restarting).await;
        self.poll_history_records(restarting, records).await
    }
    async fn history_records(&self, restarting: bool) -> Vec<crate::history::Record> {
        self.history_pending
            .records
            .lock()
            .await
            .values()
            .filter(|r| restarting || r.started_at.is_some())
            .cloned()
            .collect()
    }
    async fn poll_history_records(
        &self,
        restarting: bool,
        records: Vec<crate::history::Record>,
    ) -> Result<(), Error> {
        let _poll = if restarting {
            self.history_poll.lock().await
        } else {
            match self.history_poll.try_lock() {
                Ok(lock) => lock,
                Err(_) => return Ok(()),
            }
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        use futures_util::StreamExt as _;
        let reads = futures_util::stream::iter(records)
            .map(|record| async move {
                let result = match record.job_id.as_deref() {
                    Some(id) => tokio::time::timeout_at(deadline, self.inner.job(id)).await,
                    None => Ok(Err(Error::NotFound)),
                };
                (record, result)
            })
            .buffer_unordered(64);
        tokio::pin!(reads);
        while let Some((record, result)) = reads.next().await {
            let Some(id) = record.job_id.as_deref() else {
                continue;
            };
            match result {
                Ok(Ok(mut job)) => {
                    if restarting && record.started_at.is_some() && job.state == JobState::Ready {
                        job.state = JobState::Ended;
                        self.record_job(&job, Some("unresolved_after_restart"))
                            .await?;
                    } else {
                        self.record_job(&job, None).await?;
                    }
                }
                Ok(Err(Error::NotFound)) if record.started_at.is_none() => {
                    let mut records = self.history_pending.records.lock().await;
                    if records.remove(id).is_some() {
                        self.checkpoint_history(&records);
                    }
                }
                _ if restarting && record.started_at.is_some() => {
                    let job = Job {
                        id: id.into(),
                        printer: record.printer,
                        name: String::new(),
                        state: JobState::Ended,
                        estimate: Estimate::default(),
                        progress: None,
                        seconds_left: None,
                        detail: None,
                    };
                    self.record_job(&job, Some("unresolved_after_restart"))
                        .await?;
                }
                _ => {}
            }
        }
        Ok(())
    }
    pub async fn history_source(&self, id: &str, source: &str) -> Result<(), Error> {
        let mut records = self.history_pending.records.lock().await;
        if let Some(record) = records.get_mut(id) {
            let label = crate::history::label(source);
            if record.printer_kind == "simulated" {
                record.file_name = Some(label.clone());
            }
            record.source = Some(label);
            self.checkpoint_history(&records);
        }
        Ok(())
    }
    pub fn timing(
        mut self,
        clock: Arc<dyn crate::simulated::Clock>,
        interval: std::time::Duration,
        expiry: std::time::Duration,
    ) -> Self {
        self.clock = clock;
        self.interval = interval;
        self.expiry = expiry;
        self
    }
    /// The task owns only a weak reference; dropping the server stops polling.
    pub fn monitor(self: &Arc<Self>) {
        self.monitor_history();
        let weak = Arc::downgrade(self);
        let interval = self.interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(safety) = weak.upgrade() else { break };
                let ids: Vec<_> = safety
                    .state
                    .lock()
                    .await
                    .cleared
                    .iter()
                    .filter(|(_, held)| **held)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    let _ = safety.fresh(&id).await;
                }
            }
        });
    }
    // History reads must not delay the existing bed-clear polling cadence.
    fn monitor_history(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let interval = self.interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(safety) = weak.upgrade() else { break };
                if safety.poll_history(false).await.is_err() {
                    crate::history::log_error();
                }
            }
        });
    }
    pub async fn review_digest(&self, id: Option<&str>) -> Result<Option<String>, Error> {
        let state = self.state.lock().await;
        let Some(id) = id else { return Ok(None) };
        let Some(review) = state.reviews.get(id).cloned() else {
            return Ok(None);
        };
        drop(state);
        let digest = hash(&file_bytes(&review.path).await?);
        if digest != review.hash {
            return Err(Error::Refused(
                "Prepared file changed. Prepare and review again.".into(),
            ));
        }
        Ok(Some(digest))
    }
    pub fn with_store(mut self, path: std::path::PathBuf) -> Self {
        self.store = Some(path);
        self
    }
    async fn checkpoint(&self, state: &State) -> Result<(), Error> {
        if let Some(path) = &self.store {
            let bytes = self
                .history_pending
                .safety_bytes(state)
                .map_err(|_| Error::Unavailable)?;
            write_state(path, &bytes)
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        Ok(())
    }
    pub async fn restore(&self, path: &Path) -> Result<(), String> {
        match read_state(path).await {
            Ok(bytes) if bytes.len() <= 16 << 20 => {
                let state: State = serde_json::from_slice(&bytes)
                    .map_err(|_| "Cannot read safety state. Check the private data folder.")?;
                if state.reviews.len() > 5000
                    || state.observed.len() > 64
                    || state.job_names.len() > 64
                    || state.terminal_seen.len() > 64
                {
                    return Err(
                        "Safety state exceeds its limits. Check the private data folder.".into(),
                    );
                }
                *self.state.lock().await = state;
                if let Some(pending) = self.history_pending_path() {
                    self.history_pending.restore(&pending, Some(&bytes)).await;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(pending) = self.history_pending_path() {
                    self.history_pending.restore(&pending, None).await;
                }
                Ok(())
            }
            _ => Err("Cannot read safety state. Check the private data folder.".into()),
        }
    }
    pub async fn save(&self, path: &Path) -> Result<(), String> {
        let state = self.state.lock().await;
        let bytes = self
            .history_pending
            .safety_bytes(&*state)
            .map_err(|_| "Cannot save safety state.")?;
        write_state(path, &bytes)
            .await
            .map_err(|_| "Cannot save safety state.".into())
    }

    fn observe(&self, state: &mut State, printers: &mut [Printer]) {
        let now = self.clock.now();
        for printer in printers {
            let observation = (printer.job_id.clone(), printer.detail.clone());
            let terminal_changed = printer.needs_clearing == Some(true)
                && (state.observed.get(&printer.id) != Some(&observation)
                    || !state
                        .terminal_seen
                        .get(&printer.id)
                        .copied()
                        .unwrap_or(false));
            state
                .terminal_seen
                .insert(printer.id.clone(), printer.needs_clearing == Some(true));
            let identity_changed = state
                .observed
                .get(&printer.id)
                .is_some_and(|old| old != &observation)
                || state
                    .job_names
                    .get(&printer.id)
                    .is_some_and(|old| old != &printer.job_name);
            state
                .job_names
                .insert(printer.id.clone(), printer.job_name.clone());
            state.observed.insert(printer.id.clone(), observation);
            let cleared = state.cleared.entry(printer.id.clone()).or_insert(false);
            let late = state
                .clearance
                .get(&printer.id)
                .is_none_or(|(created, last)| {
                    now.saturating_sub(*created) >= self.expiry
                        || now.saturating_sub(*last) > self.interval * 2
                });
            if printer.state != PrinterState::Idle || terminal_changed || identity_changed || late {
                *cleared = false;
            }
            if *cleared {
                if let Some((_, last)) = state.clearance.get_mut(&printer.id) {
                    *last = now;
                }
            }
            printer.needs_clearing =
                Some(!*cleared && !self.inner.bed_ready(&printer.id, printer.detail.as_deref()));
        }
    }
    async fn fresh(&self, id: &str) -> Result<Printer, Error> {
        let mut printer =
            match tokio::time::timeout(self.interval * 2, self.inner.printer(id)).await {
                Ok(Ok(printer)) => printer,
                _ => {
                    let mut state = self.state.lock().await;
                    state.cleared.insert(id.into(), false);
                    state.clearance.remove(id);
                    return Err(Error::Unavailable);
                }
            };
        self.observe(
            &mut *self.state.lock().await,
            std::slice::from_mut(&mut printer),
        );
        Ok(printer)
    }
}
impl Backend for Safety {
    fn queue_profile(&self, printer: &str) -> Option<(String, Options)> {
        self.inner.queue_profile(printer)
    }

    fn controls(&self, printer: &str) -> Vec<&'static str> {
        self.inner.controls(printer)
    }
    fn check_action(&self, printer: &str, action: &str) -> Result<(), Error> {
        self.inner.check_action(printer, action)
    }
    fn shutdown(&self) -> Reply<'_, ()> {
        self.inner.shutdown()
    }

    fn review_action<'a>(
        &'a self,
        action: &'a str,
        printer: &'a str,
        job: Option<&'a str>,
    ) -> Reply<'a, ()> {
        self.inner.review_action(action, printer, job)
    }
    fn printer<'a>(&'a self, id: &'a str) -> Reply<'a, Printer> {
        Box::pin(async move { self.fresh(id).await })
    }
    fn check_control(&self, printer: &str) -> Result<(), Error> {
        self.inner.check_control(printer)
    }

    fn printers(&self) -> Reply<'_, Vec<Printer>> {
        Box::pin(async {
            let mut printers =
                match tokio::time::timeout(self.interval * 2, self.inner.printers()).await {
                    Ok(Ok(printers)) => printers,
                    _ => {
                        let mut state = self.state.lock().await;
                        state.cleared.clear();
                        state.clearance.clear();
                        return Err(Error::Unavailable);
                    }
                };
            self.observe(&mut *self.state.lock().await, &mut printers);
            Ok(printers)
        })
    }
    fn clear<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            self.inner.check_action(id, "clear")?;
            let printer = self.fresh(id).await?;
            let mut state = self.state.lock().await;
            if state.observed.get(id) != Some(&(printer.job_id.clone(), printer.detail.clone())) {
                return Err(Error::Refused(
                    "The printer changed. Read fresh status before acknowledging its bed.".into(),
                ));
            }
            if printer.state != PrinterState::Idle {
                return Err(Error::Refused(format!(
                    "{} is not idle. Wait before acknowledging its bed.",
                    printer.name
                )));
            }
            self.inner.clear(id).await?;
            if self.inner.latches_terminal_reports(id) {
                state.terminal_seen.insert(id.into(), false);
            }
            state.cleared.insert(id.into(), true);
            state
                .clearance
                .insert(id.into(), (self.clock.now(), self.clock.now()));
            self.checkpoint(&state).await?;
            drop(state);
            self.queue_history(HistoryTask::Clear(crate::history::Record {
                printer: id.into(),
                printer_kind: self.inner.printer_kind(id).into(),
                job_id: None,
                job_name: None,
                file_name: None,
                file_size: None,
                source: None,
                outcome: "bed_clear".into(),
                started_at: None,
                ended_at: crate::history::now(),
                duration_seconds: None,
                estimate: None,
            }));
            Ok(())
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
            let digest = hash(&file_bytes(path).await?);
            let estimate = self.inner.prepare(id, path, printer, options).await?;
            let record = if self.history_root.is_some() {
                let record = async {
                    let name = crate::history::label(&self.inner.job(id).await?.name);
                    let source = crate::history::label(
                        &path.file_name().unwrap_or_default().to_string_lossy(),
                    );
                    let (file_name, file_size) = match self.inner.history_file(id).await? {
                        Some(file) => file,
                        None => (
                            source.clone(),
                            tokio::fs::metadata(path)
                                .await
                                .map_err(|_| Error::Unavailable)?
                                .len(),
                        ),
                    };
                    Ok::<_, Error>(crate::history::Record {
                        printer: printer.into(),
                        printer_kind: self.inner.printer_kind(printer).into(),
                        job_id: Some(id.into()),
                        job_name: Some(name),
                        file_name: Some(crate::history::label(&file_name)),
                        file_size: Some(file_size),
                        source: Some(source),
                        outcome: String::new(),
                        started_at: None,
                        ended_at: String::new(),
                        duration_seconds: None,
                        estimate: Some(estimate.clone()),
                    })
                }
                .await;
                match record {
                    Ok(record) => Some(record),
                    Err(_) => {
                        crate::history::log_error();
                        None
                    }
                }
            } else {
                None
            };
            let mut state = self.state.lock().await;
            state.reviews.insert(
                id.into(),
                Review {
                    printer: printer.into(),
                    path: path.into(),
                    hash: digest,
                },
            );
            drop(state);
            if let Some(record) = record {
                let mut records = self.history_pending.records.lock().await;
                if records.len() < 5000 {
                    records.insert(id.into(), record);
                    self.checkpoint_history(&records);
                } else {
                    crate::history::log_error();
                }
            }
            Ok(estimate)
        })
    }
    fn start<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let printer = self.inner.job_printer(id).await?;
            let start = {
                let mut starts = self.starts.lock().await;
                if starts.len() >= 64 && !starts.contains_key(&printer) {
                    return Err(Error::Unavailable);
                }
                starts.entry(printer.clone()).or_default().clone()
            };
            // Keep the per-printer guard through backend Start and reconciliation.
            // A waiting Start must repeat every check against fresh status.
            let _start = start.lock().await;
            self.inner.check_action(&printer, "start")?;
            let job = self.inner.job(id).await?;
            if job.state != JobState::Ready {
                return Err(Error::Refused(
                    "Only a ready job can be started. Read status to reconcile it.".into(),
                ));
            }
            let printer = self.fresh(&job.printer).await?;
            let review = self
                .state
                .lock()
                .await
                .reviews
                .get(id)
                .cloned()
                .ok_or_else(|| Error::Refused("Prepare and review this job again.".into()))?;
            if review.printer != job.printer
                || hash(&file_bytes(&review.path).await?) != review.hash
            {
                return Err(Error::Refused(
                    "Prepared file or printer changed. Prepare and review this job again.".into(),
                ));
            }
            let mut state = self.state.lock().await;
            if state.observed.get(&job.printer)
                != Some(&(printer.job_id.clone(), printer.detail.clone()))
            {
                return Err(Error::Refused(
                    "The printer changed. Read fresh status before starting.".into(),
                ));
            }
            if printer.state != PrinterState::Idle {
                return Err(Error::Refused("Printer readiness is unknown or the printer is busy. Read fresh status before starting.".into()));
            }
            if !state.cleared.get(&job.printer).copied().unwrap_or(false)
                && !self
                    .inner
                    .bed_ready(&job.printer, printer.detail.as_deref())
            {
                return Err(Error::Refused(format!(
                    "Check that {} has a clear bed, then acknowledge it in this run or through the server for {}.",
                    printer.name, printer.id
                )));
            }
            if state.reviews.get(id).is_none_or(|current| {
                current.printer != review.printer
                    || current.path != review.path
                    || current.hash != review.hash
            }) {
                return Err(Error::Refused(
                    "Prepared job changed. Prepare and review again.".into(),
                ));
            }
            if self.stops.lock().await.get(id).copied().unwrap_or(false) {
                return Err(Error::Refused(
                    "Start was cancelled. Prepare a new job.".into(),
                ));
            }
            // A failed or uncertain start can still have moved the machine.
            state.cleared.insert(job.printer.clone(), false);
            self.checkpoint(&state).await?;
            drop(state);
            let time = crate::history::now();
            if let Ok(mut records) = self.history_pending.records.try_lock() {
                if let Some(record) = records.get_mut(id) {
                    record.started_at = Some(time);
                    self.checkpoint_history(&records);
                }
            } else {
                self.queue_history(HistoryTask::Started(id.into(), time));
            }
            let result = self.inner.start(id).await;
            if self.stops.lock().await.get(id).copied().unwrap_or(false) {
                // Backends reconcile uncertain starts and verify the live job before cancelling.
                if let Ok(job) = self.inner.job(id).await {
                    if job.state == JobState::Cancelled {
                        self.record_job_best_effort(&job);
                        return Ok(job);
                    }
                }
                let job = self.inner.cancel(id).await?;
                self.record_job_best_effort(&job);
                return Ok(job);
            }
            if let Ok(job) = &result {
                self.record_job_best_effort(job);
            }
            result
        })
    }
    fn cancel<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let printer = self.inner.job_printer(id).await?;
            self.inner.check_action(&printer, "cancel")?;
            {
                let mut stops = self.stops.lock().await;
                if stops.len() >= 5000 && !stops.contains_key(id) {
                    return Err(Error::Unavailable);
                }
                stops.insert(id.into(), true);
            }
            let before = self.inner.job(id).await?;
            if before.state == JobState::Cancelled {
                self.record_job_best_effort(&before);
                return Ok(before);
            }
            if before.state != JobState::Ready {
                self.state
                    .lock()
                    .await
                    .cleared
                    .insert(before.printer, false);
            }
            let job = self.inner.cancel(id).await?;
            self.record_job_best_effort(&job);
            Ok(job)
        })
    }
    fn stop<'a>(
        &'a self,
        printer: &'a str,
        identity: &'a str,
        name: &'a str,
        path: Option<&'a str>,
    ) -> Reply<'a, Printer> {
        Box::pin(async move {
            self.inner.check_action(printer, "stop")?;
            let result = self.inner.stop(printer, identity, name, path).await;
            self.state
                .lock()
                .await
                .cleared
                .insert(printer.into(), false);
            let ids = {
                let mut records = self.history_pending.records.lock().await;
                let mut ids = Vec::new();
                let mut changed = false;
                for record in records
                    .values_mut()
                    .filter(|record| record.printer == printer && record.started_at.is_some())
                {
                    record.outcome = "stopped".into();
                    changed = true;
                    if let Some(id) = &record.job_id {
                        ids.push(id.clone());
                    }
                }
                if changed {
                    self.checkpoint_history(&records);
                }
                ids
            };
            self.queue_history(HistoryTask::Stop(ids));
            let mut sent = result?;
            // Read status only after the stop has been sent, so the reply carries the
            // printer's state and its bed-check requirement. A Stop that reached the
            // printer is never reported as failed because that read failed.
            match self.printer(printer).await {
                Ok(read) => Ok(read),
                Err(_) => {
                    sent.needs_clearing = Some(true);
                    Ok(sent)
                }
            }
        })
    }
    fn pause<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            self.inner.check_action(printer, "pause")?;
            let p = self.fresh(printer).await?;
            if p.state != PrinterState::Printing {
                return Err(Error::Refused(
                    "Printer is not printing. Read fresh status.".into(),
                ));
            }
            self.inner.pause(printer).await
        })
    }
    fn resume<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            self.inner.check_action(printer, "resume")?;
            let p = self.fresh(printer).await?;
            if p.state != PrinterState::Paused
                || p.detail.as_deref().is_some_and(|s| {
                    !matches!(
                        s.split("; ").next(),
                        Some("PAUSED" | "PAUSE" | "Paused" | "paused")
                    )
                })
            {
                return Err(Error::Refused("Printer readiness is unknown or attention is required. Read fresh status before resuming.".into()));
            }
            self.inner.resume(printer).await
        })
    }
    fn job_printer<'a>(&'a self, id: &'a str) -> Reply<'a, String> {
        self.inner.job_printer(id)
    }
    fn job<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let job = self.inner.job(id).await?;
            if matches!(job.state, JobState::Printing | JobState::Paused)
                && self.stops.lock().await.get(id).copied().unwrap_or(false)
            {
                let job = self.inner.cancel(id).await?;
                self.record_job_best_effort(&job);
                return Ok(job);
            }
            self.record_job_best_effort(&job);
            Ok(job)
        })
    }
    fn forget<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            let job = self.inner.job(id).await.ok();
            self.inner.forget(id).await?;
            if let Some(job) = job.filter(|job| {
                matches!(
                    job.state,
                    JobState::Done | JobState::Cancelled | JobState::Failed | JobState::Ended
                )
            }) {
                self.record_job_best_effort(&job);
            } else {
                let mut records = self.history_pending.records.lock().await;
                if records.remove(id).is_some() {
                    self.checkpoint_history(&records);
                }
            }
            self.state.lock().await.reviews.remove(id);
            self.stops.lock().await.remove(id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use tokio::time::timeout;

    struct BlockingStart {
        jobs: Mutex<HashMap<String, Job>>,
        entered: Semaphore,
        release: Semaphore,
        calls: AtomicUsize,
    }

    impl BlockingStart {
        fn new() -> Self {
            Self {
                jobs: Mutex::new(HashMap::new()),
                entered: Semaphore::new(0),
                release: Semaphore::new(0),
                calls: AtomicUsize::new(0),
            }
        }

        async fn wait_for_start(&self) {
            timeout(Duration::from_secs(5), self.entered.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
        }
    }

    impl Backend for BlockingStart {
        fn bed_ready(&self, _printer: &str, _detail: Option<&str>) -> bool {
            true
        }

        fn printers(&self) -> Reply<'_, Vec<Printer>> {
            Box::pin(async {
                let jobs = self.jobs.lock().await;
                Ok(["one", "two"]
                    .into_iter()
                    .map(|id| {
                        let active = jobs
                            .values()
                            .find(|job| job.printer == id && job.state == JobState::Printing);
                        Printer {
                            id: id.into(),
                            name: id.into(),
                            model: "Test printer".into(),
                            state: if active.is_some() {
                                PrinterState::Printing
                            } else {
                                PrinterState::Idle
                            },
                            controls: crate::backend::ALL_CONTROLS
                                .iter()
                                .map(|action| (*action).into())
                                .collect(),
                            job_id: active.map(|job| job.id.clone()),
                            job_name: active.map(|job| job.name.clone()),
                            job_path: None,
                            detail: None,
                            needs_clearing: Some(false),
                            progress: None,
                            seconds_left: None,
                            nozzle_temperature: None,
                            bed_temperature: None,
                        }
                    })
                    .collect())
            })
        }

        fn prepare<'a>(
            &'a self,
            id: &'a str,
            _path: &'a Path,
            printer: &'a str,
            _options: Options,
        ) -> Reply<'a, Estimate> {
            Box::pin(async move {
                self.jobs.lock().await.insert(
                    id.into(),
                    Job {
                        id: id.into(),
                        printer: printer.into(),
                        name: id.into(),
                        state: JobState::Ready,
                        estimate: Estimate::default(),
                        progress: None,
                        seconds_left: None,
                        detail: None,
                    },
                );
                Ok(Estimate::default())
            })
        }

        fn start<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                let mut jobs = self.jobs.lock().await;
                let job = jobs.get_mut(id).ok_or(Error::NotFound)?;
                if job.state == JobState::Ready {
                    job.state = JobState::Printing;
                }
                Ok(job.clone())
            })
        }

        fn cancel<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
            Box::pin(async move {
                let mut jobs = self.jobs.lock().await;
                let job = jobs.get_mut(id).ok_or(Error::NotFound)?;
                job.state = JobState::Cancelled;
                Ok(job.clone())
            })
        }

        fn stop<'a>(
            &'a self,
            printer: &'a str,
            _identity: &'a str,
            _name: &'a str,
            _path: Option<&'a str>,
        ) -> Reply<'a, Printer> {
            // No live execution yet: still return promptly while Start is blocked.
            Box::pin(async move { self.printer(printer).await })
        }

        fn pause<'a>(&'a self, _printer: &'a str) -> Reply<'a, Job> {
            Box::pin(async { Err(Error::Busy) })
        }

        fn resume<'a>(&'a self, _printer: &'a str) -> Reply<'a, Job> {
            Box::pin(async { Err(Error::Busy) })
        }

        fn job<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
            Box::pin(async move {
                self.jobs
                    .lock()
                    .await
                    .get(id)
                    .cloned()
                    .ok_or(Error::NotFound)
            })
        }

        fn forget<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
            Box::pin(async move {
                self.jobs.lock().await.remove(id);
                Ok(())
            })
        }
    }

    async fn prepared(printers: &[&str]) -> (tempfile::TempDir, Safety, Arc<BlockingStart>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviewed.3mf");
        tokio::fs::write(&path, b"synthetic reviewed bytes")
            .await
            .unwrap();
        let backend = Arc::new(BlockingStart::new());
        let safety = Safety::new(backend.clone());
        for (index, printer) in printers.iter().enumerate() {
            safety
                .prepare(&format!("job{index}"), &path, printer, Options::default())
                .await
                .unwrap();
        }
        (dir, safety, backend)
    }

    fn spawn_start(
        safety: &Safety,
        id: &'static str,
    ) -> tokio::task::JoinHandle<Result<Job, Error>> {
        let safety = safety.clone();
        tokio::spawn(async move { safety.start(id).await })
    }

    #[tokio::test]
    async fn start_lock_serializes_sensor_ready_printer() {
        let (_dir, safety, backend) = prepared(&["one", "one"]).await;
        let first = spawn_start(&safety, "job0");
        backend.wait_for_start().await;
        let mut second = spawn_start(&safety, "job1");
        assert!(timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        backend.release.add_permits(1);
        assert_eq!(
            timeout(Duration::from_secs(5), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .state,
            JobState::Printing
        );
        let result = timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(Error::Refused(message)) if message.contains("busy")));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_lock_does_not_delay_stop_or_cancel() {
        let (_dir, safety, backend) = prepared(&["one"]).await;
        let first = spawn_start(&safety, "job0");
        backend.wait_for_start().await;
        timeout(
            Duration::from_secs(5),
            safety.stop("one", "job0", "job0", None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(5), safety.cancel("job0"))
                .await
                .unwrap()
                .unwrap()
                .state,
            JobState::Cancelled
        );
        assert!(!first.is_finished());
        backend.release.add_permits(1);
        assert_eq!(
            timeout(Duration::from_secs(5), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .state,
            JobState::Cancelled
        );
    }

    #[tokio::test]
    async fn start_lock_is_independent_per_printer() {
        let (_dir, safety, backend) = prepared(&["one", "two"]).await;
        let first = spawn_start(&safety, "job0");
        backend.wait_for_start().await;
        let second = spawn_start(&safety, "job1");
        backend.wait_for_start().await;
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert!(!first.is_finished());
        backend.release.add_permits(2);
        for task in [first, second] {
            assert_eq!(
                timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .state,
                JobState::Printing
            );
        }
    }
}
