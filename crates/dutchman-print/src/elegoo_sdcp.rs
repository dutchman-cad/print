//! Elegoo SDCP printers over WebSocket, handwritten from CBD's public SDCP V3 specification
//! (resin) and public Centauri Carbon protocol notes (FDM). Centauri is monitoring only.

use crate::backend::{Error, Printer, PrinterState};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

const DEADLINE: Duration = Duration::from_secs(4);
const ACK: Duration = Duration::from_secs(10);
const OUTCOME: Duration = Duration::from_secs(25);
const MAX_MESSAGE: usize = 1 << 20;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const STATUS: u64 = 0;
const STOP: u64 = 130;

const WAITING: &str = "Waiting for fresh Elegoo status.";
const UNAVAILABLE: &str =
    "Elegoo connection unavailable. Check the printer's host, port, and network.";
const LOST: &str =
    "Elegoo connection lost. Reconnecting; status is not fresh until a new report arrives.";
const STALE: &str = "Elegoo status is stale. Wait for fresh reports.";
const OTHER_BOARD: &str =
    "The printer reports a different mainboard than mainboard_id. Check the configured host.";
pub const MONITORING: &str = "Elegoo Centauri printers are monitoring only. There is no published control contract for them, so Print never uploads, starts, pauses, resumes, or stops a job on them. Use the printer's screen or Elegoo's software.";
pub const RESIN_FILES: &str = "Print slices FDM G-code only, and resin printers need CTB or GOO files from a resin slicer. Send resin jobs from that slicer or the printer, then monitor or Stop them here.";
pub const RESIN_PAUSE: &str = "Pause and resume act on a job this companion started, and Print never starts resin jobs. Pause or resume on the printer, or use Stop.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Elegoo Centauri Carbon FDM, from community protocol notes.
    Centauri,
    /// CBD SDCP V3 resin printers, from the vendor specification.
    Resin,
}
impl Profile {
    fn parse(profile: Option<&str>) -> Option<Self> {
        match profile {
            Some("centauri") => Some(Self::Centauri),
            Some("resin") => Some(Self::Resin),
            _ => None,
        }
    }
}

/// Status freshness, polling, keepalive, and reconnect timing.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// A status report older than this is stale.
    pub fresh: Duration,
    /// Interval between status requests (command 0).
    pub poll: Duration,
    /// Interval between text `ping` keepalives.
    pub keepalive: Duration,
    /// A connection that delivers nothing for this long is closed and reopened.
    pub silence: Duration,
    /// First reconnect delay; it doubles up to 30 seconds.
    pub backoff: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            fresh: Duration::from_secs(30),
            poll: Duration::from_secs(10),
            keepalive: Duration::from_secs(20),
            silence: Duration::from_secs(45),
            backoff: Duration::from_secs(1),
        }
    }
}

pub(crate) fn controls(printer: &crate::config::Printer) -> Vec<&'static str> {
    if printer.profile.as_deref() == Some("resin") {
        vec!["stop"]
    } else {
        Vec::new()
    }
}

pub fn validate(config: &crate::config::Printer) -> Result<(), String> {
    if Profile::parse(config.profile.as_deref()).is_none() {
        return Err("Set profile to centauri or resin for each Elegoo SDCP printer.".into());
    }
    let host = config.host.as_deref().unwrap_or_default();
    let hostname = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        && !host.starts_with(['-', '.']);
    if !hostname && host.parse::<std::net::Ipv6Addr>().is_err() {
        return Err(
            "Set host to the Elegoo printer's hostname or IP address, without a scheme or path."
                .into(),
        );
    }
    if config.port == Some(0) {
        return Err("Set a printer port between 1 and 65535.".into());
    }
    if config.mainboard_id.as_ref().is_some_and(|id| {
        id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric())
    }) {
        return Err(
            "Set mainboard_id to the printer's MainboardID: at most 64 letters and digits.".into(),
        );
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
        || config.lan_control
        || config.resume_commissioned
        || !config.ams_mapping.is_empty()
        || config.slicer.is_some()
        || config.printer_profile.is_some()
        || config.print_profile.is_some()
        || config.filament_profile.is_some()
    {
        return Err("Set only host, profile, and optional port and mainboard_id for Elegoo SDCP printers. Print does not slice or send jobs to them, so leave slicer and slicer profiles unset.".into());
    }
    Ok(())
}

