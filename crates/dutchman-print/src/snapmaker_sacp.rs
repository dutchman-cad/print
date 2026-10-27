//! SACP over TCP for Snapmaker J1 and Artisan, from Snapmaker's published Packet, Types,
//! System Commands, and Modules documentation. Only the module-info (0x01/0x20) and
//! machine-info (0x01/0x21) requests are ever sent; the 3D-printing commands are unpublished.

use super::{offline, plain, report};
use crate::backend::{Printer, PrinterState};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{watch, Mutex};
use tokio::time::Instant;

pub const PORT: u16 = 8888;
/// Peer IDs from Packet.md.
pub const SOFTWARE: u8 = 0;
pub const CONTROLLER: u8 = 1;
/// Attribute values from Packet.md.
pub const REQUEST: u8 = 0;
pub const ACK: u8 = 1;
pub const SYSTEM: u8 = 0x01;
pub const MODULE_INFO: u8 = 0x20;
pub const MACHINE_INFO: u8 = 0x21;
const SOF: [u8; 2] = [0xAA, 0x55];
const VERSION: u8 = 0x01;
/// Sender, attribute, sequence, command set, command ID, and the payload checksum.
const MIN_BODY: usize = 8;
const DEADLINE: Duration = Duration::from_secs(4);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

const WAITING: &str = "Waiting for a Snapmaker SACP reply.";
const UNAVAILABLE: &str =
    "Snapmaker connection unavailable. Check the printer's host, port, and network.";
const LOST: &str =
    "Snapmaker connection lost. Reconnecting; status is not fresh until a new reply arrives.";
const STALE: &str = "Snapmaker status is stale. Wait for fresh replies.";
pub const NO_REPLY: &str = "Connected, but the printer has not answered the SACP module-info request. Snapmaker has not published how J1 and Artisan printers pair with network clients, so Print cannot complete a pairing step; check the touchscreen for a connection request.";
pub const NOT_PUBLISHED: &str =
    "Snapmaker has not published the print state, so Print never reports idle or printing.";
pub const NOT_A_PRINTER: &str =
    "No printing module is installed, so this machine is not a printer right now.";

/// The CRC-8 of the header's first six bytes: polynomial 0x07, initial value 0, no reflection.
pub fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |crc, byte| {
        (0..8).fold(crc ^ byte, |c, _| {
            if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            }
        })
    })
}

/// The TCP/IP (RFC 1071) checksum over little-endian 16-bit words; an odd last byte is the
/// low byte of its word.
pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for pair in bytes.chunks(2) {
        sum += u32::from(match *pair {
            [low, high] => u16::from_le_bytes([low, high]),
            [low] => u16::from(low),
            _ => 0,
        });
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub receiver: u8,
    pub sender: u8,
    pub attribute: u8,
    pub sequence: u16,
    pub set: u8,
    pub id: u8,
    pub payload: Vec<u8>,
}
impl Packet {
    /// A request from this software to the controller, with no payload.
    pub fn request(sequence: u16, set: u8, id: u8) -> Self {
        Self {
            receiver: CONTROLLER,
            sender: SOFTWARE,
            attribute: REQUEST,
            sequence,
            set,
            id,
            payload: Vec::new(),
        }
    }
    /// The wire bytes. Bytes 2-3 count the bytes from byte 7 to the end of the packet.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let length = u16::try_from(self.payload.len().checked_add(MIN_BODY)?).ok()?;
        let mut bytes = Vec::with_capacity(usize::from(length) + 7);
        bytes.extend_from_slice(&SOF);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.push(VERSION);
        bytes.push(self.receiver);
        bytes.push(crc8(&bytes));
        bytes.push(self.sender);
        bytes.push(self.attribute);
        bytes.extend_from_slice(&self.sequence.to_le_bytes());
        bytes.push(self.set);
        bytes.push(self.id);
        bytes.extend_from_slice(&self.payload);
        let sum = checksum(bytes.get(7..)?);
        bytes.extend_from_slice(&sum.to_le_bytes());
        Some(bytes)
    }
}

