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
    cleared: HashMap<String, bool>,
    reviews: HashMap<String, Review>,
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
impl Safety {
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            state: Mutex::new(State::default()),
            stops: Mutex::new(HashMap::new()),
            store: None,
        }
    }
    pub fn with_store(mut self, path: std::path::PathBuf) -> Self {
        self.store = Some(path);
        self
    }
    async fn checkpoint(&self, state: &State) -> Result<(), Error> {
        if let Some(path) = &self.store {
            let bytes = serde_json::to_vec(state).map_err(|_| Error::Unavailable)?;
            let temporary = path.with_extension("new");
            tokio::fs::write(&temporary, bytes)
                .await
                .map_err(|_| Error::Unavailable)?;
            tokio::fs::rename(temporary, path)
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        Ok(())
    }
    pub async fn restore(&self, path: &Path) -> Result<(), String> {
        match read_state(path).await {
            Ok(bytes) if bytes.len() <= 16 << 20 => {
                *self.state.lock().await = serde_json::from_slice(&bytes)
                    .map_err(|_| "Cannot read safety state. Check the private data folder.")?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("Cannot read safety state. Check the private data folder.".into()),
        }
    }
    pub async fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec(&*self.state.lock().await)
            .map_err(|_| "Cannot save safety state.")?;
        let temporary = path.with_extension("new");
        tokio::fs::write(&temporary, bytes)
            .await
            .map_err(|_| "Cannot save safety state.")?;
        tokio::fs::rename(temporary, path)
            .await
            .map_err(|_| "Cannot save safety state.".into())
    }
    fn observe(state: &mut State, printers: &mut [Printer]) {
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
            state.observed.insert(printer.id.clone(), observation);
            let cleared = state.cleared.entry(printer.id.clone()).or_insert(false);
            if printer.state != PrinterState::Idle || terminal_changed {
                *cleared = false;
            }
            printer.needs_clearing = Some(!*cleared);
        }
    }
    async fn fresh(&self, state: &mut State, id: &str) -> Result<Printer, Error> {
        let mut printers = self.inner.printers().await?;
        Self::observe(state, &mut printers);
        printers
            .into_iter()
            .find(|p| p.id == id)
            .ok_or(Error::NotFound)
    }
}
impl Backend for Safety {
    fn printers(&self) -> Reply<'_, Vec<Printer>> {
        Box::pin(async {
            let mut state = self.state.lock().await;
            let mut printers = self.inner.printers().await?;
            Self::observe(&mut state, &mut printers);
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
            if !state.cleared.get(&job.printer).copied().unwrap_or(false) {
                return Err(Error::Refused(format!(
                    "Check that {} has a clear bed, then acknowledge it with clear {}.",
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
            if p.state != PrinterState::Paused || p.detail.as_deref().is_some_and(|s| s != "PAUSED")
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