fn url(config: &crate::config::Printer, profile: Profile) -> String {
    let host = config.host.as_deref().unwrap_or_default();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    // The resin specification serves port 3030; the Centauri notes use the default port.
    match (profile, config.port) {
        (_, Some(port)) => format!("ws://{host}:{port}/websocket"),
        (Profile::Resin, None) => format!("ws://{host}:3030/websocket"),
        (Profile::Centauri, None) => format!("ws://{host}/websocket"),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
fn temperature(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|t| t.is_finite() && (-100.0..=1000.0).contains(t))
}
fn layers(info: Option<&Value>) -> Option<f64> {
    let info = info?;
    let current = info.get("CurrentLayer")?.as_u64()?;
    let total = info.get("TotalLayer")?.as_u64()?;
    (total > 0 && current <= total).then(|| current as f64 / total as f64)
}

/// The resin print substate names from the SDCP V3 `PrintInfo.Status` enumeration.
fn resin_substate(code: i64) -> Option<&'static str> {
    Some(match code {
        0 => "IDLE",
        1 => "HOMING",
        2 => "DESCENDING",
        3 => "EXPOSING",
        4 => "LIFTING",
        5 => "PAUSING",
        6 => "PAUSED",
        7 => "STOPPING",
        8 => "STOPPED",
        9 => "COMPLETED",
        10 => "FILE_CHECKING",
        _ => return None,
    })
}
fn resin_error(code: i64) -> &'static str {
    match code {
        1 => "file MD5 check failed",
        2 => "file read failed",
        3 => "resolution mismatch",
        4 => "format mismatch",
        5 => "machine model mismatch",
        _ => "unknown error",
    }
}
/// Print control acknowledgement codes from the SDCP V3 specification.
fn ack_meaning(code: i64) -> String {
    match code {
        1 => "busy".into(),
        2 => "file not found".into(),
        3 => "MD5 verification failed".into(),
        4 => "file read failed".into(),
        5 => "resolution mismatch".into(),
        6 => "unrecognized file format".into(),
        7 => "machine model mismatch".into(),
        _ => format!("code {code}"),
    }
}

