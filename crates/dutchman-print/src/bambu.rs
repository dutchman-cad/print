//! Bambu LAN telemetry and explicitly enabled experimental control.

use crate::backend::{Error, Printer, PrinterState};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, Transport};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::time::Instant;

const FRESH: Duration = Duration::from_secs(30);
const PACING: Duration = Duration::from_secs(300);
const DEADLINE: Duration = Duration::from_secs(4);

#[path = "bambu_control.rs"]
mod control;
pub(crate) use control::start_payload;
pub use control::StartProfile;

#[path = "bambu_ftps.rs"]
mod ftps;

pub fn unsupported() -> Error {
    Error::Refused(
        "Bambu control is disabled. Set lan_control = true only after confirming LAN/Developer Mode allows control."
            .into(),
    )
}

pub fn validate(p: &crate::config::Printer) -> Result<(), String> {
    validate_enrollment(p, false)
}

pub(crate) fn validate_enrollment(
    p: &crate::config::Printer,
    enrollment: bool,
) -> Result<(), String> {
    let host = p.host.as_deref().unwrap_or_default();
    if host.is_empty()
        || host.len() > 253
        || host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '@' | '#' | '?'))
    {
        return Err("Set Bambu host to an IP address or hostname without a port or URL.".into());
    }
    if host.contains(':') && host.parse::<std::net::IpAddr>().is_err() {
        return Err("Set Bambu host without a port or URL.".into());
    }
    if p.port == Some(0) {
        return Err("Set the Bambu port between 1 and 65535.".into());
    }
    if p.ftps_port == Some(0) {
        return Err("Set the Bambu ftps_port between 1 and 65535.".into());
    }
    let serial = p.serial.as_deref().unwrap_or_default();
    if serial.is_empty()
        || serial.len() > 64
        || !serial
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(
            "Set the Bambu printer serial to at most 64 letters, digits, or hyphens.".into(),
        );
    }
    if p.access_code
        .as_ref()
        .is_none_or(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control))
    {
        return Err("Set the Bambu LAN access_code without control characters.".into());
    }
    if p.ca_file.is_some() == p.cert_sha256.is_some() && !(enrollment && p.ca_file.is_none()) {
        return Err("Set exactly one Bambu ca_file or cert_sha256 trust decision.".into());
    }
    if p.cert_sha256
        .as_ref()
        .is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("Set cert_sha256 to the certificate's 64 hexadecimal SHA-256 digits.".into());
    }
    if p.url.is_some() || p.api_key.is_some() || p.username.is_some() || p.password.is_some() {
        return Err("Use host, serial, and access_code for Bambu LAN monitoring.".into());
    }
    p.start_profile.validate()?;
    if p.ams_mapping.len() > 64
        || p.ams_mapping
            .iter()
            .any(|(k, v)| k.parse::<usize>().ok().is_none_or(|n| n == 0 || n > 64) || *v > 15)
    {
        return Err("Set ams_mapping keys to filament numbers 1–64 and slots to 0–15.".into());
    }
    Ok(())
}

#[derive(Debug)]
struct Identity {
    ca: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    enrollment: bool,
    pin: Option<String>,
    serial: ServerName<'static>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for Identity {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.enrollment {
            Ok(ServerCertVerified::assertion())
        } else if let Some(ca) = &self.ca {
            ca.verify_server_cert(cert, chain, &self.serial, ocsp, now)
        } else if self.pin.as_deref() == Some(crate::safety::hash(cert.as_ref()).as_str()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "Printer certificate does not match the configured fingerprint".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn tls(p: &crate::config::Printer) -> Result<rustls::ClientConfig, String> {
    use std::io::Read as _;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let ca = if let Some(path) = &p.ca_file {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| "Cannot open Bambu ca_file.")?
            .take((1 << 20) + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read Bambu ca_file.")?;
        if bytes.len() > 1 << 20 {
            return Err("Bambu ca_file exceeds 1 MiB.".into());
        }
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut bytes.as_slice()) {
            roots
                .add(cert.map_err(|_| "Cannot read Bambu CA certificates.")?)
                .map_err(|_| "Cannot trust Bambu CA certificate.")?;
        }
        if roots.is_empty() {
            return Err("Add a PEM CA certificate to Bambu ca_file.".into());
        }
        Some(
            rustls::client::WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots),
                provider.clone(),
            )
            .build()
            .map_err(|_| "Cannot configure Bambu CA trust.")?,
        )
    } else {
        None
    };
    let serial = ServerName::try_from(p.serial.clone().unwrap_or_default())
        .map_err(|_| "Check the Bambu serial certificate name.")?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot configure Bambu TLS.")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Identity {
            ca,
            enrollment: false,
            pin: p.cert_sha256.as_ref().map(|s| s.to_ascii_lowercase()),
            serial,
            provider,
        }))
        .with_no_client_auth();
    // The broker address is an IP or hostname; certificate identity is the configured serial.
    // Suppress address-based SNI rather than advertising the wrong identity.
    config.enable_sni = false;
    Ok(config)
}

