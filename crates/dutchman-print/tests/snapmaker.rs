//! Snapmaker 2.0 (HTTP) and J1/Artisan (SACP) monitoring, exercised only against byte
//! fixtures and test-owned loopback servers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "helpers/snapmaker.rs"]
mod fake;
#[path = "helpers/slicer.rs"]
mod slicer_fixture;
use axum::{body::Body, http::StatusCode, Router};
use dutchman_print::{
    backend::{Backend, Error, Options, Printer, PrinterState},
    printers::Printers,
    safety::Safety,
    snapmaker::{
        http::{self, Machine},
        sacp::{self, Decoder, MachineInfo, ModuleInfo, Packet},
        MONITORING_2, MONITORING_J1,
    },
};
use fake::{Approval, Fake2, FakeJ1, Mode, TOKEN};
use serde_json::{json, Value};
use std::{future::Future, sync::Arc, time::Duration};
use tower::ServiceExt as _;

async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}
fn refused(result: Result<impl std::fmt::Debug, Error>, message: &str) {
    match result {
        Err(Error::Refused(text)) => assert!(text.contains(message), "{text}"),
        other => panic!("expected refusal containing {message:?}, got {other:?}"),
    }
}
fn detail(p: &Printer) -> &str {
    p.detail.as_deref().unwrap_or_default()
}
fn base() -> dutchman_print::config::Printer {
    dutchman_print::config::Printer {
        id: "test".into(),
        name: "Test".into(),
        model: "Test".into(),
        ..Default::default()
    }
}
async fn send(app: &Router, request: axum::http::Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (code, String::from_utf8_lossy(&bytes).into_owned())
}
async fn post(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    let (code, text) = send(
        app,
        axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    (code, serde_json::from_str(&text).unwrap_or(Value::Null))
}
fn router(config: dutchman_print::config::Config, dir: &std::path::Path) -> Router {
    let mut settings = dutchman_print::ServerConfig::default();
    settings.storage.data_dir = dir.join("api");
    settings.storage.min_free = 0;
    std::fs::create_dir_all(&settings.storage.data_dir).unwrap();
    dutchman_print::router(Arc::new(Printers::new(config).unwrap()), settings)
}

/// Every challenge, action route, and backend call is refused with the vendor-contract
/// message, and the printer reads as `expected` throughout.
async fn refuses_every_action(
    printer: dutchman_print::config::Printer,
    message: &str,
    expected: PrinterState,
) {
    let dir = tempfile::tempdir().unwrap();
    let app = router(fake::config(printer.clone()), dir.path());
    let (code, body) = send(
        &app,
        axum::http::Request::builder()
            .uri("/api/v1/printers")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(
        body.contains(&format!("\"state\":\"{expected}\"")),
        "{body}"
    );
    for action in ["start", "cancel", "stop", "pause", "resume"] {
        let (code, body) = post(
            &app,
            "/api/v1/confirmations",
            json!({"action": action, "printer": "test"}),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{action}: {body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("monitoring only"), "{action}: {error}");
        assert!(error.contains("not published"), "{action}: {error}");
    }
    for route in ["stop", "pause", "resume"] {
        let (code, body) = post(
            &app,
            &format!("/api/v1/printers/test/{route}"),
            json!({"confirmation": "0".repeat(64)}),
        )
        .await;
        assert!(code.is_client_error(), "{route}: {code} {body}");
    }
    let input = slicer_fixture::input(dir.path(), |_| Vec::new());
    let (code, text) = send(
        &app,
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/jobs?printer=test")
            .header("Content-Type", "model/3mf")
            .body(Body::from(std::fs::read(&input).unwrap()))
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{text}");
    assert!(text.contains("monitoring only"), "{text}");

    let b = Safety::new(Arc::new(Printers::new(fake::config(printer)).unwrap()));
    assert_eq!(b.printer("test").await.unwrap().state, expected);
    refused(b.check_control("test"), message);
    refused(
        b.prepare("one", &input, "test", Options::default()).await,
        message,
    );
    assert!(matches!(b.start("one").await, Err(Error::NotFound)));
    assert!(matches!(b.cancel("one").await, Err(Error::NotFound)));
    refused(b.stop("test", "job", "part.gcode", None).await, message);
    refused(b.pause("test").await, message);
    refused(b.resume("test").await, message);
}

// SACP packets, checked against bytes computed by hand from Packet.md and Types.md.

const MODULE_REQUEST: [u8; 15] = [
    0xAA, 0x55, 0x08, 0x00, 0x01, 0x01, 0x7F, 0x00, 0x00, 0x01, 0x00, 0x01, 0x20, 0xFD, 0xDF,
];
const MACHINE_REQUEST: [u8; 15] = [
    0xAA, 0x55, 0x08, 0x00, 0x01, 0x01, 0x7F, 0x00, 0x00, 0x02, 0x00, 0x01, 0x21, 0xFC, 0xDE,
];
/// Module-info ACK, sequence 1: result 0, one Printing Module (key 5, ID 0, index 0, state 1,
/// serial 0x12345678, hardware 2, firmware "v1.0.2").
const MODULE_ACK: [u8; 35] = [
    0xAA, 0x55, 0x1C, 0x00, 0x01, 0x00, 0x47, 0x01, 0x01, 0x01, 0x00, 0x01, 0x20, 0x00, 0x01, 0x05,
    0x00, 0x00, 0x00, 0x01, 0x78, 0x56, 0x34, 0x12, 0x02, 0x06, 0x00, 0x76, 0x31, 0x2E, 0x30, 0x2E,
    0x32, 0xB5, 0x9B,
];
fn printing_module() -> ModuleInfo {
    ModuleInfo {
        key: 5,
        module_id: 0,
        index: 0,
        state: 1,
        serial: 0x1234_5678,
        hardware: 2,
        firmware: "v1.0.2".into(),
    }
}

#[test]
fn sacp_checksums_and_requests_match_byte_fixtures() {
    // The CRC-8 (polynomial 0x07) catalogue check value.
    assert_eq!(sacp::crc8(b"123456789"), 0xF4);
    // RFC 1071's worked example sums to 0xDDF2; its complement goes on the wire as 22 0D.
    assert_eq!(
        sacp::checksum(&[0x00, 0x01, 0xF2, 0x03, 0xF4, 0xF5, 0xF6, 0xF7]).to_le_bytes(),
        [0x22, 0x0D]
    );
    assert_eq!(sacp::checksum(&[0x01]), !0x0001);
    let request = Packet::request(1, sacp::SYSTEM, sacp::MODULE_INFO);
    assert_eq!(request.encode().unwrap(), MODULE_REQUEST);
    assert_eq!(
        Packet::request(2, sacp::SYSTEM, sacp::MACHINE_INFO)
            .encode()
            .unwrap(),
        MACHINE_REQUEST
    );
    let mut decoder = Decoder::default();
    decoder.push(&MODULE_ACK);
    let ack = decoder.next_packet().unwrap();
    assert_eq!(
        (ack.receiver, ack.sender, ack.attribute, ack.sequence),
        (sacp::SOFTWARE, sacp::CONTROLLER, sacp::ACK, 1)
    );
    assert_eq!((ack.set, ack.id), (sacp::SYSTEM, sacp::MODULE_INFO));
    assert_eq!(
        sacp::parse_modules(&ack.payload),
        Ok(vec![printing_module()])
    );
    assert_eq!(decoder.next_packet(), None);
    assert_eq!((decoder.rejected(), decoder.buffered()), (0, 0));
    // Re-encoding the decoded packet reproduces the fixture.
    assert_eq!(ack.encode().unwrap(), MODULE_ACK);
}

#[test]
fn sacp_fragments_wait_until_the_whole_packet_validates() {
    let mut decoder = Decoder::default();
    for (at, byte) in MODULE_ACK.iter().enumerate() {
        assert_eq!(decoder.next_packet(), None, "after {at} bytes");
        decoder.push(&[*byte]);
    }
    assert_eq!(decoder.next_packet().unwrap().sequence, 1);
    assert_eq!(decoder.rejected(), 0);

    // Two packets in one read, split across a third.
    let mut stream = MODULE_REQUEST.to_vec();
    stream.extend_from_slice(&MACHINE_REQUEST);
    stream.extend_from_slice(&MODULE_ACK[..20]);
    decoder.push(&stream);
    assert_eq!(decoder.next_packet().unwrap().id, sacp::MODULE_INFO);
    assert_eq!(decoder.next_packet().unwrap().id, sacp::MACHINE_INFO);
    assert_eq!(decoder.next_packet(), None);
    decoder.push(&MODULE_ACK[20..]);
    assert_eq!(decoder.next_packet().unwrap().id, sacp::MODULE_INFO);
    assert_eq!(decoder.rejected(), 0);
}

#[test]
fn sacp_corrupt_packets_are_never_returned() {
    let corrupt = |at: usize| {
        let mut bytes = MODULE_ACK;
        bytes[at] ^= 0x01;
        bytes
    };
    // Header CRC, length, version, payload, and payload checksum damage.
    for at in [6, 2, 4, 5, 20, 33, 34] {
        let mut decoder = Decoder::default();
        decoder.push(&corrupt(at));
        assert_eq!(decoder.next_packet(), None, "byte {at}");
        assert!(
            decoder.rejected() > 0 || decoder.buffered() > 0,
            "byte {at}"
        );
        // A valid packet after the damage is still found, and only it.
        decoder.push(&MODULE_ACK);
        decoder.push(&MODULE_ACK);
        let mut found = 0;
        while let Some(packet) = decoder.next_packet() {
            assert_eq!(packet.payload, MODULE_ACK[13..33], "byte {at}");
            found += 1;
        }
        assert!((1..=2).contains(&found), "byte {at}: {found}");
    }
    // Noise and a lone start byte before the packet are skipped.
    let mut decoder = Decoder::default();
    decoder.push(&[0x00, 0x55, 0xAA, 0xAA, 0x13]);
    assert_eq!(decoder.next_packet(), None);
    decoder.push(&MODULE_ACK);
    assert_eq!(decoder.next_packet().unwrap().sequence, 1);
    assert_eq!(decoder.rejected(), 5);
    // A header announcing fewer bytes than the fixed fields is refused.
    let mut short = vec![0xAA, 0x55, 0x07, 0x00, 0x01, 0x00];
    short.push(sacp::crc8(&short));
    short.extend_from_slice(&[0; 7]);
    let mut decoder = Decoder::default();
    decoder.push(&short);
    assert_eq!(decoder.next_packet(), None);
}

#[test]
fn sacp_module_and_machine_info_follow_the_documented_fields() {
    let module = &MODULE_ACK[15..33];
    // UInt8 count, UInt16 count, and no count all read the same single module.
    let mut wide = vec![0, 1, 0];
    wide.extend_from_slice(module);
    let mut open = vec![0];
    open.extend_from_slice(module);
    for payload in [&MODULE_ACK[13..33], &wide[..], &open[..]] {
        assert_eq!(sacp::parse_modules(payload), Ok(vec![printing_module()]));
    }
    assert_eq!(sacp::parse_modules(&[0]), Ok(vec![]));
    assert_eq!(sacp::parse_modules(&[0, 0]), Ok(vec![]));
    let error = sacp::parse_modules(&[3]).unwrap_err();
    assert!(error.contains("result 3"), "{error}");
    assert!(sacp::parse_modules(&[]).is_err());
    // Truncated or padded lists match no reading.
    assert!(sacp::parse_modules(&MODULE_ACK[13..32]).is_err());
    let mut padded = MODULE_ACK[13..33].to_vec();
    padded.push(0);
    assert!(sacp::parse_modules(&padded).is_err());

    let machine = fake::machine_payload(4, "V1.2.3");
    assert_eq!(
        sacp::parse_machine(&machine),
        Ok(MachineInfo {
            model: 4,
            hardware: 3,
            firmware: "V1.2.3".into()
        })
    );
    assert!(sacp::parse_machine(&machine[..machine.len() - 1]).is_err());
    assert!(sacp::parse_machine(&[1]).unwrap_err().contains("result 1"));
    assert_eq!(sacp::machine_name(3), Some("Snapmaker Artisan"));
    assert_eq!(sacp::machine_name(6), None);
}

#[test]
fn sacp_modules_identify_whether_the_machine_is_a_printer() {
    let config = base();
    let with = |ids: &[u16]| -> Vec<ModuleInfo> {
        ids.iter()
            .map(|id| ModuleInfo {
                module_id: *id,
                ..printing_module()
            })
            .collect()
    };
    let machine = MachineInfo {
        model: 4,
        hardware: 1,
        firmware: "V1.2.3".into(),
    };
    for (ids, state, named) in [
        (
            &[0][..],
            PrinterState::Busy,
            "Printing Module (ID 0), firmware v1.0.2",
        ),
        (
            &[13],
            PrinterState::Busy,
            "Dual Extrusion Printing Module (ID 13)",
        ),
        (
            &[0, 16],
            PrinterState::Busy,
            "Snapmaker Artisan Enclosure (ID 16)",
        ),
        (&[1], PrinterState::Attention, "CNC Module (ID 1)"),
        (&[2], PrinterState::Attention, "1.6W Laser Module (ID 2)"),
        (&[14], PrinterState::Attention, "10W Laser Module (ID 14)"),
        (&[20], PrinterState::Attention, "40W Laser Module (ID 20)"),
        (&[99], PrinterState::Attention, "unknown module ID 99"),
        (&[7], PrinterState::Attention, "Air Purifier Module (ID 7)"),
        (&[], PrinterState::Attention, "NO PRINTING MODULE"),
    ] {
        let p = sacp::status(&config, &with(ids), Some(&machine));
        assert_eq!(p.state, state, "{ids:?}");
        let text = detail(&p);
        assert!(text.contains(named), "{ids:?}: {text}");
        assert!(text.contains("Snapmaker J1, firmware V1.2.3"), "{text}");
        if state == PrinterState::Busy {
            assert!(text.starts_with("PRINTING MODULE; "), "{text}");
            assert!(text.ends_with(sacp::NOT_PUBLISHED), "{text}");
        } else {
            assert!(text.starts_with("NO PRINTING MODULE"), "{text}");
            assert!(text.ends_with(sacp::NOT_A_PRINTER), "{text}");
        }
        assert_eq!((p.job_id, p.progress), (None, None));
    }
    // Printer-supplied text cannot add lines or control characters to detail.
    let mut hostile = with(&[0]);
    hostile[0].firmware = format!("v1\n{}", "x".repeat(100));
    let p = sacp::status(&config, &hostile, None);
    assert!(!detail(&p).contains('\n'));
    assert!(!detail(&p).contains(&"x".repeat(40)));
}

fn quick_j1() -> sacp::Timing {
    sacp::Timing {
        fresh: Duration::from_millis(500),
        poll: Duration::from_millis(100),
        reply: Duration::from_millis(300),
        connect: Duration::from_secs(2),
        backoff: Duration::from_millis(50),
    }
}
fn j1(f: &FakeJ1) -> sacp::Client {
    sacp::Client::new(f.printer())
        .unwrap()
        .with_timing(quick_j1())
}
/// Only the documented module-info and machine-info requests ever reach the printer.
async fn only_info_requests(f: &FakeJ1) {
    let requests = f.requests().await;
    assert!(!requests.is_empty());
    for r in &requests {
        assert_eq!(
            (r.receiver, r.sender, r.attribute, r.set),
            (
                sacp::CONTROLLER,
                sacp::SOFTWARE,
                sacp::REQUEST,
                sacp::SYSTEM
            ),
            "{r:?}"
        );
        assert!(
            matches!(r.id, sacp::MODULE_INFO | sacp::MACHINE_INFO),
            "{r:?}"
        );
        assert!(r.payload.is_empty(), "{r:?}");
    }
}

#[tokio::test]
async fn j1_reads_modules_and_never_claims_a_print_state() {
    let f = FakeJ1::new(&[0]).await;
    let c = j1(&f);
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Busy, "{:?}", p.detail);
    assert!(
        detail(&p).contains("Printing Module (ID 0)"),
        "{:?}",
        p.detail
    );
    until("machine info", || async {
        detail(&c.snapshot().await).contains("Snapmaker J1, firmware V1.2.3")
    })
    .await;
    // Fragmented replies are applied once complete.
    f.set_mode(Mode::Fragmented).await;
    f.machine.lock().await.modules = vec![(14, "v2".into())];
    until("laser module", || async {
        c.snapshot().await.state == PrinterState::Attention
    })
    .await;
    assert!(detail(&c.snapshot().await).contains(sacp::NOT_A_PRINTER));
    // A nonzero result is an error, never idle.
    f.set_mode(Mode::Result(2)).await;
    until("result error", || async {
        c.snapshot().await.state == PrinterState::Error
    })
    .await;
    assert!(detail(&c.snapshot().await).contains("result 2"));
    // A printer that stops answering goes stale and reads offline.
    f.set_mode(Mode::Silent).await;
    until("stale", || async {
        let p = c.snapshot().await;
        p.state == PrinterState::Offline && detail(&p).contains("stale")
    })
    .await;
    only_info_requests(&f).await;
    assert_eq!(f.machine.lock().await.connections, 1);
    // Each request on the connection takes the next sequence number.
    let sequences: Vec<_> = f.requests().await.iter().map(|r| r.sequence).collect();
    assert!(
        sequences.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
        "{sequences:?}"
    );
}

#[tokio::test]
async fn j1_replies_with_another_sequence_or_bad_checksums_are_never_applied() {
    for mode in [Mode::WrongSequence, Mode::Corrupt, Mode::Silent] {
        let f = FakeJ1::new(&[0]).await;
        f.set_mode(mode).await;
        let c = j1(&f);
        let p = c.snapshot().await;
        assert_eq!(p.state, PrinterState::Offline, "{mode:?}");
        assert_eq!(detail(&p), sacp::NO_REPLY, "{mode:?}");
        assert!(detail(&p).contains("not published how J1 and Artisan printers pair"));
        until("several rounds", || async { f.requests().await.len() >= 6 }).await;
        assert_eq!(c.snapshot().await.state, PrinterState::Offline, "{mode:?}");
        // The connection stays open; a correct reply to a later request is applied.
        f.set_mode(Mode::Answer).await;
        until("answered", || async {
            c.snapshot().await.state == PrinterState::Busy
        })
        .await;
        only_info_requests(&f).await;
        assert_eq!(f.machine.lock().await.connections, 1, "{mode:?}");
    }
}

#[tokio::test]
async fn j1_reconnects_need_a_fresh_reply_and_unreachable_printers_are_offline() {
    let f = FakeJ1::new(&[0]).await;
    let c = j1(&f);
    assert_eq!(c.snapshot().await.state, PrinterState::Busy);
    // After a dropped connection, nothing is fresh until the new connection answers.
    f.set_mode(Mode::Silent).await;
    f.drop_connections();
    until("reconnected", || async {
        f.machine.lock().await.connections >= 2
    })
    .await;
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(!detail(&p).contains("stale"), "{:?}", p.detail);
    f.set_mode(Mode::Answer).await;
    until("fresh after reconnect", || async {
        c.snapshot().await.state == PrinterState::Busy
    })
    .await;
    only_info_requests(&f).await;

    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut config = base();
    config.kind = Some("snapmaker-j1".into());
    config.host = Some("127.0.0.1".into());
    config.port = Some(closed);
    let p = sacp::Client::new(config)
        .unwrap()
        .with_timing(quick_j1())
        .snapshot()
        .await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(detail(&p).contains("unavailable"), "{:?}", p.detail);
}

#[tokio::test]
async fn j1_refuses_every_machine_action_without_sending_anything() {
    let f = FakeJ1::new(&[0]).await;
    refuses_every_action(f.printer(), MONITORING_J1, PrinterState::Busy).await;
    only_info_requests(&f).await;
}

// Snapmaker 2.0 over HTTP.

#[test]
fn http_status_maps_only_the_published_idle() {
    let config = base();
    let machine = Machine::from_connect(&fake::connect_reply(), TOKEN);
    assert_eq!(
        machine,
        Machine {
            series: Some("Snapmaker 2.0 A150".into()),
            head: Some("3".into()),
            not_printing: false,
            enclosure: Some(false),
        }
    );
    let p = http::status(&config, &fake::idle(), &machine);
    assert_eq!(p.state, PrinterState::Idle);
    assert_eq!(detail(&p), "IDLE; Snapmaker 2.0 A150; head type 3");
    assert_eq!(
        (p.job_id, p.progress, p.nozzle_temperature),
        (None, None, None)
    );
    for (status, starts) in [
        (json!("RUNNING"), "RUNNING; "),
        (json!("PAUSED"), "PAUSED; "),
        (json!("idle"), "idle; "),
        (json!(7), "7; "),
        (json!(""), "UNKNOWN STATUS; "),
        (Value::Null, "UNKNOWN STATUS; "),
        (json!({"nested": true}), "UNKNOWN STATUS; "),
    ] {
        let p = http::status(&config, &json!({"status": status}), &machine);
        assert_eq!(p.state, PrinterState::Error, "{status}");
        assert!(detail(&p).starts_with(starts), "{status}: {:?}", p.detail);
        assert!(detail(&p).contains("published only IDLE"), "{:?}", p.detail);
    }
    let p = http::status(&config, &json!({}), &machine);
    assert_eq!(p.state, PrinterState::Error);
    // Raw values are sanitised and capped.
    let p = http::status(
        &config,
        &json!({"status": format!("BAD\nVALUE{}", "Z".repeat(100))}),
        &machine,
    );
    assert!(detail(&p).starts_with("BAD?VALUE"), "{:?}", p.detail);
    assert!(!detail(&p).contains(&"Z".repeat(30)));
    // A head named as laser or CNC means the machine is not a printer right now.
    for head in ["LASER", "cnc", "Laser 10W"] {
        let mut reply = fake::connect_reply();
        reply["headType"] = json!(head);
        reply["hasEnclosure"] = json!(true);
        let machine = Machine::from_connect(&reply, TOKEN);
        let p = http::status(&config, &fake::idle(), &machine);
        assert_eq!(p.state, PrinterState::Attention, "{head}");
        assert!(detail(&p).contains(&format!("head type {head}; enclosure")));
        assert!(detail(&p).ends_with(http::NOT_A_PRINTER));
        let p = http::status(&config, &json!({"status": "RUNNING"}), &machine);
        assert_eq!(p.state, PrinterState::Error, "{head}");
    }
    // The series is never used to guess the head, and a missing head is simply not shown.
    let machine = Machine::from_connect(
        &json!({"token": TOKEN, "series": "Snapmaker 2.0 A350 Laser"}),
        TOKEN,
    );
    let p = http::status(&config, &fake::idle(), &machine);
    assert_eq!(p.state, PrinterState::Idle);
    assert!(!detail(&p).contains("head type"));
    // A value that repeats the token is withheld.
    let machine = Machine::from_connect(&json!({"token": TOKEN, "series": TOKEN}), TOKEN);
    assert_eq!(machine.series.as_deref(), Some("(withheld)"));
}

fn quick_2() -> http::Timing {
    http::Timing {
        pair: Duration::from_millis(600),
        settle: Duration::from_millis(50),
        poll: Duration::from_millis(50),
        fresh: Duration::from_millis(500),
        request: Duration::from_secs(2),
        retry: Duration::from_millis(50),
    }
}
fn two(f: &Fake2, timing: http::Timing) -> http::Client {
    http::Client::new(f.printer()).unwrap().with_timing(timing)
}
/// The token travels only as the documented parameter, and no reply text contains it.
async fn token_hygiene(f: &Fake2, seen: &[Printer]) {
    for p in seen {
        let text = serde_json::to_string(p).unwrap();
        assert!(!text.contains(TOKEN), "{text}");
    }
    for r in f.requests().await {
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/api/v1/connect") => {
                assert_eq!((r.query, r.body.as_str()), (None, ""), "connect");
            }
            ("GET", "/api/v1/status") => {
                assert_eq!(r.query, Some(format!("token={TOKEN}")));
                assert!(r.body.is_empty());
            }
            ("POST", "/api/v1/disconnect") => {
                assert_eq!(r.query, None);
                assert_eq!(r.body, format!("token={TOKEN}"));
            }
            other => panic!("undocumented request {other:?}"),
        }
    }
}

#[tokio::test]
async fn http_pairs_polls_status_and_goes_stale() {
    let f = Fake2::new(Approval::Immediate).await;
    let c = two(&f, quick_2());
    let mut seen = Vec::new();
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Idle, "{:?}", p.detail);
    assert_eq!(detail(&p), "IDLE; Snapmaker 2.0 A150; head type 3");
    seen.push(p);
    assert_eq!(f.count("/api/v1/connect").await, 1);
    until("polling", || async { f.count("/api/v1/status").await >= 3 }).await;

    // Undocumented values are errors with the value shown.
    f.machine.lock().await.status = json!({"status": "RUNNING"});
    until("running", || async {
        c.snapshot().await.state == PrinterState::Error
    })
    .await;
    seen.push(c.snapshot().await);
    assert!(detail(&seen[1]).starts_with("RUNNING; "));
    // A value echoing the token is withheld.
    f.machine.lock().await.status = json!({"status": format!("BUSY {TOKEN}")});
    until("withheld", || async {
        detail(&c.snapshot().await).starts_with("(withheld)")
    })
    .await;

    // Missing live responses go stale, then offline; the token is kept.
    f.machine.lock().await.failing = true;
    until("stale", || async {
        let p = c.snapshot().await;
        p.state == PrinterState::Offline && detail(&p).contains("stale")
    })
    .await;
    seen.push(c.snapshot().await);
    f.machine.lock().await.status = fake::idle();
    f.machine.lock().await.failing = false;
    until("fresh again", || async {
        c.snapshot().await.state == PrinterState::Idle
    })
    .await;
    assert_eq!(f.count("/api/v1/connect").await, 1);

    // A token the printer stops accepting ends the connection.
    f.machine.lock().await.revoked = true;
    until("ended", || async {
        detail(&c.snapshot().await).starts_with("The Snapmaker printer no longer accepts")
            || f.count("/api/v1/connect").await > 1
    })
    .await;
    f.machine.lock().await.revoked = false;
    // Reading status again requests a new connection.
    until("paired again", || async {
        c.snapshot().await.state == PrinterState::Idle
    })
    .await;
    assert!(f.count("/api/v1/connect").await >= 2);
    token_hygiene(&f, &seen).await;
}

