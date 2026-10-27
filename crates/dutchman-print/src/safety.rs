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
#[derive(serde::Serialize, serde::Deserialize)]
struct Review {
    printer: String,
    path: std::path::PathBuf,
    hash: String,
}

pub struct Safety {
    inner: Arc<dyn Backend>,
    state: Mutex<State>,
    stops: Mutex<HashMap<String, bool>>,
    store: Option<std::path::PathBuf>,
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
            state: Mutex::new(State::default()),
            stops: Mutex::new(HashMap::new()),
            store: None,
            clock: Arc::new(crate::simulated::SystemClock::default()),
            interval: std::time::Duration::from_secs(5),
            expiry: std::time::Duration::from_secs(900),
        }
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
        let weak = Arc::downgrade(self);
        let interval = self.interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(safety) = weak.upgrade() else { break };
                let mut state = safety.state.lock().await;
                let ids: Vec<_> = state
                    .cleared
                    .iter()
                    .filter(|(_, held)| **held)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    let _ = safety.fresh(&mut state, &id).await;
                }
            }
        });
    }
    pub async fn review_digest(&self, id: Option<&str>) -> Result<Option<String>, Error> {
        let state = self.state.lock().await;
        let Some(id) = id else { return Ok(None) };
        let Some(review) = state.reviews.get(id) else {
            return Ok(None);
        };
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
            let bytes = serde_json::to_vec(state).map_err(|_| Error::Unavailable)?;
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
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("Cannot read safety state. Check the private data folder.".into()),
        }
    }
    pub async fn save(&self, path: &Path) -> Result<(), String> {
        let state = self.state.lock().await;
        let bytes = serde_json::to_vec(&*state).map_err(|_| "Cannot save safety state.")?;
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
    async fn fresh(&self, state: &mut State, id: &str) -> Result<Printer, Error> {
        let mut printer =
            match tokio::time::timeout(self.interval * 2, self.inner.printer(id)).await {
                Ok(Ok(printer)) => printer,
                _ => {
                    state.cleared.insert(id.into(), false);
                    state.clearance.remove(id);
                    return Err(Error::Unavailable);
                }
            };
        self.observe(state, std::slice::from_mut(&mut printer));
        Ok(printer)
    }
}
impl Backend for Safety {
    fn review_action<'a>(
        &'a self,
        action: &'a str,
        printer: &'a str,
        job: Option<&'a str>,
    ) -> Reply<'a, ()> {
        self.inner.review_action(action, printer, job)
    }
    fn printer<'a>(&'a self, id: &'a str) -> Reply<'a, Printer> {
        Box::pin(async move { self.fresh(&mut *self.state.lock().await, id).await })
    }
    fn check_control(&self, printer: &str) -> Result<(), Error> {
        self.inner.check_control(printer)
    }

    fn printers(&self) -> Reply<'_, Vec<Printer>> {
        Box::pin(async {
            let mut state = self.state.lock().await;
            let mut printers =
                match tokio::time::timeout(self.interval * 2, self.inner.printers()).await {
                    Ok(Ok(printers)) => printers,
                    _ => {
                        state.cleared.clear();
                        state.clearance.clear();
                        return Err(Error::Unavailable);
                    }
                };
            self.observe(&mut state, &mut printers);
            Ok(printers)
        })
    }
    fn clear<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let printer = self.fresh(&mut state, id).await?;
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
            self.checkpoint(&state).await
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
            self.state.lock().await.reviews.insert(
                id.into(),
                Review {
                    printer: printer.into(),
                    path: path.into(),
                    hash: digest,
                },
            );
            Ok(estimate)
        })
    }
    fn start<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let job = self.inner.job(id).await?;
            self.inner.check_control(&job.printer)?;
            if job.state != JobState::Ready {
                return Err(Error::Refused(
                    "Only a ready job can be started. Read status to reconcile it.".into(),
                ));
            }
            let printer = self.fresh(&mut state, &job.printer).await?;
            if printer.state != PrinterState::Idle {
                return Err(Error::Refused("Printer readiness is unknown or the printer is busy. Read fresh status before starting.".into()));
            }
            if printer.needs_clearing != Some(false) {
                return Err(Error::Refused(format!(
                    "Check that {} has a clear bed, then acknowledge it in this run or through the server for {}.",
                    printer.name, printer.id
                )));
            }
            let review = state
                .reviews
                .get(id)
                .ok_or_else(|| Error::Refused("Prepare and review this job again.".into()))?;
            if review.printer != job.printer
                || hash(&file_bytes(&review.path).await?) != review.hash
            {
                return Err(Error::Refused(
                    "Prepared file or printer changed. Prepare and review this job again.".into(),
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
            let result = self.inner.start(id).await;
            if self.stops.lock().await.get(id).copied().unwrap_or(false) {
                // Backends reconcile uncertain starts and verify the live job before cancelling.
                return self.inner.cancel(id).await;
            }
            result
        })
    }
    fn cancel<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            {
                let mut stops = self.stops.lock().await;
                if stops.len() >= 5000 && !stops.contains_key(id) {
                    return Err(Error::Unavailable);
                }
                stops.insert(id.into(), true);
            }
            let mut state = self.state.lock().await;
            let before = self.inner.job(id).await?;
            self.inner.check_control(&before.printer)?;
            if before.state == JobState::Cancelled {
                return Ok(before);
            }
            let result = self.inner.cancel(id).await;
            if before.state != JobState::Ready {
                state.cleared.insert(before.printer, false);
            }
            result
        })
    }
    fn pause<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            self.inner.check_control(printer)?;
            let mut state = self.state.lock().await;
            let p = self.fresh(&mut state, printer).await?;
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
            self.inner.check_control(printer)?;
            let mut state = self.state.lock().await;
            let p = self.fresh(&mut state, printer).await?;
            if p.state != PrinterState::Paused
                || p.detail
                    .as_deref()
                    .is_some_and(|s| !matches!(s, "PAUSED" | "PAUSE"))
            {
                return Err(Error::Refused("Printer readiness is unknown or attention is required. Read fresh status before resuming.".into()));
            }
            self.inner.resume(printer).await
        })
    }
    fn job<'a>(&'a self, id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let _state = self.state.lock().await;
            let job = self.inner.job(id).await?;
            if matches!(job.state, JobState::Printing | JobState::Paused)
                && self.stops.lock().await.get(id).copied().unwrap_or(false)
            {
                return self.inner.cancel(id).await;
            }
            Ok(job)
        })
    }
    fn forget<'a>(&'a self, id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            self.inner.forget(id).await?;
            state.reviews.remove(id);
            self.stops.lock().await.remove(id);
            Ok(())
        })
    }
}