/// Reassembles packets from a byte stream. A packet is returned only once it is complete
/// and both its header CRC-8 and payload checksum validate; bytes that fail are skipped.
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    rejected: usize,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }
    /// Bytes discarded so far as noise or as part of a corrupt packet.
    pub fn rejected(&self) -> usize {
        self.rejected
    }
    /// Bytes held while waiting for the rest of a packet.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
    fn discard(&mut self, count: usize) {
        let count = count.min(self.buffer.len());
        self.buffer.drain(..count);
        self.rejected += count;
    }
    pub fn next_packet(&mut self) -> Option<Packet> {
        loop {
            match self.buffer.windows(2).position(|w| w == SOF) {
                Some(0) => {}
                Some(start) => self.discard(start),
                None => {
                    // A trailing 0xAA may begin the next packet.
                    let keep = usize::from(self.buffer.last() == Some(&0xAA));
                    self.discard(self.buffer.len() - keep);
                    return None;
                }
            }
            let &[_, _, low, high, version, receiver, crc] = self.buffer.get(..7)? else {
                return None;
            };
            let length = usize::from(u16::from_le_bytes([low, high]));
            let header = self.buffer.get(..6).map(crc8);
            if header != Some(crc) || version != VERSION || length < MIN_BODY {
                self.discard(1);
                continue;
            }
            let total = 7 + length;
            let packet = self.buffer.get(..total)?;
            let (Some(body), Some(&[sum_low, sum_high])) =
                (packet.get(7..total - 2), packet.get(total - 2..))
            else {
                return None;
            };
            if checksum(body) != u16::from_le_bytes([sum_low, sum_high]) {
                self.discard(1);
                continue;
            }
            let &[sender, attribute, seq_low, seq_high, set, id, ref payload @ ..] = body else {
                self.discard(1);
                continue;
            };
            let packet = Packet {
                receiver,
                sender,
                attribute,
                sequence: u16::from_le_bytes([seq_low, seq_high]),
                set,
                id,
                payload: payload.to_vec(),
            };
            self.buffer.drain(..total);
            return Some(packet);
        }
    }
}

/// Little-endian fields as defined in Types.md.
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(count)?;
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }
    fn u16(&mut self) -> Option<u16> {
        match *self.take(2)? {
            [low, high] => Some(u16::from_le_bytes([low, high])),
            _ => None,
        }
    }
    fn u32(&mut self) -> Option<u32> {
        match *self.take(4)? {
            [a, b, c, d] => Some(u32::from_le_bytes([a, b, c, d])),
            _ => None,
        }
    }
    /// A UInt16 byte length followed by the content.
    fn string(&mut self) -> Option<String> {
        let length = usize::from(self.u16()?);
        Some(String::from_utf8_lossy(self.take(length)?).into_owned())
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

/// One entry of the module-info list, in the field order of System-Commands.md.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInfo {
    pub key: u8,
    pub module_id: u16,
    pub index: u8,
    pub state: u8,
    pub serial: u32,
    pub hardware: u8,
    pub firmware: String,
}
fn module(r: &mut Reader<'_>) -> Option<ModuleInfo> {
    Some(ModuleInfo {
        key: r.u8()?,
        module_id: r.u16()?,
        index: r.u8()?,
        state: r.u8()?,
        serial: r.u32()?,
        hardware: r.u8()?,
        firmware: r.string()?,
    })
}