#[tokio::test]
async fn http_pairing_waits_for_the_touchscreen_and_times_out() {
    // The connect request is held until the user confirms.
    let f = Fake2::new(Approval::Hold).await;
    let mut timing = quick_2();
    timing.retry = Duration::from_secs(3600);
    let c = two(&f, timing);
    let mut seen = vec![c.snapshot().await];
    assert_eq!(seen[0].state, PrinterState::Offline);
    assert_eq!(detail(&seen[0]), http::APPROVE);
    until("timed out", || async {
        detail(&c.snapshot().await) == http::TIMED_OUT
    })
    .await;
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(detail(&p).contains("Approve the request on the touchscreen"));
    assert!(detail(&p).contains("retry"));
    seen.push(p);
    // No connection was kept and no status was requested.
    assert_eq!(f.count("/api/v1/connect").await, 1);
    assert_eq!(f.count("/api/v1/status").await, 0);
    // Approval after the deadline is not taken as a connection, and there is no retry
    // before the retry delay.
    f.approve();
    for _ in 0..5 {
        seen.push(c.snapshot().await);
        assert_eq!(seen.last().unwrap().state, PrinterState::Offline);
    }
    assert_eq!(f.count("/api/v1/connect").await, 1);
    assert_eq!(f.count("/api/v1/status").await, 0);
    token_hygiene(&f, &seen).await;

    // With a short retry delay, reading status again pairs once approved.
    let g = Fake2::new(Approval::Hold).await;
    let c = two(&g, quick_2());
    assert_eq!(detail(&c.snapshot().await), http::APPROVE);
    g.approve();
    until("approved", || async {
        c.snapshot().await.state == PrinterState::Idle
    })
    .await;
    assert_eq!(g.count("/api/v1/connect").await, 1);
    token_hygiene(&g, &[c.snapshot().await]).await;
}

