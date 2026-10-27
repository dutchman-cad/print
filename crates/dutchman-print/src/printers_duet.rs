//! Duet jobs: one staged file per preparation, started by name and tracked by that file.
use super::*;
use crate::backend::PrinterState;
use crate::duet::staged_path;

impl Printers {
    pub(super) async fn reconcile_duet(
        &self,
        target: &mut Prepared,
    ) -> Result<crate::duet::Snapshot, Error> {
        let client = self.duet.get(&target.job.printer).ok_or(Error::NotFound)?;
        let live = client.snapshot().await?;
        let ours = staged_path(&target.filename);
        if target.attempted
            && matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            let live_file = live.file.as_deref() == Some(ours.as_str());
            match live.printer.state {
                PrinterState::Printing | PrinterState::Paused if live_file => {
                    target.remote = Some(ours);
                    target.job.state = if live.printer.state == PrinterState::Paused {
                        JobState::Paused
                    } else {
                        JobState::Printing
                    };
                    target.job.progress = live.printer.progress;
                    target.job.seconds_left = live.printer.seconds_left;
                }
                // An emergency stop ends a job this companion saw running.
                PrinterState::Error
                    if live.status.as_deref() == Some("halted")
                        && (live_file || target.remote.is_some()) =>
                {
                    target.job.state = JobState::Failed;
                    target.job.seconds_left = None;
                }
                // Standalone firmware does not report how a file ended. Idle after observed
                // activity, or with this unique file as the last one processed, ends the job.
                PrinterState::Idle
                    if live.status.as_deref() == Some("idle")
                        && (target.remote.is_some()
                            || live.last_file.as_deref() == Some(ours.as_str()) && !live_file) =>
                {
                    target.job.state = if target.cancelling {
                        JobState::Cancelled
                    } else {
                        JobState::Ended
                    };
                    target.job.seconds_left = None;
                }
                _ => {}
            }
        }
        Ok(live)
    }
    async fn duet_send(
        &self,
        target: &mut Prepared,
        client: &crate::duet::Client,
        code: &str,
    ) -> Result<(), Error> {
        if let Err(error) = client.gcode(code).await {
            let _ = self.reconcile_duet(target).await;
            return Err(if matches!(error, Error::Unavailable) {
                refuse("Duet command reply was lost. Read fresh status to reconcile the job; the command will not be repeated.")
            } else {
                error
            });
        }
        Ok(())
    }
    pub(super) async fn duet_control(
        &self,
        target: &mut Prepared,
        action: &str,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        let client = self
            .duet
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?
            .clone();
        let live = self.reconcile_duet(target).await?;
        let ours = staged_path(&target.filename);
        if !target.attempted
            || !matches!(target.job.state, JobState::Printing | JobState::Paused)
            || target.remote.as_deref() != Some(ours.as_str())
            || live.file.as_deref() != Some(ours.as_str())
            || !matches!(
                (action, live.status.as_deref()),
                ("pause", Some("processing"))
                    | ("resume", Some("paused"))
                    | ("cancel", Some("processing" | "paused" | "pausing"))
            )
        {
            return Err(refuse("No matching live job or readiness is unknown. Read fresh status to reconcile the printer."));
        }
        self.validate_control(target).await?;
        if action != "cancel" {
            self.duet_send(
                target,
                &client,
                if action == "pause" { "M25" } else { "M24" },
            )
            .await?;
            self.poll_boundary(target, guard).await?;
            self.wait_duet(target, action).await?;
            return Ok(target.job.clone());
        }
        self.record_cancel(target).await?;
        // M0 cancels only a paused file job, so a running job is paused first.
        if live.status.as_deref() != Some("paused") {
            if live.status.as_deref() == Some("processing") {
                self.duet_send(target, &client, "M25").await?;
            }
            self.publish(target).await?;
            self.wait_duet(target, "pause").await?;
            if target.job.state == JobState::Cancelled {
                return Ok(target.job.clone());
            }
        }
        self.duet_send(target, &client, "M0").await?;
        self.publish(target).await?;
        self.wait_duet(target, "cancel").await?;
        Ok(target.job.clone())
    }
    pub(super) async fn wait_duet(&self, target: &mut Prepared, action: &str) -> Result<(), Error> {
        use std::time::Duration;
        use tokio::time::{sleep, timeout_at, Instant};
        let deadline = Instant::now()
            + Duration::from_secs(if matches!(action, "start" | "cancel") {
                25
            } else {
                10
            });
        let ours = staged_path(&target.filename);
        let mut pending = false;
        loop {
            if let Ok(Ok(live)) = timeout_at(deadline, self.reconcile_duet(target)).await {
                self.publish(target).await?;
                if target.job.state == JobState::Cancelled {
                    return Ok(());
                }
                let live_file = live.file.as_deref() == Some(ours.as_str());
                let confirmed = match action {
                    "pause" => {
                        target.job.state == JobState::Paused
                            && live_file
                            && live.status.as_deref() == Some("paused")
                    }
                    "resume" => {
                        target.job.state == JobState::Printing
                            && live_file
                            && live.status.as_deref() == Some("processing")
                    }
                    "start" => {
                        (target.job.state == JobState::Printing && live_file)
                            || matches!(target.job.state, JobState::Ended | JobState::Failed)
                    }
                    _ => false,
                };
                if confirmed {
                    return Ok(());
                }
                if matches!(target.job.state, JobState::Ended | JobState::Failed) {
                    return Err(refuse(
                        "The job ended while the command was pending. Read fresh status.",
                    ));
                }
                if live.file.is_some() && !live_file {
                    return Err(refuse(
                        "Live job changed while the command was pending. Read fresh status.",
                    ));
                }
            }
            if !pending {
                eprintln!(
                    "Duet {action} is pending. Waiting for fresh status; do not repeat the command."
                );
                pending = true;
            }
            if Instant::now() >= deadline {
                return Err(refuse("Duet command is not confirmed after waiting. Read fresh status to reconcile the pending outcome; do not repeat the command."));
            }
            sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    pub(super) async fn start_duet(
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
            .duet
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?
            .clone();
        crate::duet::ready(&client.snapshot().await?)?;
        let bytes = file_bytes(&target.path).await?;
        let info = client.review().await?;
        check_nozzle(&bytes, &info)?;
        if info != target.info {
            return Err(refuse("Machine identity, firmware, build volume, or tooling changed. Prepare and review again."));
        }
        if hash(&bytes) != target.digest {
            return Err(refuse("Prepared G-code changed. Prepare and review again."));
        }
        let filename = target.filename.clone();
        if client.stored(&filename).await?.is_some() {
            return Err(refuse(
                "Staging file already exists. Prepare a new job after inspecting storage.",
            ));
        }
        crate::duet::ready(&client.snapshot().await?)?;
        let staged = async {
            client.upload(&filename, &bytes).await?;
            if client.stored(&filename).await? != Some(bytes.len() as u64) {
                return Err(refuse(
                    "Uploaded file differs from the reviewed name or size. Prepare again.",
                ));
            }
            Ok(())
        }
        .await;
        if let Err(error) = staged {
            if client.delete(&filename).await.is_err() {
                return Err(refuse("Upload failed and remote cleanup is not confirmed. Inspect Duet storage and remove the staged file before preparing again."));
            }
            return Err(error);
        }
        target.staged = Some(filename.clone());
        self.publish(target).await?;
        crate::duet::ready(&client.snapshot().await?)?;
        if client.review().await? != target.info {
            return Err(refuse(
                "Machine configuration changed during upload. Prepare and review again.",
            ));
        }
        self.attempted(target).await?;
        let reply = client
            .gcode(&format!("M32 \"{}\"", staged_path(&filename)))
            .await;
        if let Err(error) = reply {
            let _ = self.reconcile_duet(target).await;
            return Err(if matches!(error, Error::Unavailable) {
                refuse("Start reply was lost. Read status to reconcile the intended file and job. Start will not be repeated.")
            } else {
                error
            });
        }
        self.poll_boundary(target, guard).await?;
        self.wait_duet(target, "start").await?;
        Ok(target.job.clone())
    }
    /// Stops an external live file: pause, confirm, then M0, each sent once.
    pub(super) async fn stop_duet(
        &self,
        client: &crate::duet::Client,
        identity: &str,
        name: &str,
    ) -> Result<(), Error> {
        use std::time::Duration;
        use tokio::time::{sleep, timeout_at, Instant};
        let fresh = client.snapshot().await?;
        if fresh.file.as_deref() != Some(identity)
            || fresh.printer.job_name.as_deref() != Some(name)
            || !matches!(
                fresh.status.as_deref(),
                Some("processing" | "paused" | "pausing")
            )
        {
            return Err(refuse("The live job changed. Confirm Stop again."));
        }
        if fresh.status.as_deref() != Some("paused") {
            if fresh.status.as_deref() == Some("processing") {
                client.gcode("M25").await?;
            }
            eprintln!("Pause before Stop is pending. Waiting for fresh status; do not repeat the command.");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(Ok(live)) = timeout_at(deadline, client.snapshot()).await {
                    if live.file.as_deref() != Some(identity) {
                        return Err(refuse(
                            "Live job changed while Stop was pending. Read fresh status.",
                        ));
                    }
                    if live.status.as_deref() == Some("paused") {
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    return Err(refuse("Pause before Stop is not confirmed. Stop was not sent; read fresh status before another action."));
                }
                sleep(
                    Duration::from_millis(250)
                        .min(deadline.saturating_duration_since(Instant::now())),
                )
                .await;
            }
        }
        client.gcode("M0").await?;
        eprintln!("Stop is pending. Waiting for fresh status; do not repeat the command.");
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if let Ok(Ok(live)) = timeout_at(deadline, client.snapshot()).await {
                if live.status.as_deref() == Some("idle")
                    && live.last_file.as_deref() == Some(identity)
                {
                    return Ok(());
                }
                if live.file.as_deref().is_some_and(|f| f != identity) {
                    return Err(refuse(
                        "Live job changed while Stop was pending. Read fresh status.",
                    ));
                }
            }
            if Instant::now() >= deadline {
                return Err(refuse("Stop is not confirmed after waiting. Read fresh status; do not repeat the command."));
            }
            sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
}
