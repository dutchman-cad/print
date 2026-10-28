//! PDF jobs use the shared durable start boundary and per-printer command guards.
use super::*;
use crate::ipp::{self, Group};

fn options(target: &Prepared) -> Result<Options, Error> {
    serde_json::from_value(target.info.clone()).map_err(|_| Error::Unavailable)
}
fn job_id(group: &Group) -> Result<i32, Error> {
    group
        .get("job-id")
        .ok_or(Error::Unavailable)?
        .number(0x21)
        .and_then(|id| {
            if id > 0 {
                Ok(id)
            } else {
                Err(Error::Unavailable)
            }
        })
}
fn job_name(group: &Group) -> Result<&str, Error> {
    let a = group.get("job-name").ok_or(Error::Unavailable)?;
    let names = a.texts(0x42)?;
    match names.as_slice() {
        [name] => Ok(name),
        _ => Err(Error::Unavailable),
    }
}
impl Printers {
    pub(super) async fn prepare_ipp(
        &self,
        id: &str,
        path: &Path,
        printer: &str,
        options: Options,
    ) -> Result<Estimate, Error> {
        options.validate().map_err(Error::Refused)?;
        if options.printer_profile.is_some()
            || options.print_profile.is_some()
            || options.filament_profile.is_some()
        {
            return Err(refuse(
                "IPP prints PDF documents directly. Leave out slicer profiles.",
            ));
        }
        let bytes = file_bytes(path).await?;
        ipp::check_pdf(&bytes)?;
        let client = self.ipp.get(printer).ok_or(Error::NotFound)?;
        let snapshot = client.snapshot().await?;
        client.check_options(&snapshot.attributes, &options)?;
        let mut info = serde_json::to_value(&options).map_err(|_| Error::Unavailable)?;
        let document_name = options.document_name.as_deref().unwrap_or("input.pdf");
        if document_name.is_empty()
            || document_name.len() > 256
            || document_name.chars().any(char::is_control)
        {
            return Err(refuse("Use a plain PDF filename of at most 256 bytes."));
        }
        info.as_object_mut()
            .ok_or(Error::Unavailable)?
            .insert("document_name".into(), Value::String(document_name.into()));
        // Make the default explicit: the reviewed copy count cannot depend on a printer default.
        info.as_object_mut()
            .ok_or(Error::Unavailable)?
            .insert("copies".into(), Value::from(options.copies.unwrap_or(1)));
        let effective: Options =
            serde_json::from_value(info.clone()).map_err(|_| Error::Unavailable)?;
        client.check_options(&snapshot.attributes, &effective)?;
        let estimate = Estimate::default();
        let target = Prepared {
            job: Job {
                detail: None,
                id: id.into(),
                printer: printer.into(),
                name: options.name.unwrap_or_else(|| document_name.into()),
                state: JobState::Ready,
                estimate: estimate.clone(),
                progress: None,
                seconds_left: None,
            },
            path: path.into(),
            digest: hash(&bytes),
            info,
            remote: None,
            attempted: false,
            filename: format!("dutchman-{}", hash(id.as_bytes())),
            profiles: self.profiles(printer)?,
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
    pub(super) async fn reconcile_ipp(&self, target: &mut Prepared) -> Result<(), Error> {
        if !target.attempted
            || !matches!(
                target.job.state,
                JobState::Ready | JobState::Printing | JobState::Paused
            )
        {
            return Ok(());
        }
        let client = self.ipp.get(&target.job.printer).ok_or(Error::NotFound)?;
        let group = if let Some(remote) = &target.remote {
            let id = remote.parse::<i32>().map_err(|_| Error::Unavailable)?;
            let group = client.job(id).await?;
            if job_id(&group)? != id {
                return Err(refuse(
                    "The IPP job identity changed. Read fresh status before another action.",
                ));
            }
            group
        } else {
            let jobs = client.jobs().await?;
            let mut ours = jobs
                .into_iter()
                .filter(|g| job_name(g).ok() == Some(target.filename.as_str()));
            match (ours.next(), ours.next()) {
                (Some(job), None) => job,
                (None, _) => return Ok(()),
                _ => return Err(refuse("Multiple IPP jobs match this submission. Inspect the printer job list; Start will not be repeated.")),
            }
        };
        if job_name(&group)? != target.filename {
            return Err(refuse(
                "The IPP job name differs from the tracked submission. No command was sent.",
            ));
        }
        let id = job_id(&group)?;
        let state = group
            .get("job-state")
            .ok_or(Error::Unavailable)?
            .number(0x23)?;
        let outcome = match state {
            3..=6 => JobState::Printing,
            7 => JobState::Cancelled,
            8 => JobState::Failed,
            9 => JobState::Done,
            _ => {
                return Err(refuse(
                    "IPP job state is unknown. Read fresh status; do not repeat Start.",
                ))
            }
        };
        target.remote = Some(id.to_string());
        target.job.state = outcome;
        Ok(())
    }
    pub(super) async fn review_ipp(
        &self,
        action: &str,
        printer: &str,
        job: Option<&str>,
    ) -> Result<(), Error> {
        let mut target = self.tracked(job.ok_or(Error::NotFound)?).await?;
        if target.job.printer != printer {
            return Err(Error::NotFound);
        }
        let client = self.ipp.get(printer).ok_or(Error::NotFound)?;
        if action == "start" {
            if target.attempted || target.job.state != JobState::Ready {
                return Err(refuse(
                    "Start was already attempted. Read status; do not repeat Start.",
                ));
            }
            let snapshot = client.snapshot().await?;
            ipp::ready(&snapshot)?;
            client.check_options(&snapshot.attributes, &options(&target)?)?;
            let bytes = file_bytes(&target.path).await?;
            ipp::check_pdf(&bytes)?;
            if hash(&bytes) != target.digest {
                return Err(refuse("Prepared PDF changed. Prepare and review again."));
            }
        } else if target.attempted {
            self.reconcile_ipp(&mut target).await?;
            if target.remote.is_none() || target.job.state != JobState::Printing {
                return Err(refuse(
                    "No matching active IPP job. Read fresh status to reconcile the submission.",
                ));
            }
        }
        Ok(())
    }
    pub(super) async fn start_ipp(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        if target.attempted || target.job.state != JobState::Ready {
            return Err(refuse(
                "Start was already attempted. Read status; Start will not be repeated.",
            ));
        }
        let client = self.ipp.get(&target.job.printer).ok_or(Error::NotFound)?;
        let snapshot = client.snapshot().await?;
        ipp::ready(&snapshot)?;
        let options = options(target)?;
        let template = client.template(&snapshot.attributes, &options)?;
        let bytes = file_bytes(&target.path).await?;
        ipp::check_pdf(&bytes)?;
        if hash(&bytes) != target.digest {
            return Err(refuse("Prepared PDF changed. Prepare and review again."));
        }
        if client
            .jobs()
            .await?
            .iter()
            .any(|g| job_name(g).ok() == Some(target.filename.as_str()))
        {
            return Err(refuse("The IPP submission name already exists. Inspect the printer job list and prepare a new job."));
        }
        client
            .submit(4, &target.filename, template.clone(), &[])
            .await?;
        let snapshot = client.snapshot().await?;
        ipp::ready(&snapshot)?;
        if client.template(&snapshot.attributes, &options)? != template {
            return Err(refuse(
                "IPP document settings changed. Prepare and review again.",
            ));
        }
        self.attempted(target).await?;
        let reply = client.submit(2, &target.filename, template, &bytes).await;
        if let Ok(reply) = &reply {
            let mut groups = reply.groups.iter().filter(|g| g.tag == 2);
            if let (Some(group), None) = (groups.next(), groups.next()) {
                if let Ok(id) = job_id(group) {
                    target.remote = Some(id.to_string());
                }
            }
        }
        self.poll_boundary(target, guard).await?;
        if reply.is_err() {
            // Reconciliation is read-only even when the connection failed after submission.
            let _ = self.reconcile_ipp(target).await;
            self.publish(target).await?;
            return Err(refuse("Print-Job reply was lost or refused. Read status to reconcile the submission. Start will not be repeated."));
        }
        self.wait_ipp(target, false).await?;
        Ok(target.job.clone())
    }
    pub(super) async fn cancel_ipp(
        &self,
        target: &mut Prepared,
        guard: &mut MachineGuard,
    ) -> Result<Job, Error> {
        self.reconcile_ipp(target).await?;
        if target.job.state != JobState::Printing || target.cancelling {
            return Err(refuse("No matching active IPP job, or cancellation is already pending. Read fresh status."));
        }
        let id = target
            .remote
            .as_deref()
            .and_then(|s| s.parse::<i32>().ok())
            .ok_or_else(|| {
                refuse("IPP submission is unresolved. Read status; do not repeat Start.")
            })?;
        let client = self.ipp.get(&target.job.printer).ok_or(Error::NotFound)?;
        self.validate_control(target).await?;
        self.record_cancel(target).await?;
        let reply = client.cancel(id).await;
        self.poll_boundary(target, guard).await?;
        if reply.is_err() {
            let _ = self.reconcile_ipp(target).await;
            self.publish(target).await?;
            return Err(refuse("Cancel-Job reply was lost or refused. Read status to reconcile the job; the command will not be repeated."));
        }
        self.wait_ipp(target, true).await?;
        Ok(target.job.clone())
    }
    async fn wait_ipp(&self, target: &mut Prepared, cancel: bool) -> Result<(), Error> {
        use tokio::time::{sleep_until, timeout_at, Instant};
        let deadline = Instant::now() + std::time::Duration::from_secs(25);
        loop {
            if let Ok(Ok(())) = timeout_at(deadline, self.reconcile_ipp(target)).await {
                self.publish(target).await?;
                if target.remote.is_some()
                    && ((!cancel && target.job.state != JobState::Ready)
                        || (cancel
                            && !matches!(target.job.state, JobState::Ready | JobState::Printing)))
                {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(refuse("IPP command is not confirmed after waiting. Read fresh status; do not repeat the command."));
            }
            sleep_until((Instant::now() + std::time::Duration::from_millis(250)).min(deadline))
                .await;
        }
    }
}