#[tokio::test]
async fn http_token_refused_until_approval_is_released_on_timeout() {
    // The token arrives at once, but status is refused until the user confirms.
    let f = Fake2::new(Approval::Pending).await;
    let mut timing = quick_2();
    timing.retry = Duration::from_secs(3600);
    let c = two(&f, timing);
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Offline);
    assert_eq!(detail(&p), http::APPROVE);
    let mut seen = vec![p];
    until("timed out", || async {
        detail(&c.snapshot().await) == http::TIMED_OUT
    })
    .await;
    // The unapproved token was released with the documented disconnect.
    until("disconnect", || async {
        f.count("/api/v1/disconnect").await == 1
    })
    .await;
    seen.push(c.snapshot().await);
    token_hygiene(&f, &seen).await;

    let g = Fake2::new(Approval::Pending).await;
    let c = two(&g, quick_2());
    assert_eq!(detail(&c.snapshot().await), http::APPROVE);
    until("status attempted", || async {
        g.count("/api/v1/status").await >= 1
    })
    .await;
    g.approve();
    until("approved", || async {
        c.snapshot().await.state == PrinterState::Idle
    })
    .await;
    assert_eq!(g.count("/api/v1/disconnect").await, 0);
    token_hygiene(&g, &[c.snapshot().await]).await;
}

