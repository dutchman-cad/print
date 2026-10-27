//! Printer controls bound to an observed execution rather than a prepared job.
use super::*;
use crate::backend::PrinterState;

impl Printers {
    pub(super) async fn stop_live(
        &self,
        printer: &str,
        identity: &str,
        name: &str,
        job_path: Option<&str>,
    ) -> Result<Printer, Error> {
        self.check_action(printer, "stop")?;
        let mut guard = self.machine(printer).await?;
        let live = self.printer(printer).await?;
        if live.job_id.as_deref() != Some(identity)
            || live.job_name.as_deref() != Some(name)
            || live.job_path.as_deref() != job_path
            || !matches!(
                live.state,
                PrinterState::Printing | PrinterState::Paused | PrinterState::Attention
            )
        {
            return Err(refuse(
                "The live job changed. Read fresh status and confirm Stop again.",
            ));
        }
        let target = self
            .jobs
            .lock()
            .await
            .values()
            .find(|p| {
                p.job.printer == printer
                    && p.attempted
                    && (p.remote.as_deref() == Some(identity)
                        || p.remote.is_none()
                            && (p.filename == name
                                || p.filename.trim_end_matches(".gcode.3mf") == name))
                    && matches!(
                        p.job.state,
                        JobState::Ready | JobState::Printing | JobState::Paused
                    )
            })
            .cloned();
        if let Some(mut target) = target {
            if target.remote.is_none() {
                target.remote = Some(identity.into());
                target.job.state = if live.state == PrinterState::Paused {
                    JobState::Paused
                } else {
                    JobState::Printing
                };
                self.publish(&mut target).await?;
            }
            let result = self
                .dispatch_control(&mut target, "cancel", &mut guard)
                .await;
            self.publish(&mut target).await?;
            self.cleanup_staged(&mut target).await;
            self.publish(&mut target).await?;
            result?;
            return self.printer(printer).await;
        }
        if let Some(client) = self.bambu.get(printer) {
            client
                .command(
                    serde_json::json!({"print":{"command":"stop","param":""}}),
                    name,
                    "FAILED",
                    Some(identity),
                    &mut guard,
                )
                .await?;
            return self.printer(printer).await;
        }
        if let Some(client) = self.duet.get(printer) {
            self.stop_duet(client, identity, name).await?;
            return self.printer(printer).await;
        }
        if self.repetier.contains_key(printer) {
            return Err(refuse(crate::repetier::STOP));
        }
        if let Some(client) = self.sdcp.get(printer) {
            client.stop(identity, name).await?;
            return self.printer(printer).await;
        }
        if let Some(client) = self.snapmaker.get(printer) {
            return Err(client.refusal());
        }
        let mut baseline = None;
        let mut file_path = None;
        if let Some(client) = self.octoprint.get(printer) {
            // Encode each component: an external file may have folders or punctuation.
            let (origin, _) = identity
                .split_once('/')
                .ok_or_else(|| refuse("Live file identity is missing. Read fresh status."))?;
            let path = job_path.ok_or_else(|| {
                refuse("Live file path is missing. Read fresh status before Stop.")
            })?;
            let encode = |part: &str| -> String {
                part.bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                            char::from(b).to_string()
                        } else {
                            format!("%{b:02X}")
                        }
                    })
                    .collect()
            };
            let path = format!(
                "/api/files/{}/{}",
                encode(origin),
                path.split('/').map(encode).collect::<Vec<_>>().join("/")
            );
            baseline = Some(crate::octoprint::history(&client.json(&path).await?));
            file_path = Some(path);
            let fresh = client.snapshot().await?.printer;
            if fresh.job_id.as_deref() != Some(identity)
                || fresh.job_path.as_deref() != job_path
                || fresh.job_name.as_deref() != Some(name)
                || !matches!(
                    fresh.state,
                    PrinterState::Printing | PrinterState::Paused | PrinterState::Attention
                )
            {
                return Err(refuse("The live job changed. Confirm Stop again."));
            }
            client
                .command("/api/job", serde_json::json!({"command":"cancel"}))
                .await?;
        } else if let Some(client) = self.moonraker.get(printer) {
            let fresh = client.snapshot().await?;
            if fresh.printer.job_id.as_deref() != Some(identity)
                || fresh.printer.job_name.as_deref() != Some(name)
                || fresh.history.get("status").and_then(Value::as_str) != Some("in_progress")
            {
                return Err(refuse("The live job changed. Confirm Stop again."));
            }
            client.command("/printer/print/cancel").await?;
        } else {
            // PrusaLink addresses the execution itself; a replacement cannot be cancelled by this URL.
            if !identity.bytes().all(|b| b.is_ascii_digit()) {
                return Err(refuse("Live job identity is invalid. Read fresh status."));
            }
            let (code, _) = self
                .client(printer)?
                .request(
                    Method::DELETE,
                    &format!("/api/v1/job/{identity}"),
                    Bytes::new(),
                    false,
                )
                .await?;
            if code != StatusCode::NO_CONTENT {
                return Err(refuse(
                    "Printer refused Stop. Read fresh status before another action.",
                ));
            }
        }
        eprintln!("Stop is pending. Waiting for fresh status; do not repeat the command.");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
        loop {
            let observe = async {
                let mut p = self.printer(printer).await?;
                let confirmed = if let Some(client) = self.moonraker.get(printer) {
                    let s = client.snapshot().await?;
                    let confirmed = s.printer.job_id.as_deref() == Some(identity)
                        && s.printer.state == PrinterState::Idle
                        && s.history.get("status").and_then(Value::as_str) == Some("cancelled");
                    p = s.printer;
                    confirmed
                } else if let Some(client) = self.octoprint.get(printer) {
                    if p.state == PrinterState::Idle {
                        let history = crate::octoprint::history(
                            &client
                                .json(file_path.as_deref().ok_or(Error::Unavailable)?)
                                .await?,
                        );
                        crate::octoprint::history_date(&history).is_some_and(|date| {
                            baseline
                                .as_ref()
                                .and_then(crate::octoprint::history_date)
                                .is_none_or(|old| date > old)
                        }) && history.get("success").and_then(Value::as_bool) == Some(false)
                    } else {
                        false
                    }
                } else {
                    self.client(printer)?.terminal(identity, name) == Some(JobState::Cancelled)
                        || p.state == PrinterState::Idle
                            && p.detail.as_deref() == Some("STOPPED")
                            && p.job_id.as_deref().is_none_or(|id| id == identity)
                };
                if confirmed {
                    return Ok(Some(p));
                }
                if p.job_id.as_deref().is_some_and(|id| id != identity) {
                    return Err(refuse(
                        "Live job changed while Stop was pending. Read fresh status.",
                    ));
                }
                Ok(None)
            };
            match tokio::time::timeout_at(deadline, observe).await {
                Ok(Ok(Some(p))) => return Ok(p),
                Ok(Err(e @ Error::Refused(_))) => return Err(e),
                _ => (),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(refuse("Stop is not confirmed after waiting. Read fresh status; do not repeat the command."));
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + std::time::Duration::from_millis(250)).min(deadline),
            )
            .await;
        }
    }
}