/// Parses a module-info acknowledgement. The documentation does not define how an
/// `Array<ModuleInfo>` is counted, so the list is read with a UInt8 count, a UInt16 count,
/// and no count; every reading that consumes the payload exactly must agree, or the reply
/// is refused.
pub fn parse_modules(payload: &[u8]) -> Result<Vec<ModuleInfo>, String> {
    let mut r = Reader(payload);
    let result = r.u8().ok_or("the module-info reply has no result byte")?;
    if result != 0 {
        return Err(format!(
            "the printer answered module info with result {result}"
        ));
    }
    let rest = r.0;
    let counted = |wide: bool| {
        let mut r = Reader(rest);
        let count = if wide {
            usize::from(r.u16()?)
        } else {
            usize::from(r.u8()?)
        };
        let list = (0..count)
            .map(|_| module(&mut r))
            .collect::<Option<Vec<_>>>()?;
        r.done().then_some(list)
    };
    let open = || {
        let mut r = Reader(rest);
        let mut list = Vec::new();
        while !r.done() {
            list.push(module(&mut r)?);
        }
        Some(list)
    };
    let readings: Vec<_> = [counted(false), counted(true), open()]
        .into_iter()
        .flatten()
        .collect();
    match readings.split_first() {
        Some((first, others)) if others.iter().all(|o| o == first) => Ok(first.clone()),
        Some(_) => Err("the module list can be read in more than one way".into()),
        None => Err("the module list does not match the documented fields".into()),
    }
}

/// The machine-info acknowledgement, in the field order of System-Commands.md.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineInfo {
    pub model: u8,
    pub hardware: u8,
    pub firmware: String,
}
pub fn parse_machine(payload: &[u8]) -> Result<MachineInfo, String> {
    let mut r = Reader(payload);
    let unreadable = || "the machine-info reply does not match the documented fields".to_owned();
    let result = r.u8().ok_or_else(unreadable)?;
    if result != 0 {
        return Err(format!(
            "the printer answered machine info with result {result}"
        ));
    }
    let model = r.u8().ok_or_else(unreadable)?;
    let hardware = r.u8().ok_or_else(unreadable)?;
    // The internal hardware serial and the warranty serial number are not shown.
    r.u32().ok_or_else(unreadable)?;
    let firmware = r.string().ok_or_else(unreadable)?;
    r.string().ok_or_else(unreadable)?;
    if !r.done() {
        return Err(unreadable());
    }
    Ok(MachineInfo {
        model,
        hardware,
        firmware,
    })
}

/// Machine model IDs from System-Commands.md.
pub fn machine_name(model: u8) -> Option<&'static str> {
    Some(match model {
        0 => "Snapmaker A150",
        1 => "Snapmaker A250",
        2 => "Snapmaker A350",
        3 => "Snapmaker Artisan",
        4 => "Snapmaker J1",
        5 => "Snapmaker Ray",
        _ => return None,
    })
}
/// Module IDs from Modules.md.
pub fn module_name(id: u16) -> Option<&'static str> {
    Some(match id {
        0 => "Printing Module",
        1 => "CNC Module",
        2 => "1.6W Laser Module",
        7 => "Air Purifier Module",
        13 => "Dual Extrusion Printing Module",
        14 => "10W Laser Module",
        16 => "Snapmaker Artisan Enclosure",
        19 => "20W Laser Module",
        20 => "40W Laser Module",
        518 => "Snapmaker Ray Enclosure",
        _ => return None,
    })
}
/// Modules.md names IDs 0 and 13 as printing modules.
pub fn printing_module(id: u16) -> bool {
    matches!(id, 0 | 13)
}

