//! Bambu LAN telemetry. Control is deliberately unavailable.

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

pub fn unsupported() -> Error {
    Error::Refused(
        "Bambu control is not supported yet. Monitoring and local preparation are available."
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
    reason: &'static str,
    terminal: bool,
}
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            fields: Map::new(),
            received: None,
            reason: "Waiting for fresh Bambu telemetry.",
            terminal: false,
        }
    }
}
impl Telemetry {
    fn report(&mut self, bytes: &[u8]) -> bool {
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return false;
        };
        let Some(print) = value.get("print").and_then(Value::as_object) else {
            return false;
        };
        let previous_state = self.fields.get("gcode_state").cloned();
        let previous_job = self.fields.get("subtask_name").cloned();
        // Only retain known scalar telemetry. Unknown nested data cannot grow the cache.
        let mut changed = false;
        for key in [
            "gcode_state",
            "mc_percent",
            "mc_remaining_time",
            "nozzle_temper",
            "bed_temper",
            "subtask_name",
            "gcode_file",
            "print_error",
        ] {
            if let Some(v) = print.get(key) {
                if v.is_number()
                    || v.is_null()
                    || v.as_str()
                        .is_some_and(|s| s.len() <= 256 && !s.chars().any(char::is_control))
                {
                    self.fields.insert(key.into(), v.clone());
                    changed = true;
                }
            }
        }
        if !changed {
            return false;
        }
        if !self.fields.get("gcode_state").is_some_and(Value::is_string) {
            return false;
        }
        let terminal = matches!(
            self.fields.get("gcode_state").and_then(Value::as_str),
            Some("FINISH" | "FAILED")
        );
        if terminal
            && (self.fields.get("gcode_state") != previous_state.as_ref()
                || self.fields.get("subtask_name") != previous_job.as_ref())
        {
            self.terminal = true;
        }
        self.received = Some(Instant::now());
        true
    }
    fn printer(&self, p: &crate::config::Printer, fresh: Duration) -> Printer {
        let live = self.received.is_some_and(|t| t.elapsed() < fresh);
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
        } else if self
            .fields
            .get("print_error")
            .and_then(Value::as_u64)
            .is_some_and(|n| n != 0)
        {
            PrinterState::Error
        } else {
            match detail {
                "IDLE" | "FINISH" => PrinterState::Idle,
                "PREPARE" | "RUNNING" => PrinterState::Printing,
                "PAUSE" => PrinterState::Paused,
                _ => PrinterState::Error,
            }
        };
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
            detail: Some(detail.into()),
            needs_clearing: Some(self.terminal),
            job_id: None,
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
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = self.worker.get_mut().take() {
            task.abort();
        }
    }
}
impl Client {
    pub fn new(config: crate::config::Printer, oneshot: bool) -> Result<Self, String> {
        validate(&config)?;
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
        let mut state = Telemetry {
            terminal: tx.borrow().terminal,
            ..Telemetry::default()
        };
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
                Ok(Event::Incoming(Incoming::Publish(p)))
                    if p.topic == report && !p.retain && due.is_none() =>
                {
                    state.terminal = tx.borrow().terminal;
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
                    }
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
                        s.fields.clear();
                        s.reason = reason;
                    });
                    break;
                }
            }
        }
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
