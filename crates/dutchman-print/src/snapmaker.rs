//! Snapmaker 2.0 (legacy HTTP) and Snapmaker J1 and Artisan (SACP over TCP) printers,
//! handwritten from Snapmaker's published API description and SACP documentation. Both are
//! monitoring only: nothing beyond the documented read requests is ever sent.

use crate::backend::{Error, Printer, PrinterState};

#[path = "snapmaker_http.rs"]
pub mod http;
#[path = "snapmaker_sacp.rs"]
pub mod sacp;

pub const MONITORING_2: &str = "Snapmaker 2.0 printers are monitoring only. Snapmaker has not published an HTTP contract for uploading, starting, pausing, resuming, or stopping a job on them, only connect, status, and disconnect, so Print never sends one. Use the touchscreen or Snapmaker Luban.";
pub const MONITORING_J1: &str = "Snapmaker J1 and Artisan printers are monitoring only. Snapmaker has not published the SACP commands for 3D printing, so Print never uploads, starts, pauses, resumes, or stops a job on them. Use the touchscreen or Snapmaker Luban.";

/// Host and port only: there are no credentials, profiles, or slicer settings to give.
pub fn validate(config: &crate::config::Printer) -> Result<(), String> {
    let host = config.host.as_deref().unwrap_or_default();
    let hostname = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        && !host.starts_with(['-', '.']);
    if !hostname && host.parse::<std::net::Ipv6Addr>().is_err() {
        return Err(
            "Set host to the Snapmaker printer's hostname or IP address, without a scheme or path."
                .into(),
        );
    }
    if config.port == Some(0) {
        return Err("Set a printer port between 1 and 65535.".into());
    }
    if config.url.is_some()
        || config.api_key.is_some()
        || config.username.is_some()
        || config.password.is_some()
        || config.access_code.is_some()
        || config.serial.is_some()
        || config.ca_file.is_some()
        || config.cert_sha256.is_some()
        || config.ftps_port.is_some()
        || config.profile.is_some()
        || config.mainboard_id.is_some()
        || config.nozzle_diameter.is_some()
        || config.lan_control
        || config.resume_commissioned
        || !config.ams_mapping.is_empty()
        || config.slicer.is_some()
        || config.printer_profile.is_some()
        || config.print_profile.is_some()
        || config.filament_profile.is_some()
    {
        return Err("Set only host and an optional port for Snapmaker 2.0, J1, and Artisan printers. Print does not slice or send jobs to them, so leave slicer and slicer profiles unset.".into());
    }
    Ok(())
}

/// Whether a printer kind is one of the monitoring-only Snapmaker kinds.
pub fn is_kind(kind: Option<&str>) -> bool {
    matches!(kind, Some("snapmaker-2" | "snapmaker-j1"))
}

/// Keeps printable ASCII only, so a printer-supplied value cannot forge `detail` text.
pub(crate) fn plain(value: &str, limit: usize) -> String {
    value
        .chars()
        .take(limit)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

pub(crate) fn offline(config: &crate::config::Printer, reason: &str) -> Printer {
    report(config, PrinterState::Offline, reason.into())
}

pub(crate) fn report(
    config: &crate::config::Printer,
    state: PrinterState,
    detail: String,
) -> Printer {
    Printer {
        id: config.id.clone(),
        name: config.name.clone(),
        model: config.model.clone(),
        state,
        detail: Some(detail),
        needs_clearing: None,
        job_path: None,
        job_id: None,
        job_name: None,
        progress: None,
        seconds_left: None,
        nozzle_temperature: None,
        bed_temperature: None,
    }
}

/// One monitoring-only Snapmaker printer of either transport.
pub enum Client {
    Http(http::Client),
    Sacp(sacp::Client),
}
impl Client {
    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        match config.kind.as_deref() {
            Some("snapmaker-2") => http::Client::new(config).map(Self::Http),
            Some("snapmaker-j1") => sacp::Client::new(config).map(Self::Sacp),
            _ => Err("Set kind to snapmaker-2 or snapmaker-j1.".into()),
        }
    }
    pub async fn snapshot(&self) -> Printer {
        match self {
            Self::Http(client) => client.snapshot().await,
            Self::Sacp(client) => client.snapshot().await,
        }
    }
    fn message(&self) -> &'static str {
        match self {
            Self::Http(_) => MONITORING_2,
            Self::Sacp(_) => MONITORING_J1,
        }
    }
    /// Every machine action, preparation included, is refused.
    pub fn check_control(&self) -> Result<(), Error> {
        Err(self.refusal())
    }
    pub fn refusal(&self) -> Error {
        Error::Refused(self.message().into())
    }
}
