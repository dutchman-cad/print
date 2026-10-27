//! Repetier-Server jobs: one staged model per preparation, queued and started by id, and
//! tracked by its unique name.
use super::*;
use crate::backend::PrinterState;
use crate::repetier::named;

impl Printers {
    pub(super) async fn reconcile_repetier(
        &self,
        target: &mut Prepared,
    ) -> Result<crate::repetier::Snapshot, Error> {
        let client = self
            .repetier
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?;
        let live = client.snapshot().await?;
        if target.attempted
            && matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            let ours = live
                .job
                .as_deref()
                .is_some_and(|j| named(j, &target.filename));
            match live.printer.state {
                PrinterState::Printing if ours => {
                    target.remote = live
                        .printer
                        .job_id
                        .clone()
                        .or_else(|| {
                            target
                                .history
                                .get("job")
                                .and_then(Value::as_u64)
                                .map(|id| id.to_string())
                        })
                        .or_else(|| live.job.clone());
                    target.job.state = JobState::Printing;
                }
                // The REST interface does not report how a job ended: finish and kill events
                // reach open websockets only. A job seen running that is no longer live ended.
                PrinterState::Idle | PrinterState::Printing if !ours && target.remote.is_some() => {
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
    pub(super) async fn repetier_control(
        &self,
        target: &mut Prepared,
        action: &str,
    ) -> Result<Job, Error> {
        match action {
            "pause" => return Err(refuse(crate::repetier::PAUSE)),
            "resume" => return Err(refuse(crate::repetier::RESUME)),
            _ => {}
        }
        let client = self
            .repetier
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?
            .clone();
        let live = self.reconcile_repetier(target).await?;
        let ours = live
            .job
            .as_deref()
            .is_some_and(|j| named(j, &target.filename));
        if target.attempted
            && target.job.state == JobState::Printing
            && target.remote.is_some()
            && ours
            && live.printer.state == PrinterState::Printing
        {
            self.validate_control(target).await?;
            self.record_cancel(target).await?;
            // stopJob names no job; the live job was just checked to be this one.
            if let Err(error) = client.command("stopJob", serde_json::json!({}), true).await {
                let _ = self.reconcile_repetier(target).await;
                return Err(if matches!(error, Error::Unavailable) {
                    refuse("Cancel reply was lost. Read fresh status to reconcile the job; the command will not be repeated.")
                } else {
                    error
                });
            }
            self.publish(target).await?;
            self.wait_repetier(target, "cancel").await?;
            return Ok(target.job.clone());
        }
        // A start that never ran leaves its job in the queue; removing it moves nothing.
        let queued = target.history.get("job").and_then(Value::as_u64);
        if let Some(id) = queued.filter(|id| {
            target.attempted
                && target.job.state == JobState::Ready
                && target.remote.is_none()
                && live.printer.state == PrinterState::Idle
                && live.job.is_none()
                && live
                    .jobs
                    .iter()
                    .any(|j| j.id == *id && named(&j.name, &target.filename))
        }) {
            self.validate_control(target).await?;
            self.record_cancel(target).await?;
            let removed = client
                .command("removeJob", serde_json::json!({"id":id}), true)
                .await;
            let after = self.reconcile_repetier(target).await?;
            self.publish(target).await?;
            if target.job.state == JobState::Cancelled {
                return Ok(target.job.clone());
            }
            if after
                .job
                .as_deref()
                .is_some_and(|j| named(j, &target.filename))
            {
                return Err(refuse(
                    "The queued job started while its removal was pending. Read fresh status before another action.",
                ));
            }
            if removed.is_err() || after.jobs.iter().any(|j| j.id == id) {
                return Err(refuse("Removing the queued job is not confirmed. Read fresh status before another action."));
            }
            target.job.state = JobState::Cancelled;
            target.job.seconds_left = None;
            return Ok(target.job.clone());
        }
        Err(refuse(
            "No matching live job or readiness is unknown. Read fresh status to reconcile the printer.",
        ))
    }
    pub(super) async fn wait_repetier(
        &self,
        target: &mut Prepared,
        action: &str,
    ) -> Result<(), Error> {
        use std::time::Duration;
        use tokio::time::{sleep, timeout_at, Instant};
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut pending = false;
        loop {
            if let Ok(Ok(live)) = timeout_at(deadline, self.reconcile_repetier(target)).await {
                self.publish(target).await?;
                if target.job.state == JobState::Cancelled {
                    return Ok(());
                }
                let ours = live
                    .job
                    .as_deref()
                    .is_some_and(|j| named(j, &target.filename));
                if action == "start"
                    && ((target.job.state == JobState::Printing && ours)
                        || target.job.state == JobState::Ended)
                {
                    return Ok(());
                }
                if target.job.state == JobState::Ended {
                    return Err(refuse(
                        "The job ended while the command was pending. Read fresh status.",
                    ));
                }
                if live.job.is_some() && !ours {
                    return Err(refuse(
                        "Live job changed while the command was pending. Read fresh status.",
                    ));
                }
            }
            if !pending {
                eprintln!("Repetier-Server {action} is pending. Waiting for fresh status; do not repeat the command.");
                pending = true;
            }
            if Instant::now() >= deadline {
                return Err(refuse("Repetier-Server command is not confirmed after waiting. Read fresh status to reconcile the pending outcome; do not repeat the command."));
            }
            sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }
    pub(super) async fn start_repetier(
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
            .repetier
            .get(&target.job.printer)
            .ok_or(Error::NotFound)?
            .clone();
        let live = client.snapshot().await?;
        crate::repetier::ready(&live)?;
        let bytes = file_bytes(&target.path).await?;
        let info = client.review().await?;
        check_nozzle(&bytes, &info)?;
        if info != target.info {
            return Err(refuse("Server, machine configuration, build volume, or tooling changed. Prepare and review again."));
        }
        if hash(&bytes) != target.digest {
            return Err(refuse("Prepared G-code changed. Prepare and review again."));
        }
        let filename = target.filename.clone();
        if live.jobs.iter().any(|j| named(&j.name, &filename))
            || client
                .models()
                .await?
                .iter()
                .any(|m| named(&m.name, &filename))
        {
            return Err(refuse(
                "Staging model already exists. Prepare a new job after inspecting the Repetier-Server model list.",
            ));
        }
        client.can_upload(bytes.len()).await?;
        crate::repetier::ready(&client.snapshot().await?)?;
        let staged = async {
            client.upload(&filename, &bytes).await?;
            let models = client.models().await?;
            let mut stored = models.iter().filter(|m| named(&m.name, &filename));
            match (stored.next(), stored.next()) {
                (Some(model), None) if model.length == Some(bytes.len() as u64) => Ok(model.id),
                _ => Err(refuse(
                    "Uploaded model differs from the reviewed name or size. Prepare again.",
                )),
            }
        }
        .await;
        let model = match staged {
            Ok(model) => model,
            Err(error) => {
                if client.delete(&filename).await.is_err() {
                    return Err(refuse("Upload failed and remote cleanup is not confirmed. Inspect the Repetier-Server model list and remove the staged model before preparing again."));
                }
                return Err(error);
            }
        };
        target.staged = Some(filename.clone());
        target.history = serde_json::json!({"model":model});
        self.publish(target).await?;
        crate::repetier::ready(&client.snapshot().await?)?;
        if client.review().await? != target.info {
            return Err(refuse(
                "Machine configuration changed during upload. Prepare and review again.",
            ));
        }
        // copyModel takes a start flag, so queueing is already inside the start boundary:
        // it is sent once, with autostart false, after the attempt is saved.
        self.attempted(target).await?;
        let copied = client
            .command(
                "copyModel",
                serde_json::json!({"id":model,"autostart":false}),
                true,
            )
            .await;
        let queued = client.jobs().await.ok().and_then(|jobs| {
            let mut ours = jobs.into_iter().filter(|j| named(&j.name, &filename));
            match (ours.next(), ours.next()) {
                (Some(job), None) => Some(job.id),
                _ => None,
            }
        });
        let Some(queued) = queued else {
            let _ = self.reconcile_repetier(target).await;
            return Err(match copied {
                Err(error @ (Error::Authentication | Error::Refused(_))) => error,
                _ => refuse("Queueing the job is not confirmed. Read status to reconcile the intended file and job. Start will not be repeated."),
            });
        };
        target.history = serde_json::json!({"model":model,"job":queued});
        self.publish(target).await?;
        let live = self.reconcile_repetier(target).await?;
        if live.job.is_some() {
            // A server that started the queued job by itself is waited for, never restarted.
            self.poll_boundary(target, guard).await?;
            self.wait_repetier(target, "start").await?;
            return Ok(target.job.clone());
        }
        crate::repetier::ready(&live)?;
        let reply = client
            .command("startJob", serde_json::json!({"id":queued}), true)
            .await;
        if let Err(error) = reply {
            let _ = self.reconcile_repetier(target).await;
            return Err(if matches!(error, Error::Unavailable) {
                refuse("Start reply was lost. Read status to reconcile the intended file and job. Start will not be repeated.")
            } else {
                error
            });
        }
        self.poll_boundary(target, guard).await?;
        self.wait_repetier(target, "start").await?;
        Ok(target.job.clone())
    }
    /// Checks a challenge before it is issued; nothing is sent to the printer.
    pub(super) async fn review_repetier(
        &self,
        client: &crate::repetier::Client,
        action: &str,
        printer: &str,
        job: Option<&str>,
    ) -> Result<(), Error> {
        match action {
            "pause" => return Err(refuse(crate::repetier::PAUSE)),
            "resume" => return Err(refuse(crate::repetier::RESUME)),
            _ => {}
        }
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
            crate::repetier::ready(&live)?;
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
        } else if target.attempted {
            let running = target.remote.is_some()
                && live.printer.state == PrinterState::Printing
                && live
                    .job
                    .as_deref()
                    .is_some_and(|j| named(j, &target.filename));
            let queued = target.remote.is_none()
                && live.printer.state == PrinterState::Idle
                && live.job.is_none()
                && target
                    .history
                    .get("job")
                    .and_then(Value::as_u64)
                    .is_some_and(|id| live.jobs.iter().any(|j| j.id == id));
            if !running && !queued {
                return Err(refuse("The live job differs from the reviewed job or readiness is unknown. Read fresh status."));
            }
        }
        Ok(())
    }
    /// Stop is offered only for the live job this companion started, since stopJob cannot
    /// name a job. The tracked job is reconciled so Stop can find it by its live id.
    pub(super) async fn review_repetier_stop(
        &self,
        client: &crate::repetier::Client,
        printer: &str,
    ) -> Result<(), Error> {
        let live = client.snapshot().await?;
        let target = self
            .jobs
            .lock()
            .await
            .values()
            .find(|p| {
                p.job.printer == printer
                    && p.attempted
                    && matches!(
                        p.job.state,
                        JobState::Ready | JobState::Printing | JobState::Paused
                    )
                    && live.job.as_deref().is_some_and(|j| named(j, &p.filename))
            })
            .cloned();
        let Some(mut target) = target else {
            return Err(refuse(crate::repetier::STOP));
        };
        self.reconcile_repetier(&mut target).await?;
        self.publish(&mut target).await?;
        if target.remote.is_none() || target.remote != live.printer.job_id {
            return Err(refuse(crate::repetier::STOP));
        }
        Ok(())
    }
}
