//! The asynchronous seam implemented by print backends.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

pub type Reply<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Printer or job not found. Check its id.")]
    NotFound,
    #[error("The printer is busy. Wait for its current job to finish.")]
    Busy,
    #[error("{0}")]
    Refused(String),
    #[error("The backend is unavailable or offline. Try again later.")]
    Unavailable,
    #[error("Printer authentication failed. Check its configured credentials.")]
    Authentication,
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrinterState {
    Idle,
    Printing,
    Paused,
    Error,
    Offline,
}

impl std::fmt::Display for PrinterState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Idle => "idle",
            Self::Printing => "printing",
            Self::Paused => "paused",
            Self::Error => "error",
            Self::Offline => "offline",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Ready,
    Printing,
    Paused,
    Done,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Printer {
    pub id: String,
    pub name: String,
    pub model: String,
    pub state: PrinterState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_clearing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds_left: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nozzle_temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bed_temperature: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Options {
    pub printer_profile: Option<String>,
    pub print_profile: Option<String>,
    pub filament_profile: Option<String>,
    pub name: Option<String>,
}

/// Validates a preset name before it can reach an external slicer.
pub(crate) fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.starts_with('-') {
        return Err("Profile names can't start with '-'.".into());
    }
    if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err("Use plain profile names of 1–256 bytes.".into());
    }
    Ok(())
}

impl Options {
    /// Applies the same preset rules to API, CLI, and direct backend callers.
    pub fn validate(&self) -> Result<(), String> {
        for name in [
            &self.printer_profile,
            &self.print_profile,
            &self.filament_profile,
        ]
        .into_iter()
        .flatten()
        {
            validate_profile_name(name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Estimate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filament_g: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<u64>,
    // Local paths stay in the companion; the API serves the preview separately.
    #[serde(skip)]
    pub preview: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub id: String,
    pub printer: String,
    pub name: String,
    pub state: JobState,
    pub estimate: Estimate,
    pub progress: Option<f64>,
    pub seconds_left: Option<u64>,
}

/// Errors must be safe to show to callers, without credentials or backend addresses.
pub trait Backend: Send + Sync {
    fn restore_jobs<'a>(&'a self, _path: &'a Path) -> Reply<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn persist_jobs<'a>(&'a self, _path: &'a Path) -> Reply<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Validate backend-specific preconditions without sending a machine command.
    fn review_action<'a>(
        &'a self,
        _action: &'a str,
        _printer: &'a str,
        _job: Option<&'a str>,
    ) -> Reply<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    /// Only PrusaLink READY is an operator's printer-side bed acknowledgement.
    fn bed_ready(&self, _printer: &str, _detail: Option<&str>) -> bool {
        false
    }
    /// Reject unavailable control before applying readiness checks.
    fn check_control(&self, _printer: &str) -> Result<(), Error> {
        Ok(())
    }

    /// Whether clear acknowledges a terminal-report latch, rather than a persistent live state.
    fn latches_terminal_reports(&self, _printer: &str) -> bool {
        false
    }
    /// Backend bookkeeping after the shared safety policy accepts bed acknowledgement.
    fn clear<'a>(&'a self, _printer: &'a str) -> Reply<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    /// Return fresh telemetry; unavailable or unknown readiness must never become idle.
    fn printers(&self) -> Reply<'_, Vec<Printer>>;
    fn printer<'a>(&'a self, id: &'a str) -> Reply<'a, Printer> {
        Box::pin(async move {
            self.printers()
                .await?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or(Error::NotFound)
        })
    }
    fn prepare<'a>(
        &'a self,
        job_id: &'a str,
        input: &'a Path,
        printer: &'a str,
        options: Options,
    ) -> Reply<'a, Estimate>;
    fn start<'a>(&'a self, job: &'a str) -> Reply<'a, Job>;
    fn cancel<'a>(&'a self, job: &'a str) -> Reply<'a, Job>;
    fn pause<'a>(&'a self, printer: &'a str) -> Reply<'a, Job>;
    fn resume<'a>(&'a self, printer: &'a str) -> Reply<'a, Job>;
    fn job<'a>(&'a self, job_id: &'a str) -> Reply<'a, Job>;
    /// Release expired job state as well as its files.
    fn forget<'a>(&'a self, job_id: &'a str) -> Reply<'a, ()>;
}
