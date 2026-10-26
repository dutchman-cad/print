//! In-memory demonstration backend. Optional external slicing; no printer connections.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::backend::{
    Backend, Error, Estimate, Job, JobState, Options, Printer, PrinterState, Reply,
};

pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

pub struct SystemClock(Instant);
impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Running {
    job: Job,
    created: Duration,
    elapsed: Duration,
    since: Option<Duration>,
}

pub struct Simulated {
    printers: Vec<crate::config::Printer>,
    jobs: Mutex<BTreeMap<String, Running>>,
    clock: Arc<dyn Clock>,
    slicer: Option<crate::slicer::Slicer>,
}

impl Simulated {
    pub fn new(config: crate::config::Config, clock: Arc<dyn Clock>) -> Result<Self, String> {
        config.validate()?;
        let slicer = if config.backend.slice {
            Some(crate::slicer::Slicer::new(
                config.slicer.clone().ok_or("Set [slicer].")?,
            )?)
        } else {
            None
        };
        Ok(Self {
            slicer,
            printers: config.printers,
            jobs: Mutex::new(BTreeMap::new()),
            clock,
        })
    }

    /// CLI snapshots contain only simulation state, never backend credentials.
    pub fn restore(&self, path: &Path) -> Result<(), String> {
        use std::io::Read as _;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("Cannot read simulated jobs.".into()),
        };
        let mut bytes = Vec::new();
        file.take((16 << 20) + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read simulated jobs.")?;
        if bytes.len() > 16 << 20 {
            return Err("Simulated job storage is too big.".into());
        }
        let mut jobs: BTreeMap<String, Running> = serde_json::from_slice(&bytes).map_err(|_| {
            "Cannot read simulated jobs. Remove the simulation state file to reset it."
        })?;
        if jobs.len() > 5000 {
            return Err("Too many simulated jobs.".into());
        }
        let now = self.clock.now();
        jobs.retain(|id, r| {
            let expired = now.saturating_sub(r.created) > Duration::from_secs(3600);
            if expired && id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                if let Some(root) = path.parent() {
                    let _ = std::fs::remove_dir_all(root.join(id));
                }
            }
            !expired && self.printers.iter().any(|p| p.id == r.job.printer)
        });
        *self.jobs.lock().map_err(|_| "Simulation is unavailable.")? = jobs;
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let jobs = self.jobs().map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(&*jobs).map_err(|_| "Cannot save simulated jobs.")?;
        let temporary = path.with_extension("new");
        std::fs::write(&temporary, bytes).map_err(|_| "Cannot save simulated jobs.")?;
        std::fs::rename(temporary, path).map_err(|_| "Cannot save simulated jobs.".into())
    }

    fn jobs(&self) -> Result<MutexGuard<'_, BTreeMap<String, Running>>, Error> {
        let mut jobs = self.jobs.lock().map_err(|_| Error::Unavailable)?;
        for running in jobs.values_mut() {
            let elapsed = running.elapsed.saturating_add(
                running
                    .since
                    .map(|since| self.clock.now().saturating_sub(since))
                    .unwrap_or_default(),
            );
            if matches!(running.job.state, JobState::Printing | JobState::Paused) {
                let seconds = running.job.estimate.seconds.unwrap_or(1).max(1);
                running.job.progress =
                    Some((elapsed.as_secs_f64() / seconds as f64).clamp(0.0, 1.0));
                running.job.seconds_left = Some(seconds.saturating_sub(elapsed.as_secs()));
                if elapsed >= Duration::from_secs(seconds) {
                    running.job.state = JobState::Done;
                    running.since = None;
                }
            }
        }
        Ok(jobs)
    }

    fn change(&self, printer: &str, pause: bool) -> Result<Job, Error> {
        if !self.printers.iter().any(|p| p.id == printer) {
            return Err(Error::NotFound);
        }
        let mut jobs = self.jobs()?;
        let running = jobs
            .values_mut()
            .find(|r| {
                r.job.printer == printer
                    && matches!(r.job.state, JobState::Printing | JobState::Paused)
            })
            .ok_or_else(|| Error::Refused("The printer has no active job.".into()))?;
        let expected = if pause {
            JobState::Printing
        } else {
            JobState::Paused
        };
        if running.job.state != expected {
            return Err(Error::Refused("The job is already in that state.".into()));
        }
        if pause {
            if let Some(since) = running.since.take() {
                running.elapsed = running
                    .elapsed
                    .saturating_add(self.clock.now().saturating_sub(since));
            }
            running.job.state = JobState::Paused;
        } else {
            running.since = Some(self.clock.now());
            running.job.state = JobState::Printing;
        }
        Ok(running.job.clone())
    }
}