impl Printers {
    /// Called with the printer command mutex held. No global jobs lock spans I/O.
    pub(super) async fn cleanup_staged(&self, target: &mut Prepared) {
        if matches!(
            target.job.state,
            JobState::Ready | JobState::Printing | JobState::Paused
        ) {
            return;
        }
        let Some(staged) = target.staged.clone() else {
            return;
        };
        let cleanup = async {
            let live = if let Ok(client) = self.client(&target.job.printer) {
                client.snapshot().await?.printer
            } else {
                self.printer(&target.job.printer).await?
            };
            // A terminal identity can remain in idle telemetry. During any active or
            // unresolved state, defer deletion even if its display name differs.
            if live.state != PrinterState::Idle {
                return Err(Error::Unavailable);
            }
            // Persisted paths must still address exactly the unique name made for this job.
            let expected = format!("dutchman-{}", hash(target.job.id.as_bytes()));
            if target.filename != format!("{expected}.gcode")
                && target.filename != format!("{expected}.gcode.3mf")
            {
                return Err(Error::Unavailable);
            }
            if let Some(client) = self.bambu.get(&target.job.printer) {
                if staged != target.filename {
                    return Err(Error::Unavailable);
                }
                client.delete(&staged).await
            } else if let Some(client) = self.octoprint.get(&target.job.printer) {
                if staged != target.filename {
                    return Err(Error::Unavailable);
                }
                client.delete(&staged).await
            } else if let Some(client) = self.moonraker.get(&target.job.printer) {
                if staged != target.filename {
                    return Err(Error::Unavailable);
                }
                client.delete(&staged).await
            } else if let Some(client) = self.duet.get(&target.job.printer) {
                if staged != target.filename {
                    return Err(Error::Unavailable);
                }
                client.delete(&staged).await
            } else if let Some(client) = self.repetier.get(&target.job.printer) {
                if staged != target.filename {
                    return Err(Error::Unavailable);
                }
                client.delete(&staged).await
            } else {
                let storage = staged
                    .strip_prefix("/api/v1/files/")
                    .and_then(|p| p.strip_suffix(&format!("/{}", target.filename)))
                    .ok_or(Error::Unavailable)?;
                if storage.is_empty()
                    || !storage
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                {
                    return Err(Error::Unavailable);
                }
                let (code, _) = self
                    .client(&target.job.printer)?
                    .request(Method::DELETE, &staged, Bytes::new(), false)
                    .await?;
                if matches!(code, StatusCode::NO_CONTENT | StatusCode::NOT_FOUND) {
                    Ok(())
                } else {
                    Err(Error::Unavailable)
                }
            }
        }
        .await;
        if cleanup.is_ok() {
            target.staged = None;
            target.job.detail = None;
        } else {
            target.job.detail = Some("Staged file cleanup is unconfirmed. Inspect printer storage and remove the staged file when it is no longer live.".into());
        }
    }
}