/// Resin state from the machine activities and print substate. A retained COMPLETED or
/// STOPPED substate never overrides an active print activity.
fn resin(status: &Value) -> (PrinterState, String) {
    let unknown = |what: String| {
        (
            PrinterState::Error,
            format!("{what}; Unrecognised resin status. Read fresh status."),
        )
    };
    let activities = status
        .get("CurrentStatus")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().map(Value::as_i64).collect::<Option<Vec<_>>>())
        .filter(|a| !a.is_empty() && a.iter().all(|c| (0..=4).contains(c)));
    let Some(activities) = activities else {
        return unknown("UNKNOWN ACTIVITY".into());
    };
    let info = status.get("PrintInfo");
    let code = info.and_then(|i| i.get("Status")).and_then(Value::as_i64);
    let Some((code, label)) = code.and_then(|c| resin_substate(c).map(|l| (c, l))) else {
        return unknown(match code {
            Some(code) => format!("UNKNOWN PRINT STATUS {code}"),
            None => "UNKNOWN PRINT STATUS".into(),
        });
    };
    let error = info
        .and_then(|i| i.get("ErrorNumber"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if error != 0 {
        return (
            PrinterState::Error,
            format!("{label}; Error {error}: {}", resin_error(error)),
        );
    }
    if activities.contains(&1) {
        return match code {
            6 => (PrinterState::Paused, label.into()),
            0 | 8 | 9 => (PrinterState::Printing, "PRINTING".into()),
            _ => (PrinterState::Printing, label.into()),
        };
    }
    if let Some(activity) = activities.iter().find(|c| **c != 0) {
        let name = match activity {
            2 => "FILE_TRANSFERRING",
            3 => "EXPOSURE_TESTING",
            _ => "DEVICES_TESTING",
        };
        return (PrinterState::Busy, name.into());
    }
    match code {
        0 | 8 | 9 => (PrinterState::Idle, label.into()),
        _ => (
            PrinterState::Error,
            format!("{label}; The machine reports idle during an active print stage. Read fresh status."),
        ),
    }
}

/// Centauri state from `PrintInfo.Status` as observed in community notes. Resin meanings
/// never apply: 8 and 9 are preparation and start here, not stopped and completed.
fn centauri(status: &Value) -> (PrinterState, String) {
    let code = status
        .get("PrintInfo")
        .and_then(|i| i.get("Status"))
        .and_then(Value::as_i64);
    let (state, label) = match code {
        Some(0) => (PrinterState::Idle, "IDLE"),
        Some(5) => (PrinterState::Printing, "PAUSING"),
        Some(8) => (PrinterState::Printing, "PREPARING"),
        Some(9) => (PrinterState::Printing, "STARTING"),
        Some(13) => (PrinterState::Printing, "PRINTING"),
        Some(20) => (PrinterState::Printing, "RESUMING"),
        Some(10) => (PrinterState::Paused, "PAUSED"),
        Some(code) => {
            return (
                PrinterState::Error,
                format!("UNKNOWN STATUS {code}; Unrecognised Centauri status. Read fresh status."),
            )
        }
        None => {
            return (
                PrinterState::Error,
                "UNKNOWN STATUS; Unrecognised Centauri status. Read fresh status.".into(),
            )
        }
    };
    (state, label.into())
}

/// Maps one SDCP `Status` object to printer JSON, without freshness or bed clearance.
pub fn report(config: &crate::config::Printer, status: &Value) -> Printer {
    let profile = Profile::parse(config.profile.as_deref()).unwrap_or(Profile::Resin);
    let info = status.get("PrintInfo");
    let (state, mut detail) = match profile {
        Profile::Resin => resin(status),
        Profile::Centauri => centauri(status),
    };
    let active = matches!(state, PrinterState::Printing | PrinterState::Paused);
    let mut printer = Printer {
        controls: controls(config).into_iter().map(String::from).collect(),
        id: config.id.clone(),
        name: config.name.clone(),
        model: config.model.clone(),
        state,
        detail: None,
        needs_clearing: None,
        job_path: None,
        job_id: text(info.and_then(|i| i.get("TaskId"))),
        job_name: text(info.and_then(|i| i.get("Filename"))),
        progress: layers(info).filter(|_| active),
        seconds_left: None,
        nozzle_temperature: None,
        bed_temperature: None,
    };
    let degrees = |label: &str, key: &str| {
        temperature(status.get(key)).map(|t| format!("; {label} {t:.0} °C"))
    };
    match profile {
        Profile::Resin => {
            // Ticks are milliseconds in the resin specification.
            let ticks = |key: &str| info.and_then(|i| i.get(key)).and_then(Value::as_u64);
            printer.seconds_left = ticks("TotalTicks")
                .zip(ticks("CurrentTicks"))
                .map(|(total, current)| total.saturating_sub(current) / 1000)
                .filter(|_| active);
            // UV LED and enclosure temperatures are not nozzle or bed temperatures.
            detail.extend(degrees("UV LED", "TempOfUVLED"));
            detail.extend(degrees("box", "TempOfBox"));
        }
        Profile::Centauri => {
            // Tick units are undocumented for Centauri, so time left stays unavailable.
            printer.nozzle_temperature = temperature(status.get("TempOfNozzle"));
            printer.bed_temperature = temperature(status.get("TempOfHotbed"));
            detail.extend(degrees("box", "TempOfBox"));
        }
    }
    printer.detail = Some(detail);
    printer
}

#[derive(Clone)]
struct Telemetry {
    connected: bool,
    generation: u64,
    status: Option<Value>,
    received: Option<Instant>,
    mainboard: Option<String>,
    brand: String,
    reason: &'static str,
    changed: bool,
    observed: Option<Value>,
}
#[derive(Clone)]
enum Inbound {
    Response {
        request: String,
        mainboard: String,
        cmd: u64,
        ack: Option<i64>,
    },
    Lost,
}
struct Outgoing {
    text: String,
    generation: u64,
    sent: oneshot::Sender<Result<(), Error>>,
}
struct Link {
    url: String,
    profile: Profile,
    expected: Option<String>,
    timing: Timing,
    state: watch::Sender<Telemetry>,
    replies: broadcast::Sender<Inbound>,
}

pub struct Client {
    config: crate::config::Printer,
    profile: Profile,
    timing: Timing,
    state: watch::Sender<Telemetry>,
    replies: broadcast::Sender<Inbound>,
    commands: mpsc::Sender<Outgoing>,
    command_rx: Mutex<Option<mpsc::Receiver<Outgoing>>>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: std::sync::atomic::AtomicBool,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = self.worker.get_mut().take() {
            task.abort();
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn refuse(message: &str) -> Error {
    Error::Refused(message.into())
}
/// The SDCP request envelope. Resin follows the vendor specification; Centauri follows the
/// requests shown in its protocol notes, which carry no topic.
fn envelope(profile: Profile, cmd: u64, request: &str, mainboard: &str, brand: &str) -> Value {
    // From is 0 (local network client) in the specification; the notes' requests use 1.
    let from = match profile {
        Profile::Resin => 0,
        Profile::Centauri => 1,
    };
    let data = json!({
        "Cmd": cmd,
        "Data": {},
        "RequestID": request,
        "MainboardID": mainboard,
        "TimeStamp": now(),
        "From": from,
    });
    match profile {
        Profile::Resin => json!({
            "Id": brand,
            "Data": data,
            "Topic": format!("sdcp/request/{mainboard}"),
        }),
        Profile::Centauri => json!({"Id": brand, "Data": data}),
    }
}

impl Client {
    /// No vendor disconnect command is documented; closing the worker closes its socket.
    pub async fn shutdown(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut worker = self.worker.lock().await;
        if let Some(task) = worker.take() {
            task.abort();
            let _ = task.await;
        }
        self.state.send_modify(|t| {
            t.connected = false;
            t.received = None;
            t.reason = UNAVAILABLE;
        });
    }

    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        validate(&config)?;
        let profile = Profile::parse(config.profile.as_deref())
            .ok_or("Set profile to centauri or resin for each Elegoo SDCP printer.")?;
        let (state, _) = watch::channel(Telemetry {
            connected: false,
            generation: 0,
            status: None,
            received: None,
            mainboard: config.mainboard_id.clone(),
            brand: String::new(),
            reason: WAITING,
            changed: false,
            observed: None,
        });
        let (replies, _) = broadcast::channel(64);
        let (commands, command_rx) = mpsc::channel(4);
        Ok(Self {
            config,
            profile,
            timing: Timing::default(),
            state,
            replies,
            commands,
            command_rx: Mutex::new(Some(command_rx)),
            worker: Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }
    /// Replaces the timing before the first status read, for simulated printers.
    pub fn with_timing(mut self, timing: Timing) -> Self {
        let bound = |d: Duration| d.clamp(Duration::from_millis(10), Duration::from_secs(3600));
        self.timing = Timing {
            fresh: bound(timing.fresh),
            poll: bound(timing.poll),
            keepalive: bound(timing.keepalive),
            silence: bound(timing.silence),
            backoff: bound(timing.backoff),
        };
        self
    }
    pub fn profile(&self) -> Profile {
        self.profile
    }
    async fn begin(&self) {
        let mut worker = self.worker.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if worker.is_none() {
            let Some(commands) = self.command_rx.lock().await.take() else {
                return;
            };
            let link = Link {
                url: url(&self.config, self.profile),
                profile: self.profile,
                expected: self.config.mainboard_id.clone(),
                timing: self.timing,
                state: self.state.clone(),
                replies: self.replies.clone(),
            };
            *worker = Some(tokio::spawn(run(link, commands)));
        }
    }
    fn live(&self, t: &Telemetry) -> bool {
        t.connected
            && t.status.is_some()
            && t.received.is_some_and(|r| r.elapsed() < self.timing.fresh)
    }
    fn printer(&self, t: &Telemetry) -> Printer {
        match t.status.as_ref().filter(|_| self.live(t)) {
            Some(status) => {
                let mut printer = report(&self.config, status);
                printer.needs_clearing = Some(t.changed);
                printer
            }
            None => self.offline(if t.connected && t.status.is_some() {
                STALE
            } else {
                t.reason
            }),
        }
    }
    fn offline(&self, reason: &str) -> Printer {
        Printer {
            controls: controls(&self.config)
                .into_iter()
                .map(String::from)
                .collect(),
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state: PrinterState::Offline,
            detail: Some(reason.into()),
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
    /// Fresh status, waiting briefly for the first report on a new connection.
    pub async fn snapshot(&self) -> Printer {
        self.begin().await;
        let mut rx = self.state.subscribe();
        let _ = tokio::time::timeout(DEADLINE, async {
            loop {
                {
                    let t = rx.borrow_and_update();
                    if self.live(&t)
                        || (!t.connected && t.reason != WAITING)
                        || t.reason == OTHER_BOARD
                    {
                        break;
                    }
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        let t = rx.borrow().clone();
        self.printer(&t)
    }
    pub fn acknowledge(&self) {
        self.state.send_modify(|t| t.changed = false);
    }
    /// Centauri accepts no machine action at all.
    pub fn check_control(&self) -> Result<(), Error> {
        if controls(&self.config).contains(&"stop") {
            Ok(())
        } else {
            Err(refuse(MONITORING))
        }
    }
    /// Why preparation, Start, pause, resume, and cancel are unavailable.
    pub fn refusal(&self, action: &str) -> Error {
        match (self.profile, action) {
            (Profile::Centauri, _) => refuse(MONITORING),
            (Profile::Resin, "pause" | "resume") => refuse(RESIN_PAUSE),
            (Profile::Resin, _) => refuse(RESIN_FILES),
        }
    }

    /// Sends Stop (command 130) once for the confirmed live task and waits for STOPPED.
    pub async fn stop(&self, identity: &str, name: &str) -> Result<(), Error> {
        self.check_control()?;
        self.begin().await;
        let t = self.state.borrow().clone();
        let live = self.printer(&t);
        if !self.live(&t)
            || live.job_id.as_deref() != Some(identity)
            || live.job_name.as_deref() != Some(name)
            || !matches!(
                live.state,
                PrinterState::Printing | PrinterState::Paused | PrinterState::Attention
            )
        {
            return Err(refuse("The live job changed. Confirm Stop again."));
        }
        let mainboard = t
            .mainboard
            .clone()
            .ok_or_else(|| refuse("Printer identity is unknown. Read fresh status before Stop."))?;
        let mut rx = self.state.subscribe();
        self.command(STOP, "Stop", &mainboard, &t).await?;
        eprintln!("Stop is pending. Waiting for fresh status; do not repeat the command.");
        let deadline = Instant::now() + OUTCOME;
        loop {
            {
                let t = rx.borrow_and_update();
                if let Some(status) = t.status.as_ref().filter(|_| self.live(&t)) {
                    let info = status.get("PrintInfo");
                    let task = text(info.and_then(|i| i.get("TaskId")));
                    let file = text(info.and_then(|i| i.get("Filename")));
                    if task.as_deref().is_some_and(|task| task != identity) {
                        return Err(refuse(
                            "Live job changed while Stop was pending. Read fresh status.",
                        ));
                    }
                    let same = task.as_deref() == Some(identity)
                        || task.is_none() && file.as_deref() == Some(name);
                    let printing = status
                        .get("CurrentStatus")
                        .and_then(Value::as_array)
                        .is_none_or(|a| a.iter().any(|c| c.as_i64() != Some(0)));
                    let code = info.and_then(|i| i.get("Status")).and_then(Value::as_i64);
                    if same && !printing && code == Some(8) {
                        return Ok(());
                    }
                    if same && !printing && code == Some(9) {
                        return Err(refuse(
                            "The job completed before Stop was confirmed. Read fresh status.",
                        ));
                    }
                }
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => {
                    return Err(refuse(
                        "Stop is not confirmed after waiting. Read fresh status; do not repeat the command.",
                    ))
                }
            }
        }
    }

    /// Sends one request on the current connection and waits for its acknowledgement.
    /// Nothing is ever resent, including after a reconnect.
    async fn command(
        &self,
        cmd: u64,
        label: &str,
        mainboard: &str,
        t: &Telemetry,
    ) -> Result<(), Error> {
        let request = crate::new_job_id().map_err(Error::Failed)?;
        let text = envelope(self.profile, cmd, &request, mainboard, &t.brand).to_string();
        let mut replies = self.replies.subscribe();
        let (sent, done) = oneshot::channel();
        self.commands
            .try_send(Outgoing {
                text,
                generation: t.generation,
                sent,
            })
            .map_err(|_| Error::Unavailable)?;
        done.await.map_err(|_| Error::Unavailable)??;
        let lost = || {
            Error::Refused(format!(
                "{label} reply was lost. It was sent once and will not be resent; read fresh status before another action."
            ))
        };
        let ack = tokio::time::timeout(ACK, async {
            loop {
                match replies.recv().await {
                    Ok(Inbound::Response {
                        request: id,
                        mainboard: board,
                        cmd: code,
                        ack,
                    }) if id == request && board == mainboard && code == cmd => return Ok(ack),
                    Ok(Inbound::Lost) | Err(broadcast::error::RecvError::Closed) => {
                        return Err(lost())
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| lost())??;
        match ack {
            Some(0) => Ok(()),
            Some(code) => Err(Error::Refused(format!(
                "The printer refused {label}: {}. Read fresh status before another action.",
                ack_meaning(code)
            ))),
            None => Err(lost()),
        }
    }
}

/// One connection at a time; after any reconnect, status stays unavailable until a new report.
async fn run(link: Link, mut commands: mpsc::Receiver<Outgoing>) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let mut backoff = link.timing.backoff;
    loop {
        let attempt = tokio::time::timeout(
            DEADLINE,
            tokio_tungstenite::connect_async_with_config(link.url.as_str(), Some(config), true),
        )
        .await;
        if let Ok(Ok((socket, _))) = attempt {
            if serve(&link, socket, &mut commands).await {
                backoff = link.timing.backoff;
            }
            let _ = link.replies.send(Inbound::Lost);
            // A reconnect withdraws clearance and freshness even between status reads.
            link.state.send_modify(|t| {
                t.connected = false;
                t.status = None;
                t.received = None;
                t.changed = true;
                t.reason = LOST;
            });
        } else {
            link.state.send_modify(|t| {
                t.connected = false;
                if t.reason == WAITING {
                    t.reason = UNAVAILABLE;
                }
            });
        }
        while let Ok(command) = commands.try_recv() {
            let _ = command.sent.send(Err(refuse(
                "The printer connection was lost before the command was sent. It was not sent; read fresh status.",
            )));
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Returns whether the connection delivered a status report.
async fn serve(link: &Link, mut socket: Socket, commands: &mut mpsc::Receiver<Outgoing>) -> bool {
    let mut generation = 0;
    link.state.send_modify(|t| {
        t.generation = t.generation.wrapping_add(1);
        generation = t.generation;
        t.connected = true;
        t.status = None;
        t.received = None;
    });
    let mut poll = tokio::time::interval(link.timing.poll);
    let mut keepalive = tokio::time::interval_at(
        Instant::now() + link.timing.keepalive,
        link.timing.keepalive,
    );
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heard = Instant::now();
    let mut reported = false;
    loop {
        tokio::select! {
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(frame))) => {
                    heard = Instant::now();
                    reported |= receive(link, frame.as_str());
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return reported,
                Some(Ok(_)) => heard = Instant::now(),
            },
            Some(command) = commands.recv() => {
                let current = {
                    let t = link.state.borrow();
                    t.connected
                        && t.generation == generation
                        && command.generation == generation
                        && t.received.is_some_and(|r| r.elapsed() < link.timing.fresh)
                };
                if !current {
                    let _ = command.sent.send(Err(refuse(
                        "The printer connection changed after you confirmed. Nothing was sent; read fresh status and confirm again.",
                    )));
                    continue;
                }
                if socket.send(Message::text(command.text)).await.is_err() {
                    let _ = command.sent.send(Err(refuse(
                        "The connection failed while sending the command. It may have reached the printer and will not be resent; read fresh status.",
                    )));
                    return reported;
                }
                let _ = command.sent.send(Ok(()));
            }
            _ = poll.tick() => {
                let (mainboard, brand) = {
                    let t = link.state.borrow();
                    (t.mainboard.clone().unwrap_or_default(), t.brand.clone())
                };
                let request = crate::new_job_id().unwrap_or_default();
                let message = envelope(link.profile, STATUS, &request, &mainboard, &brand);
                if socket.send(Message::text(message.to_string())).await.is_err() {
                    return reported;
                }
            }
            _ = keepalive.tick() => {
                if socket.send(Message::text("ping")).await.is_err() {
                    return reported;
                }
            }
            () = tokio::time::sleep_until(heard + link.timing.silence) => return reported,
        }
    }
}

/// Handles one text frame and returns whether it was an accepted status report.
fn receive(link: &Link, frame: &str) -> bool {
    if frame == "pong" {
        return false;
    }
    let Ok(message) = serde_json::from_str::<Value>(frame) else {
        return false;
    };
    let topic = message
        .get("Topic")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(status) = message.get("Status").filter(|s| s.is_object()) {
        if !topic.is_empty() && !topic.starts_with("sdcp/status/") {
            return false;
        }
        let board = text(message.get("MainboardID"));
        if link.expected.is_some() && board != link.expected {
            link.state.send_modify(|t| t.reason = OTHER_BOARD);
            return false;
        }
        let info = status.get("PrintInfo");
        let observed = json!([
            status.get("CurrentStatus"),
            info.and_then(|i| i.get("Status")),
            info.and_then(|i| i.get("TaskId")),
            info.and_then(|i| i.get("Filename")),
            info.and_then(|i| i.get("ErrorNumber")),
        ]);
        link.state.send_modify(|t| {
            if t.observed.as_ref().is_some_and(|old| old != &observed) {
                t.changed = true;
            }
            t.observed = Some(observed);
            if board.is_some() {
                t.mainboard = board;
            }
            t.status = Some(status.clone());
            t.received = Some(Instant::now());
        });
        return true;
    }
    let Some(data) = message.get("Data") else {
        return false;
    };
    let (Some(request), Some(cmd)) = (
        text(data.get("RequestID")),
        data.get("Cmd").and_then(Value::as_u64),
    ) else {
        return false;
    };
    if !topic.is_empty() && !topic.starts_with("sdcp/response/") {
        return false;
    }
    if let Some(brand) = text(message.get("Id")).filter(|b| b.len() <= 64) {
        link.state.send_if_modified(|t| {
            let changed = t.brand != brand;
            t.brand = brand;
            changed
        });
    }
    let _ = link.replies.send(Inbound::Response {
        request,
        mainboard: text(data.get("MainboardID")).unwrap_or_default(),
        cmd,
        ack: data
            .get("Data")
            .and_then(|d| d.get("Ack"))
            .and_then(Value::as_i64),
    });
    false
}
