//! Backend-neutral printing companion. See `docs/print.md`.

pub mod backend;
pub mod bambu;
pub mod config;
pub mod confirmation;
pub mod printers;
pub mod prusalink;
pub mod safety;
mod server;
pub mod simulated;
pub mod slicer;
pub mod upload;

pub use server::{router, ServerConfig, StorageConfig};

use backend::Printer;

pub fn new_job_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "Cannot create a job id. Try again.")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn text_report(reports: &[Printer]) -> String {
    let mut text = String::new();
    for report in reports {
        text.push_str(&format!("{}: {}", report.name, report.state));
        if let Some(progress) = report.progress {
            text.push_str(&format!(", {:.1}%", progress * 100.0));
        }
        if let Some(seconds) = report.seconds_left {
            text.push_str(&format!(", {seconds}s left"));
        }
        text.push('\n');
    }
    text
}

/// Audit machine actions without confirmation values or credentials.
pub fn log_action(action: &str, job: &backend::Job) {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    eprintln!(
        "time={time} printer={} job={} action={action}",
        job.printer, job.id
    );
}

/// Deletes jobs older than `keep` in `data_dir`, now and then every few minutes; run it
/// beside the server.
pub async fn remove_expired(data_dir: std::path::PathBuf, keep: std::time::Duration) {
    loop {
        let dir = data_dir.clone();
        let _ = tokio::task::spawn_blocking(move || remove_older_than(&dir, keep)).await;
        tokio::time::sleep(std::time::Duration::from_secs(300).min(keep)).await;
    }
}

fn remove_older_than(data_dir: &std::path::Path, keep: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > keep);
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let job = name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if old && job {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Audit a bed clearance without credentials.
pub fn log_clear(printer: &str) {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    eprintln!("time={time} printer={printer} job=- action=clear");
}
