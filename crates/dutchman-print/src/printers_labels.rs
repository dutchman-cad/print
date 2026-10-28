//! Label jobs retain the shared durable attempt boundary. TSPL has no job identity.
use super::*;
use crate::backend::PrinterState;
use crate::labels;
use tokio::io::AsyncWriteExt as _;

const SENT: &str = "This label has been sent. Cancel it with the printer's own cancel or feed button: TSPL's cancel command would clear every job on the printer, not just this one.";

fn stock(p: &crate::config::Printer) -> Value {
    serde_json::json!({"language":p.language,"width_mm":p.width_mm,"height_mm":p.height_mm,"gap_mm":p.gap_mm,"dpi":p.dpi,"host":p.host,"port":p.port.unwrap_or(9100)})
}

impl Printers {
    pub(super) async fn prepare_label(
        &self,
        id: &str,
        path: &Path,
        printer: &str,
        options: Options,
    ) -> Result<Estimate, Error> {
        options.validate().map_err(Error::Refused)?;
        if options.sides.is_some()
            || options.media.is_some()
            || options.printer_profile.is_some()
            || options.print_profile.is_some()
            || options.filament_profile.is_some()
        {
            return Err(refuse("Label printers use PNG and configured stock. Leave out sides, media, and slicer profiles."));
        }
        let client = self.labels.get(printer).ok_or(Error::NotFound)?;
        let bytes = file_bytes(path).await?;
        labels::job(client.config(), &bytes, options.copies.unwrap_or(1))?;
        labels::ready(client.status().await?)?;
        let jobs = self.jobs.lock().await;
        if jobs.values().any(|p| {
            p.job.printer == printer
                && p.attempted
                && matches!(
                    p.job.state,
                    JobState::Ready | JobState::Printing | JobState::Paused
                )
        }) {
            return Err(refuse("A label submission is still unresolved. Read status and inspect the printer before preparing more work."));
        }
        drop(jobs);
        let name = options.document_name.as_deref().unwrap_or("input.png");
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            return Err(refuse("Use a plain PNG filename of at most 256 bytes."));
        }
        let estimate = Estimate::default();
        let target = Prepared {
            job: Job {
                detail: None,
                id: id.into(),
                printer: printer.into(),
                name: options.name.clone().unwrap_or_else(|| name.into()),
                state: JobState::Ready,
                estimate: estimate.clone(),
                progress: None,
                seconds_left: None,
            },
            path: path.into(),
            digest: hash(&bytes),
            info: serde_json::json!({"copies":options.copies.unwrap_or(1),"document_name":name}),
            remote: None,
            attempted: false,
            filename: id.into(),
            profiles: stock(client.config()),
            history: Value::Null,
            staged: None,
            cancelling: false,
            revision: 0,
            incarnation: Arc::new(()),
        };
        let mut jobs = self.jobs.lock().await;
        if jobs.len() >= 5000 || jobs.contains_key(id) {
            return Err(refuse("Job storage is full or this id is already in use."));
        }
        jobs.insert(id.into(), target);
        self.checkpoint(&jobs).await?;
        Ok(estimate)
    }
    pub(super) async fn reconcile_label(&self, target: &mut Prepared) -> Result<(), Error> {
        if !target.attempted
            || !matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            return Ok(());
        }
        let code = self
            .labels
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?
            .status()
            .await?;
        // Once transport failed, no later unrelated activity can establish this job's outcome.
        if target.history == Value::String("uncertain".into()) {
            return Ok(());
        }
        match labels::map_status(code).0 {
            PrinterState::Printing => {
                target.remote = Some("printing-observed".into());
                target.job.state = JobState::Printing;
            }
            PrinterState::Paused if target.remote.is_some() => target.job.state = JobState::Paused,
            PrinterState::Idle if target.remote.is_some() => {
                target.job.state = if target.cancelling {
                    JobState::Cancelled
                } else {
                    JobState::Done
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) async fn review_label(
        &self,
        action: &str,
        printer: &str,
        job: Option<&str>,
    ) -> Result<(), Error> {
        let target = self.tracked(job.ok_or(Error::NotFound)?).await?;
        if target.job.printer != printer {
            return Err(Error::NotFound);
        }
        let client = self.labels.get(printer).ok_or(Error::NotFound)?;
        let code = client.status().await?;
        if action == "start" {
            self.label_startable(&target).await?;
            labels::ready(code)?;
            self.label_bytes(&target).await?;
        } else if target.attempted {
            return Err(refuse(SENT));
        }
        Ok(())
    }
    async fn label_startable(&self, target: &Prepared) -> Result<(), Error> {
        if target.attempted || target.job.state != JobState::Ready {
            return Err(refuse(
                "Start was already attempted. Read status; Start will not be repeated.",
            ));
        }
        if self.jobs.lock().await.values().any(|p| {
            p.job.id != target.job.id
                && p.job.printer == target.job.printer
                && p.attempted
                && matches!(
                    p.job.state,
                    JobState::Ready | JobState::Printing | JobState::Paused
                )
        }) {
            return Err(refuse("Another label submission remains active or uncertain. Inspect the printer before starting."));
        }
        Ok(())
    }
    async fn label_bytes(&self, target: &Prepared) -> Result<Vec<u8>, Error> {
        let config = self
            .labels
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?
            .config();
        if stock(config) != target.profiles {
            return Err(refuse(
                "Label stock, resolution, or connection changed. Prepare and review again.",
            ));
        }
        let bytes = file_bytes(&target.path).await?;
        if hash(&bytes) != target.digest {
            return Err(refuse("Prepared PNG changed. Prepare and review again."));
        }
        let copies = target
            .info
            .get("copies")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(Error::Unavailable)?;
        labels::job(
            self.labels
                .get(&target.job.printer)
                .ok_or(Error::NotFound)?
                .config(),
            &bytes,
            copies,
        )
    }
    pub(super) async fn start_label(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        self.label_startable(target).await?;
        let client = self
            .labels
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let bytes = self.label_bytes(target).await?;
        let mut stream = client.connect().await?;
        labels::ready(labels::Client::status_on(&mut stream).await?)?;
        target.history = Value::String("uncertain".into());
        target.job.detail = Some(
            "Label outcome is uncertain. Inspect the printer; Start will not be repeated.".into(),
        );
        self.attempted(target).await?;
        // No protocol acknowledgement exists. Even a successful write needs observed activity.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(30), stream.write_all(&bytes))
                .await;
        if !matches!(result, Ok(Ok(()))) {
            target.history = Value::String("uncertain".into());
            return Err(refuse("Label transfer was interrupted; the outcome is uncertain. Inspect the printer. Start will not be repeated."));
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
        loop {
            match labels::Client::status_on(&mut stream).await {
                Ok(code) => {
                    match labels::map_status(code).0 {
                        PrinterState::Printing => {
                            target.history = Value::Null;
                            target.job.detail = None;
                            target.remote = Some("printing-observed".into());
                            target.job.state = JobState::Printing;
                        }
                        PrinterState::Idle if target.remote.is_some() => {
                            target.job.state = JobState::Done
                        }
                        _ => {}
                    }
                    if target.remote.is_some() {
                        return Ok(target.job.clone());
                    }
                }
                Err(_) => {
                    target.history = Value::String("uncertain".into());
                    return Err(refuse("Label status connection was lost; the outcome is uncertain. Inspect the printer. Start will not be repeated."));
                }
            }
            self.poll_boundary(target, guard).await?;
            if tokio::time::Instant::now() >= deadline {
                return Err(refuse("Label Start is unconfirmed: printing was not observed. Inspect the printer. Start will not be repeated."));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    /// TSPL's only cancel command clears every job on the printer, whoever sent it, so a sent
    /// label is never cancelled from here.
    pub(super) async fn cancel_label(
        &self,
        _target: &mut Prepared,
        _guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        Err(refuse(SENT))
    }
}