/// Maps a fresh module list to printer JSON. Without a published print-state contract, a
/// printing module reads `busy`, and anything else reads `attention`.
pub fn status(
    config: &crate::config::Printer,
    modules: &[ModuleInfo],
    machine: Option<&MachineInfo>,
) -> Printer {
    let mut parts: Vec<String> = Vec::new();
    let printing = modules.iter().any(|m| printing_module(m.module_id));
    parts.push(
        if printing {
            "PRINTING MODULE"
        } else {
            "NO PRINTING MODULE"
        }
        .into(),
    );
    for m in modules.iter().take(16) {
        let name = match module_name(m.module_id) {
            Some(name) => format!("{name} (ID {})", m.module_id),
            None => format!("unknown module ID {}", m.module_id),
        };
        let firmware = plain(&m.firmware, 32);
        parts.push(if firmware.trim().is_empty() {
            name
        } else {
            format!("{name}, firmware {firmware}")
        });
    }
    if let Some(machine) = machine {
        let name = machine_name(machine.model)
            .map_or_else(|| format!("machine model {}", machine.model), str::to_owned);
        let firmware = plain(&machine.firmware, 32);
        parts.push(if firmware.trim().is_empty() {
            name
        } else {
            format!("{name}, firmware {firmware}")
        });
    }
    let (state, note) = if printing {
        (PrinterState::Busy, NOT_PUBLISHED)
    } else {
        (PrinterState::Attention, NOT_A_PRINTER)
    };
    parts.push(note.into());
    report(config, state, parts.join("; "))
}

/// Freshness, polling, and reconnect timing.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// A module list older than this is stale.
    pub fresh: Duration,
    /// Interval between module-info and machine-info requests.
    pub poll: Duration,
    /// How long a new connection waits for its first reply before it reads as unanswered.
    pub reply: Duration,
    /// Deadline for opening the TCP connection.
    pub connect: Duration,
    /// First reconnect delay; it doubles up to 30 seconds.
    pub backoff: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            fresh: Duration::from_secs(30),
            poll: Duration::from_secs(10),
            reply: Duration::from_secs(3),
            connect: Duration::from_secs(4),
            backoff: Duration::from_secs(1),
        }
    }
}

#[derive(Clone)]
struct Telemetry {
    connected: bool,
    received: Option<Instant>,
    modules: Option<Result<Vec<ModuleInfo>, String>>,
    machine: Option<MachineInfo>,
    reason: &'static str,
}
struct Link {
    host: String,
    port: u16,
    timing: Timing,
    state: watch::Sender<Telemetry>,
}