impl Backend for Simulated {
    fn printers(&self) -> Reply<'_, Vec<Printer>> {
        Box::pin(async {
            let jobs = self.jobs()?;
            Ok(self
                .printers
                .iter()
                .map(|p| {
                    let active = jobs
                        .values()
                        .find(|r| {
                            r.job.printer == p.id
                                && matches!(r.job.state, JobState::Printing | JobState::Paused)
                        })
                        .map(|r| &r.job);
                    Printer {
                        detail: None,
                        needs_clearing: None,
                        id: p.id.clone(),
                        name: p.name.clone(),
                        model: p.model.clone(),
                        state: match active.map(|j| j.state) {
                            Some(JobState::Printing) => PrinterState::Printing,
                            Some(JobState::Paused) => PrinterState::Paused,
                            _ => PrinterState::Idle,
                        },
                        job_id: active.map(|j| j.id.clone()),
                        job_name: active.map(|j| j.name.clone()),
                        progress: active.and_then(|j| j.progress),
                        seconds_left: active.and_then(|j| j.seconds_left),
                        nozzle_temperature: active.map(|_| 210.0),
                        bed_temperature: active.map(|_| 60.0),
                    }
                })
                .collect())
        })
    }

    fn prepare<'a>(
        &'a self,
        job_id: &'a str,
        input: &'a Path,
        printer: &'a str,
        options: Options,
    ) -> Reply<'a, Estimate> {
        Box::pin(async move {
            options.validate().map_err(Error::Refused)?;
            let profile = self
                .printers
                .iter()
                .find(|p| p.id == printer)
                .ok_or(Error::NotFound)?;
            let estimate = if let Some(slicer) = &self.slicer {
                slicer
                    .prepare(input, profile, &options, crate::slicer::OutputKind::Gcode)
                    .await
                    .map_err(Error::Failed)?
                    .estimate
            } else {
                let size = tokio::fs::metadata(input)
                    .await
                    .map_err(|_| Error::Failed("Cannot read the 3MF file.".into()))?
                    .len();
                Estimate {
                    seconds: Some(60 + size / 1024),
                    filament_mm: Some(size as f64 / 10.0),
                    filament_g: Some(size as f64 / 1000.0),
                    layers: Some(1 + size / 4096),
                    preview: None,
                }
            };
            let mut jobs = self.jobs()?;
            if jobs.contains_key(job_id) {
                return Err(Error::Refused("This job id is already in use.".into()));
            }
            if jobs.len() >= 5000 {
                return Err(Error::Refused(
                    "Too many saved jobs. Wait for jobs to expire.".into(),
                ));
            }
            let job = Job {
                id: job_id.into(),
                printer: printer.into(),
                name: options.name.unwrap_or_else(|| {
                    input
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                }),
                state: JobState::Ready,
                estimate: estimate.clone(),
                progress: Some(0.0),
                seconds_left: estimate.seconds,
            };
            jobs.insert(
                job_id.into(),
                Running {
                    job,
                    created: self.clock.now(),
                    elapsed: Duration::ZERO,
                    since: None,
                },
            );
            Ok(estimate)
        })
    }

    fn start<'a>(&'a self, job: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut jobs = self.jobs()?;
            let target = jobs.get(job).ok_or(Error::NotFound)?;
            if target.job.state != JobState::Ready {
                return Err(Error::Refused("Only a ready job can be started.".into()));
            }
            if jobs.values().any(|r| {
                r.job.printer == target.job.printer
                    && matches!(r.job.state, JobState::Printing | JobState::Paused)
            }) {
                return Err(Error::Busy);
            }
            let target = jobs.get_mut(job).ok_or(Error::NotFound)?;
            target.since = Some(self.clock.now());
            target.job.state = JobState::Printing;
            Ok(target.job.clone())
        })
    }

    fn cancel<'a>(&'a self, job: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            let mut jobs = self.jobs()?;
            let target = jobs.get_mut(job).ok_or(Error::NotFound)?;
            if !matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            ) {
                return Err(Error::Refused("This job has already ended.".into()));
            }
            target.job.state = JobState::Cancelled;
            target.since = None;
            target.job.seconds_left = None;
            Ok(target.job.clone())
        })
    }
    fn pause<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move { self.change(printer, true) })
    }
    fn resume<'a>(&'a self, printer: &'a str) -> Reply<'a, Job> {
        Box::pin(async move { self.change(printer, false) })
    }
    fn job<'a>(&'a self, job_id: &'a str) -> Reply<'a, Job> {
        Box::pin(async move {
            self.jobs()?
                .get(job_id)
                .map(|r| r.job.clone())
                .ok_or(Error::NotFound)
        })
    }
    fn forget<'a>(&'a self, job_id: &'a str) -> Reply<'a, ()> {
        Box::pin(async move {
            self.jobs()?.remove(job_id);
            Ok(())
        })
    }
}