#[tokio::test]
async fn http_unreachable_and_unusable_replies_are_offline() {
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut config = base();
    config.kind = Some("snapmaker-2".into());
    config.host = Some("127.0.0.1".into());
    config.port = Some(closed);
    let p = http::Client::new(config)
        .unwrap()
        .with_timing(quick_2())
        .snapshot()
        .await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(detail(&p).contains("unavailable"), "{:?}", p.detail);

    for reply in [
        json!({"series": "Snapmaker 2.0 A150"}),
        json!({"token": ""}),
        json!({"token": "has space"}),
        json!({"token": "x".repeat(200)}),
    ] {
        let f = Fake2::new(Approval::Immediate).await;
        f.machine.lock().await.connect = reply.clone();
        let p = two(&f, quick_2()).snapshot().await;
        assert_eq!(p.state, PrinterState::Offline, "{reply}");
        assert!(
            detail(&p).contains("no usable token"),
            "{reply}: {:?}",
            p.detail
        );
        assert_eq!(f.count("/api/v1/status").await, 0);
    }
}

#[tokio::test]
async fn http_refuses_every_machine_action_without_sending_anything() {
    let f = Fake2::new(Approval::Immediate).await;
    refuses_every_action(f.printer(), MONITORING_2, PrinterState::Idle).await;
    token_hygiene(&f, &[]).await;
    assert!(f.count("/api/v1/status").await >= 1);
}

