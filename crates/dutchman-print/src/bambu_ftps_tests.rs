#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::config::Printer as PrinterConfig;
use crate::safety::hash;
#[path = "../tests/helpers/ftps.rs"]
mod fixture;
use fixture::Fake;
use std::sync::atomic::Ordering;
async fn stage(config: &crate::config::Printer) -> Result<(), Error> {
    tokio::time::timeout(
        Duration::from_secs(5),
        upload(config, "test.gcode.3mf", b"prepared bytes"),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn upload_reuses_tls12_and_tls13_control_session_and_checks_size() {
    for tls12 in [true, false] {
        let fake = Fake::new(tls12, false, 0).await;
        stage(&fake.config).await.unwrap();
        assert_eq!(fake.credentials.load(Ordering::SeqCst), 1);
        assert_eq!(fake.received.load(Ordering::SeqCst), 14);
        assert_eq!(fake.resumed.load(Ordering::SeqCst), 1);
        let fake = Fake::new(tls12, true, 0).await;
        assert!(stage(&fake.config)
            .await
            .unwrap_err()
            .to_string()
            .contains("size differs"));
        assert_eq!(fake.deleted.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rejects_changed_control_identity_before_credentials() {
    let mut fake = Fake::new(false, false, 0).await;
    fake.config.cert_sha256 = Some("0".repeat(64));
    assert!(stage(&fake.config)
        .await
        .unwrap_err()
        .to_string()
        .contains("TLS identity"));
    assert_eq!(fake.credentials.load(Ordering::SeqCst), 0);
    assert_eq!(fake.received.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejected_credentials_are_reported_without_echoing_secrets() {
    let mut fake = Fake::new(false, false, 0).await;
    fake.config.access_code = Some("wrong-secret".into());
    let error = stage(&fake.config).await.unwrap_err();
    assert!(matches!(error, Error::Authentication));
    assert!(!error.to_string().contains("wrong-secret"));
    assert_eq!(fake.received.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn data_channel_must_verify_and_resume_before_file_bytes() {
    let fake = Fake::new(false, false, 1).await;
    assert!(stage(&fake.config)
        .await
        .unwrap_err()
        .to_string()
        .contains("data TLS"));
    assert_eq!(fake.received.load(Ordering::SeqCst), 0);
    assert_eq!(fake.resumed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ca_uses_serial_name_for_both_channels() {
    let temp = tempfile::tempdir().unwrap();
    let ca = temp.path().join("synthetic-ca.pem");
    for serial in ["TESTSERIAL", "OTHER-SERIAL"] {
        let mut fake = Fake::new(false, false, 0).await;
        std::fs::write(&ca, &fake.pem).unwrap();
        fake.config.cert_sha256 = None;
        fake.config.ca_file = Some(ca.clone());
        fake.config.serial = Some(serial.into());
        let result = stage(&fake.config).await;
        assert_eq!(result.is_ok(), serial == "TESTSERIAL");
        assert_eq!(
            fake.credentials.load(Ordering::SeqCst),
            usize::from(serial == "TESTSERIAL")
        );
    }
}

#[tokio::test]
async fn trusted_data_channel_without_resumption_receives_no_bytes() {
    for tls12 in [true, false] {
        let fake = Fake::new(tls12, false, 2).await;
        assert!(stage(&fake.config)
            .await
            .unwrap_err()
            .to_string()
            .contains("did not resume"));
        assert_eq!(fake.received.load(Ordering::SeqCst), 0);
        assert_eq!(fake.resumed.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn rejects_filename_injection_and_empty_bytes_before_connecting() {
    let config = crate::config::Printer::default();
    for name in [
        "../test.gcode.3mf",
        "test\r\nQUIT.gcode.3mf",
        "test.3mf",
        "/test.gcode.3mf",
    ] {
        assert!(matches!(
            upload(&config, name, b"x").await,
            Err(Error::Refused(_))
        ));
    }
    assert!(matches!(
        upload(&config, "test.gcode.3mf", b"").await,
        Err(Error::Refused(_))
    ));
    assert!(passive_port("227 (127,0,0,1,0,0)").is_err());
    assert!(passive_port("227 (127,0,0,1,1,256)").is_err());
}

#[tokio::test]
async fn slow_progress_succeeds_and_missing_completion_deletes_file() {
    let fake = Fake::new(false, false, 4).await;
    let result = upload_with_idle(
        &fake.config,
        "test.gcode.3mf",
        &vec![1; 16 << 20],
        Duration::from_millis(500),
        Duration::from_millis(1000),
    )
    .await;
    assert!(
        result.is_ok(),
        "{result:?}; received {} bytes",
        fake.received.load(Ordering::SeqCst)
    );
    assert_eq!(fake.received.load(Ordering::SeqCst), 16 << 20);
    assert_eq!(fake.deleted.load(Ordering::SeqCst), 0);
    let fake = Fake::new(false, false, 3).await;
    assert!(upload_with_idle(
        &fake.config,
        "test.gcode.3mf",
        b"test",
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .await
    .is_err());
    assert_eq!(fake.deleted.load(Ordering::SeqCst), 1);
}
