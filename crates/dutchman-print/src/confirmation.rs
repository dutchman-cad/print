//! Single-use confirmations bound to fresh observations and the reviewed input.
use crate::backend::{Backend, Error, JobState, Printer, PrinterState};
use crate::safety::Safety;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub action: String,
    pub printer: String,
    pub job: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Observation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_path: Option<String>,
    pub state: PrinterState,
    pub detail: Option<String>,
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    pub progress: Option<f64>,
    pub needs_clearing: Option<bool>,
}
impl Observation {
    /// Whether the printer is still in the situation the person confirmed. Start and clear
    /// need the same observation; pause, resume and cancel need the same state and job, since
    /// progress and temperatures move while a print runs and stopping it must stay possible.
    fn matches(&self, confirmed: &Observation, action: &str) -> bool {
        match action {
            "pause" | "resume" | "cancel" | "stop" => {
                self.state == confirmed.state
                    && self.job_id == confirmed.job_id
                    && self.job_name == confirmed.job_name
                    && self.job_path == confirmed.job_path
            }
            _ => self == confirmed,
        }
    }
}
impl From<Printer> for Observation {
    fn from(p: Printer) -> Self {
        Self {
            job_path: p.job_path,
            state: p.state,
            detail: p.detail,
            job_id: p.job_id,
            job_name: p.job_name,
            progress: p.progress.map(|p| (p * 100.0).floor() / 100.0),
            needs_clearing: p.needs_clearing,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Challenge {
    pub id: String,
    #[serde(flatten)]
    pub intent: Intent,
    pub summary: String,
    pub observed: Observation,
    /// Unix seconds.
    pub expires_at: u64,
}
#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    challenge: Challenge,
    digest: Option<String>,
    used: bool,
    job_state: Option<JobState>,
}
#[derive(Default, Deserialize, Serialize)]
struct Ledger {
    challenges: HashMap<String, Entry>,
    headers: HashMap<String, u64>,
}
pub struct Confirmations {
    ledger: Mutex<Option<Ledger>>,
    path: Option<PathBuf>,
}
fn refused() -> Error {
    Error::Refused("Confirmation is invalid, used, or expired. Confirm again.".into())
}
impl Confirmations {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            ledger: Mutex::new(None),
            path,
        }
    }
    async fn load(&self, ledger: &mut Option<Ledger>) -> Result<(), Error> {
        if ledger.is_none() {
            *ledger = Some(if let Some(path) = &self.path {
                match crate::safety::read_state(path).await {
                    Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| Error::Unavailable)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
                    Err(_) => return Err(Error::Unavailable),
                }
            } else {
                Ledger::default()
            });
        }
        if ledger
            .as_ref()
            .is_some_and(|l| l.challenges.len() > 10000 || l.headers.len() > 10000)
        {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    async fn save(&self, ledger: &Ledger) -> Result<(), Error> {
        if let Some(path) = &self.path {
            let bytes = serde_json::to_vec(ledger).map_err(|_| Error::Unavailable)?;
            if bytes.len() > 16 << 20 {
                return Err(Error::Unavailable);
            }
            crate::safety::write_state(path, &bytes)
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        Ok(())
    }
    pub async fn issue(
        &self,
        safety: &Safety,
        intent: Intent,
        now: Duration,
        ttl: Duration,
    ) -> Result<Challenge, Error> {
        if intent.printer.len() > 64
            || intent.job.as_ref().is_some_and(|j| j.len() > 64)
            || intent.action.len() > 6
        {
            return Err(refused());
        }
        if intent.action != "clear" {
            safety.check_control(&intent.printer)?;
        }
        let printer = safety.printer(&intent.printer).await?;
        let job = if let Some(id) = &intent.job {
            let job = safety.job(id).await?;
            if job.printer != intent.printer {
                return Err(refused());
            }
            Some(job)
        } else {
            None
        };
        let possible = match intent.action.as_str() {
            "start" => {
                job.as_ref().is_some_and(|j| j.state == JobState::Ready)
                    && printer.state == PrinterState::Idle
                    // Terminal idle states may retain a historical identity. Native
                    // readiness is validated by review_action before issuing the challenge.
                    && printer.needs_clearing == Some(false)
            }
            "clear" => job.is_none() && printer.state == PrinterState::Idle,
            "cancel" => {
                job.as_ref().is_some_and(|j| {
                    matches!(
                        j.state,
                        JobState::Ready | JobState::Printing | JobState::Paused
                    )
                }) && printer.state != PrinterState::Offline
            }
            "stop" => {
                job.is_none()
                    && printer.job_id.is_some()
                    && printer.job_name.is_some()
                    && matches!(
                        printer.state,
                        PrinterState::Printing | PrinterState::Paused | PrinterState::Attention
                    )
            }
            "pause" => job.is_none() && printer.state == PrinterState::Printing,
            "resume" => {
                job.is_none()
                    && printer.state == PrinterState::Paused
                    && printer.detail.as_deref().is_none_or(|s| {
                        matches!(
                            s.split("; ").next(),
                            Some("PAUSED" | "PAUSE" | "Paused" | "paused")
                        )
                    })
            }
            _ => false,
        };
        if !possible {
            if intent.action == "start" {
                if let Some(detail) = printer.detail.as_deref().filter(|d| d.contains("HMS")) {
                    return Err(Error::Refused(format!(
                        "Resolve Bambu alerts before starting: {detail}."
                    )));
                }
            }
            return Err(Error::Refused(
                "This action is not possible now. Read fresh status and check bed clearance."
                    .into(),
            ));
        }
        safety
            .review_action(&intent.action, &intent.printer, intent.job.as_deref())
            .await?;
        let digest = safety.review_digest(intent.job.as_deref()).await?;
        if intent.action == "start" && digest.is_none() {
            return Err(Error::Refused("Prepare and review this job again.".into()));
        }
        let action = match intent.action.as_str() {
            "start" => "Start printing",
            "cancel" => "Cancel printing",
            "stop" => "Stop printing",
            "pause" => "Pause printing",
            "resume" => "Resume printing",
            _ => "Acknowledge the checked, clear bed",
        };
        let name = job
            .as_ref()
            .map(|j| format!(" '{}'", j.name))
            .or_else(|| printer.job_name.as_ref().map(|n| format!(" '{n}'")))
            .unwrap_or_default();
        let detail = printer
            .detail
            .clone()
            .unwrap_or_else(|| printer.state.to_string());
        let bed = if intent.action == "stop" {
            ""
        } else if printer.needs_clearing == Some(false) {
            ", bed clear"
        } else {
            ", bed clearance required"
        };
        let live = printer
            .job_id
            .as_ref()
            .map(|id| format!(", job {id}"))
            .unwrap_or_default();
        let file_path = printer
            .job_path
            .as_ref()
            .map(|path| format!(", file {path}"))
            .unwrap_or_default();
        let progress = printer
            .progress
            .map(|p| format!(", {}%", (p * 100.0).floor()))
            .unwrap_or_default();
        let challenge = Challenge {
            id: crate::new_job_id().map_err(Error::Failed)?,
            summary: format!(
                "{action}{name} on {} ({detail}{bed}{live}{file_path}{progress})?",
                printer.name
            ),
            observed: printer.into(),
            intent,
            expires_at: now.saturating_add(ttl).as_secs(),
        };
        let mut guard = self.ledger.lock().await;
        self.load(&mut guard).await?;
        let ledger = guard.as_mut().ok_or(Error::Unavailable)?;
        ledger
            .challenges
            .retain(|_, e| e.challenge.expires_at > now.as_secs());
        ledger.headers.retain(|_, expiry| *expiry > now.as_secs());
        if ledger.challenges.len() >= 10000 {
            return Err(Error::Unavailable);
        }
        ledger.challenges.insert(
            challenge.id.clone(),
            Entry {
                challenge: challenge.clone(),
                digest,
                used: false,
                job_state: job.as_ref().map(|j| j.state),
            },
        );
        self.save(ledger).await?;
        Ok(challenge)
    }
    pub async fn consume(
        &self,
        safety: &Safety,
        id: &str,
        intent: &Intent,
        header: Option<&str>,
        now: Duration,
    ) -> Result<Observation, Error> {
        let started = std::time::Instant::now();
        let mut guard = self.ledger.lock().await;
        self.load(&mut guard).await?;
        let ledger = guard.as_mut().ok_or(Error::Unavailable)?;
        let entry = ledger.challenges.get(id).ok_or_else(refused)?.clone();
        if entry.used
            || entry.challenge.expires_at <= now.as_secs()
            || entry.challenge.intent.action != intent.action
            || entry.challenge.intent.printer != intent.printer
            || entry.challenge.intent.job != intent.job
        {
            return Err(refused());
        }
        let header_hash = header.map(|h| crate::safety::hash(h.as_bytes()));
        if header_hash.as_ref().is_some_and(|h| {
            ledger
                .headers
                .get(h)
                .is_some_and(|expiry| *expiry > now.as_secs())
        }) {
            return Err(refused());
        }
        if ledger.headers.len() >= 10000 {
            return Err(Error::Unavailable);
        }
        drop(guard);
        let job_state = if let Some(id) = &intent.job {
            Some(safety.job(id).await?.state)
        } else {
            None
        };
        let observed: Observation = safety.printer(&intent.printer).await?.into();
        let digest = safety.review_digest(intent.job.as_deref()).await?;
        if !observed.matches(&entry.challenge.observed, &intent.action)
            || digest != entry.digest
            || job_state != entry.job_state
        {
            return Err(Error::Refused(
                "The printer changed since you confirmed; confirm again.".into(),
            ));
        }
        let expiry = entry.challenge.expires_at;
        if now.saturating_add(started.elapsed()).as_secs() >= expiry {
            return Err(refused());
        }
        // Network reads above never hold the shared ledger. Recheck single-use state
        // atomically before consuming the challenge and header.
        let mut guard = self.ledger.lock().await;
        self.load(&mut guard).await?;
        let ledger = guard.as_mut().ok_or(Error::Unavailable)?;
        if now.saturating_add(started.elapsed()).as_secs() >= expiry
            || ledger.challenges.get(id).is_none_or(|entry| entry.used)
            || header_hash.as_ref().is_some_and(|h| {
                ledger
                    .headers
                    .get(h)
                    .is_some_and(|until| *until > now.as_secs())
            })
        {
            return Err(refused());
        }
        if ledger.headers.len() >= 10000 {
            return Err(Error::Unavailable);
        }
        let entry = ledger.challenges.get_mut(id).ok_or_else(refused)?;
        entry.used = true;
        if let Some(hash) = header_hash {
            ledger
                .headers
                .insert(hash, now.as_secs().saturating_add(600).max(expiry));
        }
        self.save(ledger).await?;
        Ok(observed)
    }
}