#[test]
fn config_takes_only_host_and_port() {
    for kind in ["snapmaker-2", "snapmaker-j1"] {
        let base = format!("[backend]\nkind='printers'\n[[printers]]\nkind='{kind}'\nid='test'\nname='Test'\nmodel='Test'\n");
        let load = |extra: &str| {
            let config: dutchman_print::config::Config =
                toml::from_str(&format!("{base}{extra}")).unwrap();
            Printers::new(config).err()
        };
        // No slicer and no slicer profiles are needed.
        assert_eq!(load("host='192.168.1.20'"), None);
        assert_eq!(load("host='printer.local'\nport=9000"), None);
        assert_eq!(load("host='fe80::1'"), None);
        for (extra, message) in [
            ("", "Set host"),
            ("host='http://192.168.1.20'", "without a scheme"),
            ("host='192.168.1.20/api'", "without a scheme"),
            ("host='192.168.1.20'\nport=0", "port"),
            ("host='192.168.1.20'\nprinter_profile='M'", "leave slicer"),
            ("host='192.168.1.20'\nslicer='prusa'", "leave slicer"),
            ("host='192.168.1.20'\napi_key='k'", "Set only host"),
            (
                "host='192.168.1.20'\nurl='http://192.168.1.20'",
                "Set only host",
            ),
            ("host='192.168.1.20'\nlan_control=true", "Set only host"),
            ("host='192.168.1.20'\nprofile='resin'", "Set only host"),
            ("host='192.168.1.20'\nnozzle_diameter=0.4", "Set only host"),
        ] {
            let error = load(extra).unwrap();
            assert!(error.contains(message), "{kind} {extra}: {error}");
        }
    }
    let text = "[backend]\nkind='printers'\n[[printers]]\nkind='snapmaker'\nid='test'\nname='Test'\nmodel='Test'\nhost='192.168.1.20'";
    let config: dutchman_print::config::Config = toml::from_str(text).unwrap();
    assert!(Printers::new(config)
        .err()
        .unwrap()
        .contains("snapmaker-2, snapmaker-j1"));
}