pub struct Client {
    config: crate::config::Printer,
    timing: Timing,
    state: watch::Sender<Telemetry>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(task) = self.worker.get_mut().take() {
            task.abort();
        }
    }
}
impl Client {
    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        super::validate(&config)?;
        let (state, _) = watch::channel(Telemetry {
            connected: false,
            received: None,
            modules: None,
            machine: None,
            reason: WAITING,
        });
        Ok(Self {
            config,
            timing: Timing::default(),
            state,
            worker: Mutex::new(None),
        })
    }
    /// Replaces the timing before the first status read, for simulated printers.
    pub fn with_timing(mut self, timing: Timing) -> Self {
        let bound = |d: Duration| d.clamp(Duration::from_millis(10), Duration::from_secs(3600));
        self.timing = Timing {
            fresh: bound(timing.fresh),
            poll: bound(timing.poll),
            reply: bound(timing.reply),
            connect: bound(timing.connect),
            backoff: bound(timing.backoff),
        };
        self
    }
    async fn begin(&self) {
        let mut worker = self.worker.lock().await;
        if worker.is_none() {
            let link = Link {
                host: self.config.host.clone().unwrap_or_default(),
                port: self.config.port.unwrap_or(PORT),
                timing: self.timing,
                state: self.state.clone(),
            };
            *worker = Some(tokio::spawn(run(link)));
        }
    }
    fn live(&self, t: &Telemetry) -> bool {
        t.connected
            && t.modules.is_some()
            && t.received.is_some_and(|r| r.elapsed() < self.timing.fresh)
    }
    fn printer(&self, t: &Telemetry) -> Printer {
        match t.modules.as_ref().filter(|_| self.live(t)) {
            Some(Ok(modules)) => status(&self.config, modules, t.machine.as_ref()),
            Some(Err(error)) => report(
                &self.config,
                PrinterState::Error,
                format!("UNREADABLE MODULES; {error}. Read fresh status."),
            ),
            None if t.connected && t.received.is_some() => offline(&self.config, STALE),
            None => offline(&self.config, t.reason),
        }
    }
    /// Fresh status, waiting briefly for the first reply on a new connection.
    pub async fn snapshot(&self) -> Printer {
        self.begin().await;
        let mut rx = self.state.subscribe();
        let _ = tokio::time::timeout(DEADLINE, async {
            loop {
                {
                    let t = rx.borrow_and_update();
                    if self.live(&t) || t.reason != WAITING {
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
}

/// One connection at a time; after any reconnect, status stays unavailable until a new reply.
async fn run(link: Link) {
    let mut sequence: u16 = 0;
    let mut backoff = link.timing.backoff;
    loop {
        let attempt = tokio::time::timeout(
            link.timing.connect,
            tokio::net::TcpStream::connect((link.host.as_str(), link.port)),
        )
        .await;
        if let Ok(Ok(stream)) = attempt {
            if serve(&link, stream, &mut sequence).await {
                backoff = link.timing.backoff;
            }
            link.state.send_modify(|t| {
                t.connected = false;
                t.received = None;
                t.modules = None;
                t.machine = None;
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
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
    }
}

/// Returns whether the connection delivered a module list.
async fn serve(link: &Link, mut stream: tokio::net::TcpStream, sequence: &mut u16) -> bool {
    link.state.send_modify(|t| {
        t.connected = true;
        t.received = None;
        t.modules = None;
        t.machine = None;
        t.reason = WAITING;
    });
    let unanswered = Instant::now() + link.timing.reply;
    let mut decoder = Decoder::default();
    // Only the requests of the latest round can be answered; older sequences are ignored.
    let mut outstanding: Vec<(u16, u8)> = Vec::new();
    let mut poll = tokio::time::interval(link.timing.poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut answered = false;
    let mut marked = false;
    let mut buffer = [0u8; 4096];
    loop {
        tokio::select! {
            read = stream.read(&mut buffer) => match read {
                Ok(0) | Err(_) => return answered,
                Ok(count) => {
                    decoder.push(buffer.get(..count).unwrap_or_default());
                    while let Some(packet) = decoder.next_packet() {
                        answered |= apply(link, &mut outstanding, packet);
                    }
                    // A peer that streams bytes which never validate is not a SACP printer.
                    if decoder.buffered() > 1 << 17 {
                        return answered;
                    }
                }
            },
            _ = poll.tick() => {
                outstanding.clear();
                for id in [MODULE_INFO, MACHINE_INFO] {
                    *sequence = sequence.wrapping_add(1);
                    let Some(bytes) = Packet::request(*sequence, SYSTEM, id).encode() else {
                        return answered;
                    };
                    if stream.write_all(&bytes).await.is_err() {
                        return answered;
                    }
                    outstanding.push((*sequence, id));
                }
            }
            () = tokio::time::sleep_until(unanswered), if !answered && !marked => {
                marked = true;
                link.state.send_modify(|t| {
                    if t.received.is_none() {
                        t.reason = NO_REPLY;
                    }
                });
            }
        }
    }
}

/// Applies an acknowledgement only to the outstanding request with the same sequence,
/// command set, and command ID. Returns whether it delivered a module list.
fn apply(link: &Link, outstanding: &mut Vec<(u16, u8)>, packet: Packet) -> bool {
    if packet.attribute != ACK || packet.receiver != SOFTWARE || packet.set != SYSTEM {
        return false;
    }
    let Some(at) = outstanding
        .iter()
        .position(|&(sequence, id)| sequence == packet.sequence && id == packet.id)
    else {
        return false;
    };
    outstanding.remove(at);
    match packet.id {
        MODULE_INFO => {
            let modules = parse_modules(&packet.payload);
            link.state.send_modify(|t| {
                t.modules = Some(modules);
                t.received = Some(Instant::now());
            });
            true
        }
        _ => {
            let machine = parse_machine(&packet.payload).ok();
            link.state.send_modify(|t| t.machine = machine);
            false
        }
    }
}