#[derive(Clone)]
struct Telemetry {
    fields: Map<String, Value>,
    received: Option<Instant>,
    state_received: Option<Instant>,
    job_received: Option<Instant>,
    reason: &'static str,
    terminal: bool,
    connected: bool,
    terminal_error: Option<u64>,
    hms_invalid: bool,
}
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            fields: Map::new(),
            received: None,
            state_received: None,
            job_received: None,
            reason: "Waiting for fresh Bambu telemetry.",
            terminal: false,
            connected: false,
            terminal_error: None,
            hms_invalid: false,
        }
    }
}
fn value_id(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
fn material(v: Option<&Value>) -> Value {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
        .map_or(Value::Null, |s| Value::String(s.into()))
}
impl Telemetry {
    fn active_error(&self) -> bool {
        self.fields
            .get("print_error")
            .and_then(Value::as_u64)
            .is_some_and(|n| n != 0 && Some(n) != self.terminal_error)
    }
    fn serious_hms(&self) -> bool {
        self.hms_invalid
            || self
                .fields
                .get("hms")
                .and_then(Value::as_array)
                .is_some_and(|alerts| {
                    alerts.iter().any(|a| {
                        a.get("attr")
                            .and_then(Value::as_u64)
                            .is_none_or(|attr| !matches!((attr >> 8) & 0xf, 3 | 4))
                    })
                })
    }
    fn hms_detail(&self) -> String {
        if self.hms_invalid {
            return "HMS status is invalid. Read fresh status".into();
        }
        self.fields
            .get("hms")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|a| {
                let attr = a.get("attr")?.as_u64()?;
                let code = a.get("code")?.as_u64()?;
                let code = format!(
                    "{:04X}_{:04X}_{:04X}_{:04X}",
                    attr >> 16,
                    attr & 0xffff,
                    code >> 16,
                    code & 0xffff
                );
                Some(format!(
                    "HMS {code} https://wiki.bambulab.com/en/x1/troubleshooting/hmscode/{code}"
                ))
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn report(&mut self, bytes: &[u8]) -> bool {
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return false;
        };
        let Some(print) = value.get("print").and_then(Value::as_object) else {
            return false;
        };
        let previous_state = self.fields.get("gcode_state").cloned();
        let previous_job = self.fields.get("subtask_name").cloned();
        if print
            .get("subtask_name")
            .is_some_and(|name| Some(name) != previous_job.as_ref())
            && !print.contains_key("gcode_start_time")
        {
            self.fields.remove("gcode_start_time");
        }
        let old_error = self.fields.get("print_error").and_then(Value::as_u64);
        if let Some(hms) = print.get("hms") {
            self.hms_invalid = true;
            self.fields.remove("hms");
            if let Some(alerts) = hms.as_array().filter(|a| a.len() <= 64) {
                let valid = alerts.iter().all(|a| {
                    ["attr", "code"].iter().all(|key| {
                        a.get(*key)
                            .and_then(Value::as_u64)
                            .is_some_and(|n| u32::try_from(n).is_ok())
                    })
                });
                if valid {
                    let alerts: Vec<_> = alerts
                        .iter()
                        .map(|a| serde_json::json!({"attr":a.get("attr"),"code":a.get("code")}))
                        .collect();
                    self.fields.insert("hms".into(), Value::Array(alerts));
                    self.hms_invalid = false;
                }
            }
        }
        // Cache only bounded slot identities and material names from nested AMS deltas.
        let valid_trays = |value: &Value| {
            value.as_array().is_some_and(|trays| {
                trays.len() <= 4
                    && trays
                        .iter()
                        .all(|tray| tray.get("id").and_then(value_id).is_some_and(|id| id < 4))
            })
        };
        if let Some(units) = print.get("ams").and_then(|a| a.get("ams")) {
            let valid = units.as_array().is_some_and(|units| {
                units.len() <= 4
                    && units.iter().all(|unit| {
                        unit.get("id").and_then(value_id).is_some_and(|id| id < 4)
                            && unit.get("tray").is_none_or(valid_trays)
                    })
            });
            if !valid || units.as_array().is_some_and(|units| units.is_empty()) {
                self.fields.retain(|key, _| !key.starts_with("tray_"));
            }
        }
        if let Some(units) = print
            .get("ams")
            .and_then(|a| a.get("ams"))
            .and_then(Value::as_array)
            .filter(|a| a.len() <= 4)
        {
            for unit in units {
                let id = unit.get("id").and_then(value_id).filter(|n| *n < 4);
                if let (Some(id), Some(trays)) = (
                    id,
                    unit.get("tray")
                        .and_then(Value::as_array)
                        .filter(|a| a.len() <= 4),
                ) {
                    if trays.is_empty() {
                        for slot in 0..4 {
                            self.fields.remove(&format!("tray_{}", id * 4 + slot));
                        }
                    }
                    for tray in trays {
                        if let Some(slot) = tray.get("id").and_then(value_id).filter(|n| *n < 4) {
                            let key = format!("tray_{}", id * 4 + slot);
                            if tray.get("tray_type").is_some()
                                || tray.as_object().is_some_and(|t| t.len() == 1)
                            {
                                self.fields.insert(key, material(tray.get("tray_type")));
                            }
                        }
                    }
                }
            }
        }
        if let Some(tray) = print.get("vt_tray") {
            if tray.get("tray_type").is_some() || tray.as_object().is_none_or(|t| t.len() <= 1) {
                self.fields
                    .insert("external_tray".into(), material(tray.get("tray_type")));
            }
        }
        // Only retain known scalar telemetry. Unknown nested data cannot grow the cache.

        for key in [
            "gcode_state",
            "mc_percent",
            "mc_remaining_time",
            "nozzle_temper",
            "bed_temper",
            "subtask_name",
            "gcode_start_time",
            "gcode_file",
            "print_error",
            "nozzle_diameter",
            "nozzle_type",
        ] {
            if let Some(v) = print.get(key) {
                if v.is_number()
                    || v.is_null()
                    || v.as_str()
                        .is_some_and(|s| s.len() <= 256 && !s.chars().any(char::is_control))
                {
                    if key == "subtask_name" && v.is_string() {
                        self.job_received = Some(Instant::now());
                    }
                    if key == "gcode_state" && v.is_string() {
                        self.state_received = Some(Instant::now());
                    }
                    self.fields.insert(key.into(), v.clone());
                } else {
                    self.fields.remove(key);
                }
            }
        }
        self.connected = true;
        self.received = Some(Instant::now());
        if !self.fields.get("gcode_state").is_some_and(Value::is_string) {
            return false;
        }
        let terminal = matches!(
            self.fields.get("gcode_state").and_then(Value::as_str),
            Some("FINISH" | "FAILED")
        );
        let terminal_transition = terminal
            && (self.fields.get("gcode_state") != previous_state.as_ref()
                || self.fields.get("subtask_name") != previous_job.as_ref());
        if terminal_transition {
            self.terminal = true;
            self.terminal_error = Some(
                self.fields
                    .get("print_error")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
        }
        if !terminal_transition
            && self.fields.get("print_error").and_then(Value::as_u64) != old_error
        {
            // A new fault after the terminal transition is active, even if the state stays latched.
            self.terminal_error = None;
        }
        true
    }
    fn printer(&self, p: &crate::config::Printer, fresh: Duration) -> Printer {
        let live = self.connected
            && self.received.is_some_and(|t| t.elapsed() < fresh)
            && self.fields.get("gcode_state").is_some_and(Value::is_string);
        let detail = if live {
            self.fields
                .get("gcode_state")
                .and_then(Value::as_str)
                .unwrap_or("Unknown Bambu state")
        } else if self.received.is_some() {
            "Bambu telemetry is stale. Wait for fresh reports."
        } else {
            self.reason
        };
        let state = if !live {
            PrinterState::Offline
        } else if detail == "PAUSE" && !self.hms_detail().is_empty() {
            PrinterState::Attention
        } else if self.serious_hms() || self.active_error() {
            PrinterState::Error
        } else {
            match detail {
                "IDLE" | "FINISH" | "FAILED" => PrinterState::Idle,
                "PREPARE" | "RUNNING" => PrinterState::Printing,
                "PAUSE" => PrinterState::Paused,
                _ => PrinterState::Error,
            }
        };
        let mut detail = detail.to_owned();
        if live {
            let alerts = self.hms_detail();
            if !alerts.is_empty() {
                if detail == "PAUSE" {
                    detail.push_str("; Needs attention");
                }
                detail.push_str("; ");
                detail.push_str(&alerts);
            }
        }
        let number = |key: &str| {
            if live {
                self.fields
                    .get(key)
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite())
            } else {
                None
            }
        };
        Printer {
            id: p.id.clone(),
            name: p.name.clone(),
            model: p.model.clone(),
            state,
            detail: Some(detail),
            needs_clearing: Some(self.terminal),
            job_id: if live {
                let name = self
                    .fields
                    .get("subtask_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let start = self
                    .fields
                    .get("gcode_start_time")
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                    })
                    .filter(|n| *n > 0);
                name.zip(start)
                    .map(|(name, start)| format!("{name}:{start}"))
            } else {
                None
            },
            job_name: live
                .then(|| {
                    self.fields
                        .get("subtask_name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| self.fields.get("gcode_file").and_then(Value::as_str))
                        .map(String::from)
                })
                .flatten(),
            progress: number("mc_percent")
                .filter(|v| (0.0..=100.0).contains(v))
                .map(|v| v / 100.0),
            seconds_left: if live {
                self.fields
                    .get("mc_remaining_time")
                    .and_then(Value::as_u64)
                    .and_then(|v| v.checked_mul(60))
            } else {
                None
            },
            nozzle_temperature: number("nozzle_temper"),
            bed_temperature: number("bed_temper"),
        }
    }
}

pub struct Client {
    config: crate::config::Printer,
    options: MqttOptions,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    state: watch::Sender<Telemetry>,
    oneshot: bool,
    fresh: Duration,
    pacing: Duration,
    sequence: std::sync::atomic::AtomicU64,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = self.worker.get_mut().take() {
            task.abort();
        }
    }
}
impl Client {
    /// Stages bytes without starting a print, after a read-only readiness check.
    pub async fn upload(&self, filename: &str, bytes: &[u8]) -> Result<(), Error> {
        self.check_control()?;
        self.check_materials(bytes).await?;
        ftps::upload(&self.config, filename, bytes).await
    }

    pub async fn check_materials(&self, bytes: &[u8]) -> Result<(), Error> {
        self.ready().await?;
        control::check_materials(&self.config, bytes, &self.state.borrow().fields).map_err(
            |error| match error {
                Error::Refused(mut message) => {
                    if let Some(secret) = self.config.access_code.as_deref() {
                        message = message.replace(secret, "[redacted]");
                    }
                    Error::Refused(message)
                }
                other => other,
            },
        )
    }

    pub fn check_control(&self) -> Result<(), Error> {
        if self.config.lan_control {
            Ok(())
        } else {
            Err(unsupported())
        }
    }

    pub async fn ready(&self) -> Result<(), Error> {
        let status = self.snapshot().await;
        let telemetry = self.state.borrow();
        if telemetry.serious_hms() {
            return Err(Error::Refused(format!(
                "Resolve active Bambu alerts before starting: {}.",
                telemetry.hms_detail()
            )));
        }
        if status.state != PrinterState::Idle
            || telemetry.active_error()
            || telemetry
                .fields
                .get("print_error")
                .and_then(Value::as_u64)
                .is_none()
        {
            return Err(Error::Refused("Bambu readiness is unknown or the printer is busy. Read fresh idle status before starting.".into()));
        }
        Ok(())
    }

    /// A command connection is discarded after any transport error, with no reconnect or retry.
    pub async fn command(
        &self,
        mut payload: Value,
        name: &str,
        expected: &str,
        identity: Option<&str>,
        guard: &mut Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<(), Error> {
        self.check_control()?;
        // Seeded from the clock in milliseconds, so it never reaches u64::MAX.
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .to_string();
        let print = payload
            .get_mut("print")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                Error::Refused("Cannot build the Bambu command. Review the start profile.".into())
            })?;
        print.insert("sequence_id".into(), Value::String(sequence.clone()));
        let command = print
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let serial = self.config.serial.as_deref().unwrap_or_default();
        let mut options = self.options.clone();
        options.set_client_id(format!(
            "control-{}",
            crate::new_job_id().map_err(Error::Failed)?
        ));
        let (client, mut events) = AsyncClient::new(options, 8);
        let mut telemetry = self.state.borrow().clone();
        telemetry.state_received = None;
        if command == "project_file" {
            telemetry.job_received = None;
        }
        let operation = async {
            let mut sent = false;
            loop {
                match events.poll().await.map_err(|_| Error::Refused("Command reply was lost. Read status to reconcile the job; do not repeat Start.".into()))? {
                    Event::Incoming(Incoming::ConnAck(_)) => client.subscribe(format!("device/{serial}/report"), QoS::AtMostOnce).await.map_err(|_| Error::Unavailable)?,
                    Event::Incoming(Incoming::SubAck(_)) if !sent => {
                        sent = true;
                        client.publish(format!("device/{serial}/request"), QoS::AtLeastOnce, false, payload.to_string()).await.map_err(|_| Error::Unavailable)?;
                    }
                    // Keep Stop serialized through its outcome; other commands allow Stop to overtake their wait.
                    Event::Outgoing(rumqttc::Outgoing::Publish(_)) if sent && expected != "FAILED" => { guard.take(); }
                    Event::Incoming(Incoming::Publish(p)) if sent && !p.retain && p.topic == format!("device/{serial}/report") => {
                        let value: Value = serde_json::from_slice(&p.payload).unwrap_or(Value::Null);
                        if value.pointer("/print/sequence_id").and_then(Value::as_str) == Some(&sequence)
                            && value.pointer("/print/command").and_then(Value::as_str) == Some(&command)
                            && value.pointer("/print/result").and_then(Value::as_str).is_some_and(|s| !s.eq_ignore_ascii_case("success")) {
                            return Err(Error::Refused("Bambu rejected the command. Check authorization and LAN/Developer Mode, then read status.".into()));
                        }
                        telemetry.report(&p.payload);
                        if telemetry.state_received.is_some() && telemetry.job_received.is_some()
                            && telemetry.printer(&self.config, self.fresh).job_id.as_deref().is_some_and(|id| identity.is_none_or(|expected| id == expected))
                            && (expected == "FAILED" || (!telemetry.active_error() && telemetry.fields.get("print_error").and_then(Value::as_u64).is_some()))
                            && telemetry.fields.get("subtask_name").and_then(Value::as_str) == Some(name)
                            && telemetry.fields.get("gcode_state").and_then(Value::as_str).is_some_and(|s| s == expected || s == "FAILED" || (expected == "RUNNING" && s == "PREPARE")) {
                            telemetry.terminal |= self.state.borrow().terminal;
                            self.state.send_replace(telemetry);
                            return Ok(());
                        }
                    }
                    _ => (),
                }
            }
        };
        eprintln!("Bambu command is pending. Waiting for fresh status; do not repeat the command.");
        let deadline =
            Duration::from_secs(if matches!(command.as_str(), "project_file" | "stop") {
                25
            } else {
                10
            });
        tokio::time::timeout(deadline, operation).await.map_err(|_| Error::Refused("Command is not confirmed. Check authorization and read status to reconcile the job. Start will not be repeated.".into()))?
    }

    pub fn new(config: crate::config::Printer, oneshot: bool) -> Result<Self, String> {
        validate(&config)?;
        // Machine actions need live monitoring throughout review and execution.
        let oneshot = oneshot && !config.lan_control;
        let mut options = MqttOptions::new(
            format!("dutchman-{}", crate::new_job_id()?),
            config.host.clone().unwrap_or_default(),
            config.port.unwrap_or(8883),
        );
        options.set_credentials("bblp", config.access_code.clone().unwrap_or_default());
        options.set_transport(Transport::tls_with_config(tls(&config)?.into()));
        options.set_keep_alive(Duration::from_secs(10));
        options.set_max_packet_size(1 << 20, 4096);
        let (state, _) = watch::channel(Telemetry::default());
        Ok(Self {
            config,
            options,
            worker: Mutex::new(None),
            state,
            oneshot,
            fresh: FRESH,
            pacing: PACING,
            sequence: std::sync::atomic::AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "Cannot establish a Bambu command sequence.")?
                    .as_millis()
                    .try_into()
                    .map_err(|_| "Cannot establish a Bambu command sequence.")?,
            ),
        })
    }
    pub async fn snapshot(&self) -> Printer {
        let mut rx = self.state.subscribe();
        let mut worker = self.worker.lock().await;
        if worker.is_none() {
            let options = self.options.clone();
            let tx = self.state.clone();
            let serial = self.config.serial.clone().unwrap_or_default();
            let oneshot = self.oneshot;
            let pacing = self.pacing;
            *worker = Some(tokio::spawn(async move {
                monitor(options, serial, tx, oneshot, pacing).await;
            }));
        }
        drop(worker);
        if rx.borrow().received.is_none() {
            let _ = tokio::time::timeout(DEADLINE, async {
                while rx.borrow().received.is_none() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    if rx.borrow().reason != "Waiting for fresh Bambu telemetry." {
                        break;
                    }
                }
            })
            .await;
        }
        if self.oneshot {
            let worker = self.worker.lock().await;
            if let Some(task) = worker.as_ref() {
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    while !task.is_finished() {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await;
            }
        }
        let mut printer = rx.borrow().printer(&self.config, self.fresh);
        if let Some(secret) = self.config.access_code.as_deref() {
            for value in [&mut printer.detail, &mut printer.job_name]
                .into_iter()
                .flatten()
            {
                *value = value.replace(secret, "[redacted]");
            }
        }
        printer
    }
    pub fn acknowledge(&self) {
        self.state.send_modify(|s| s.terminal = false);
    }
}

