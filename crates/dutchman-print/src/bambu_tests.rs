#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use serde_json::json;
use std::sync::atomic::Ordering;
#[path = "../tests/helpers/bambu.rs"]
mod fake;
use fake::Fake;
fn config(fake: &Fake) -> crate::config::Printer {
    crate::config::Printer {
        kind: Some("bambu".into()),
        host: Some("127.0.0.1".into()),
        port: Some(fake.port),
        serial: Some("TESTSERIAL".into()),
        access_code: Some("test-access".into()),
        cert_sha256: Some(crate::safety::hash(fake.cert.as_ref())),
        id: "bambu".into(),
        name: "Bambu".into(),
        model: "Test model".into(),
        ..Default::default()
    }
}
fn client(fake: &Fake, oneshot: bool) -> Client {
    let mut c = Client::new(config(fake), oneshot).unwrap();
    c.fresh = Duration::from_millis(180);
    c.pacing = Duration::from_millis(200);
    c
}
async fn state(c: &Client, expected: PrinterState, detail: &str) -> Printer {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = c.snapshot().await;
            if p.state == expected && p.detail.as_deref() == Some(detail) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn full_state_deltas_states_missing_values_and_stale() {
    let fake = Fake::new().await;
    let c = client(&fake, false);
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Idle);
    assert_eq!(p.seconds_left, Some(180));
    assert_eq!(p.progress, Some(0.2));
    assert_eq!(p.nozzle_temperature, Some(210.0));
    for (wire, mapped) in [
        ("PREPARE", PrinterState::Printing),
        ("RUNNING", PrinterState::Printing),
        ("PAUSE", PrinterState::Paused),
        ("FINISH", PrinterState::Idle),
        ("FAILED", PrinterState::Error),
        ("NEW_STATE", PrinterState::Error),
        ("IDLE", PrinterState::Idle),
    ] {
        fake.reports
            .send(json!({"print":{"gcode_state":wire}}))
            .unwrap();
        let p = state(&c, mapped, wire).await;
        assert_eq!(p.job_name.as_deref(), Some("Test cube"));
        assert_eq!(p.bed_temperature, Some(55.0));
        if wire == "FINISH" || wire == "FAILED" {
            assert_eq!(p.needs_clearing, Some(true));
        }
    }
    fake.reports
        .send(json!({"print":{"bed_temper":60,"mc_remaining_time":null,"mc_percent":null}}))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let p = c.snapshot().await;
    assert_eq!(p.bed_temperature, Some(60.0));
    assert_eq!(p.seconds_left, None);
    assert_eq!(p.progress, None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(c.snapshot().await.state, PrinterState::Offline);
    assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn reconnect_resets_baseline_and_paces_requests() {
    let fake = Fake::new().await;
    let mut c = client(&fake, false);
    c.pacing = Duration::from_millis(1500);
    assert_eq!(c.snapshot().await.state, PrinterState::Idle);
    fake.reports
        .send(json!({"print":{"gcode_state":"PAUSE","bed_temper":99}}))
        .unwrap();
    state(&c, PrinterState::Paused, "PAUSE").await;
    fake.reports.send(Value::Null).unwrap();
    Fake::wait(&fake.connections, 2).await;
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    assert!(c.state.borrow().received.is_none());
    Fake::wait(&fake.requests, 2).await;
    let p = state(&c, PrinterState::Idle, "IDLE").await;
    assert_eq!(p.bed_temperature, Some(55.0));
}
#[tokio::test]
async fn authentication_and_tls_identity_fail_before_credentials() {
    let fake = Fake::new().await;
    let mut p = config(&fake);
    p.access_code = Some("wrong-secret".into());
    let c = Client::new(p, true).unwrap();
    let p = c.snapshot().await;
    assert_eq!(p.state, PrinterState::Offline);
    assert!(p.detail.unwrap().contains("authentication"));
    for pin in [
        "0".repeat(64),
        crate::safety::hash(
            rcgen::generate_simple_self_signed(vec!["TESTSERIAL".into()])
                .unwrap()
                .cert
                .der(),
        ),
    ] {
        let mut p = config(&fake);
        p.cert_sha256 = Some(pin);
        let c = Client::new(p, true).unwrap();
        assert_eq!(c.snapshot().await.state, PrinterState::Offline);
    }
    assert_eq!(fake.credentials.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn ca_serial_validation_and_one_shot_disconnect() {
    let fake = Fake::new().await;
    let temp = tempfile::tempdir().unwrap();
    let ca = temp.path().join("ca.pem");
    std::fs::write(&ca, fake.pem.replace("\n", "\r\n")).unwrap();
    let mut p = config(&fake);
    p.cert_sha256 = None;
    p.ca_file = Some(ca);
    let c = Client::new(p.clone(), true).unwrap();
    assert_eq!(c.snapshot().await.state, PrinterState::Idle);
    Fake::wait(&fake.disconnects, 1).await;
    p.serial = Some("OTHER-SERIAL".into());
    let c = Client::new(p, true).unwrap();
    assert_eq!(c.snapshot().await.state, PrinterState::Offline);
    assert_eq!(fake.credentials.load(Ordering::SeqCst), 1);
}
#[test]
fn bounds_and_contradictory_reports() {
    let p = crate::config::Printer::default();
    let mut s = Telemetry::default();
    assert!(!s.report(br#"{"print":{"bed_temper":30}}"#));
    assert_eq!(s.printer(&p, FRESH).state, PrinterState::Offline);
    assert!(s.report(br#"{"print":{"gcode_state":"IDLE","print_error":123,"mc_remaining_time":18446744073709551615,"mc_percent":101}}"#));
    let report = s.printer(&p, FRESH);
    assert_eq!(report.state, PrinterState::Error);
    assert_eq!(report.seconds_left, None);
    assert_eq!(report.progress, None);
    assert!(!s.report(br#"{"print":{"command":"ack"}}"#));
    assert!(s.report(br#"{"print":{"subtask_name":null,"gcode_file":"fallback.gcode","nozzle_temper":-5,"mc_percent":-1}}"#));
    let report = s.printer(&p, FRESH);
    assert_eq!(report.job_name.as_deref(), Some("fallback.gcode"));
    assert_eq!(report.nozzle_temperature, Some(-5.0));
    assert_eq!(report.progress, None);
    assert!(s.report(br#"{"print":{"gcode_state":"FINISH"}}"#));
    assert!(s.terminal);
    s.terminal = false;
    assert!(s.report(br#"{"print":{"bed_temper":31}}"#));
    assert!(!s.terminal);
    assert!(s.report(br#"{"print":{"gcode_state":"IDLE"}}"#));
    assert!(s.report(br#"{"print":{"gcode_state":"FINISH"}}"#));
    assert!(s.terminal);
}

#[tokio::test]
async fn keepalive_timeout_reconnects_without_old_state() {
    let fake = Fake::new().await;
    let mut c = client(&fake, false);
    c.options.set_keep_alive(Duration::from_secs(1));
    assert_eq!(c.snapshot().await.state, PrinterState::Idle);
    fake.reports.send(json!({"test_ignore_ping":true})).unwrap();
    Fake::wait(&fake.connections, 2).await;
    Fake::wait(&fake.requests, 2).await;
    state(&c, PrinterState::Idle, "IDLE").await;
}

#[tokio::test]
async fn secrets_are_redacted_from_telemetry() {
    let fake = Fake::new().await;
    let c = client(&fake, false);
    c.snapshot().await;
    fake.reports
        .send(json!({"print":{"gcode_state":"test-access","subtask_name":"test-access"}}))
        .unwrap();
    let p = state(&c, PrinterState::Error, "[redacted]").await;
    assert_eq!(p.job_name.as_deref(), Some("[redacted]"));
}

#[tokio::test]
async fn configuration_requires_explicit_trust_and_private_credentials() {
    let fake = Fake::new().await;
    let good = config(&fake);
    assert!(validate(&good).is_ok());
    assert_eq!(
        Client::new(good.clone(), true)
            .unwrap()
            .options
            .broker_address()
            .1,
        fake.port
    );
    let mut p = good.clone();
    p.port = None;
    assert_eq!(
        Client::new(p, true).unwrap().options.broker_address().1,
        8883
    );
    let mut p = good.clone();
    p.port = Some(0);
    assert!(validate(&p).is_err());
    for port in ["-1", "65536", "'8883'"] {
        let text = format!("id='test'\nname='Test'\nmodel='Synthetic'\nport={port}\n");
        assert!(toml::from_str::<crate::config::Printer>(&text).is_err());
    }

    let mut p = good.clone();
    p.cert_sha256 = None;
    assert!(validate(&p).is_err());
    p = good.clone();
    p.ca_file = Some("ca.pem".into());
    assert!(validate(&p).is_err());
    p = good.clone();
    p.cert_sha256 = Some("bad".into());
    assert!(validate(&p).is_err());
    p = good.clone();
    p.serial = Some("topic/#".into());
    assert!(validate(&p).is_err());
    p = good.clone();
    p.host = Some("http://127.0.0.1".into());
    assert!(validate(&p).is_err());
    p = good.clone();
    p.access_code = None;
    assert!(validate(&p).is_err());
    p = good;
    p.api_key = Some("unused".into());
    assert!(validate(&p).is_err());
    assert_eq!(fake.connections.load(Ordering::SeqCst), 0);
}