async fn monitor(
    options: MqttOptions,
    serial: String,
    tx: watch::Sender<Telemetry>,
    oneshot: bool,
    pacing: Duration,
) {
    let report = format!("device/{serial}/report");
    let request = format!("device/{serial}/request");
    let mut last_request: Option<Instant> = None;
    let mut backoff = 1;
    loop {
        let (client, mut events) = AsyncClient::new(options.clone(), 8);
        let mut network = rumqttc::NetworkOptions::new();
        network.set_connection_timeout(4);
        events.set_network_options(network);
        let mut state = tx.borrow().clone();
        state.connected = false;
        state.received = None;
        let mut due = None;
        tx.send_replace(state.clone());
        loop {
            let event = tokio::select! {
                event = events.poll() => event,
                () = async { match due { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                    let payload = serde_json::json!({"pushing":{"command":"pushall","sequence_id":"0","version":1,"push_target":1}}).to_string();
                    if client.publish(&request, QoS::AtMostOnce, false, payload).await.is_err() { break; }
                    last_request = Some(Instant::now());
                    due = None;
                    continue;
                }
            };
            match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    state.connected = true;
                    if client.subscribe(&report, QoS::AtMostOnce).await.is_err() {
                        break;
                    }
                }
                Ok(Event::Incoming(Incoming::SubAck(_))) => {
                    // Reconnect storms must not flood P1 printers with full-state requests.
                    due = Some(
                        last_request
                            .map(|t| t + pacing)
                            .unwrap_or_else(Instant::now),
                    );
                }
                Ok(Event::Incoming(Incoming::Publish(p))) if p.topic == report && !p.retain => {
                    state.terminal = tx.borrow().terminal;
                    state.received = Some(Instant::now());
                    if state.report(&p.payload) {
                        tx.send_replace(state.clone());
                        backoff = 1;
                        if oneshot {
                            let _ = client.disconnect().await;
                            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                                loop {
                                    if matches!(
                                        events.poll().await,
                                        Ok(Event::Outgoing(rumqttc::Outgoing::Disconnect)) | Err(_)
                                    ) {
                                        break;
                                    }
                                }
                            })
                            .await;
                            return;
                        }
                    } else {
                        tx.send_replace(state.clone());
                    }
                }
                Ok(Event::Incoming(_)) => {
                    state.terminal = tx.borrow().terminal;
                    state.received = Some(Instant::now());
                    tx.send_replace(state.clone());
                }
                Ok(_) => (),
                Err(error) => {
                    let reason = if matches!(
                        error,
                        rumqttc::ConnectionError::ConnectionRefused(
                            rumqttc::ConnectReturnCode::BadUserNamePassword
                                | rumqttc::ConnectReturnCode::NotAuthorized
                        )
                    ) {
                        "Bambu authentication failed. Check the LAN access code."
                    } else {
                        "Bambu connection unavailable. Check printer trust and LAN settings."
                    };
                    tx.send_modify(|s| {
                        s.received = None;
                        s.connected = false;
                        s.reason = reason;
                    });
                    break;
                }
            }
        }
        // A reconnect must revoke bed clearance even if it happens between safety polls.
        tx.send_modify(|s| {
            s.connected = false;
            s.received = None;
            s.terminal = true;
        });
        if oneshot {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

/// Certificate details observed without authenticating to MQTT or changing trust.
#[derive(serde::Serialize)]
pub struct Fingerprint {
    pub cert_sha256: String,
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    pub instruction: &'static str,
}

/// Performs one bounded TLS handshake. The certificate is observed, not trusted.
pub async fn fingerprint(p: &crate::config::Printer) -> Result<Fingerprint, String> {
    validate_enrollment(p, true)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let serial = ServerName::try_from(p.serial.clone().unwrap_or_default())
        .map_err(|_| "Check the Bambu serial certificate name.")?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot configure Bambu TLS.")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Identity {
            ca: None,
            enrollment: true,
            pin: None,
            serial: serial.clone(),
            provider,
        }))
        .with_no_client_auth();
    config.enable_sni = false;
    tokio::time::timeout(DEADLINE, async {
        let socket = tokio::net::TcpStream::connect((
            p.host.as_deref().unwrap_or_default(), p.port.unwrap_or(8883),
        )).await.map_err(|_| "Cannot connect to the Bambu TLS port. Check host and port.")?;
        let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(serial, socket).await.map_err(|_| "Cannot read the Bambu TLS certificate.")?;
        let cert = stream.get_ref().1.peer_certificates().and_then(|c| c.first())
            .ok_or("The Bambu TLS port returned no certificate.")?;
        let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
            .map_err(|_| "Cannot read the Bambu certificate details.")?;
        let report = Fingerprint {
            cert_sha256: crate::safety::hash(cert.as_ref()),
            subject: parsed.subject().to_string(),
            issuer: parsed.issuer().to_string(),
            not_before: parsed.validity().not_before.to_string(),
            not_after: parsed.validity().not_after.to_string(),
            instruction: "Compare this fingerprint with what the printer shows (if anything), then paste it as cert_sha256 in the private config. This observation does not establish trust.",
        };
        use tokio::io::AsyncWriteExt as _;
        let _ = stream.shutdown().await;
        Ok(report)
    }).await.map_err(|_| "Bambu certificate read timed out.")?
}

#[cfg(test)]
#[path = "bambu_tests.rs"]
mod tests;
