//! Bounded, durable JSON Lines history. Only explicit metadata is serialized.
use crate::backend::{Error, Estimate};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

const KEEP: usize = 10_000;
const MAX_BYTES: u64 = 32 << 20;
const MAX_LINE: usize = 3072;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    pub printer: String,
    pub printer_kind: String,
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    pub file_name: Option<String>,
    pub file_size: Option<u64>,
    pub source: Option<String>,
    pub outcome: String,
    pub started_at: Option<String>,
    pub ended_at: String,
    pub duration_seconds: Option<u64>,
    pub estimate: Option<Estimate>,
}

#[derive(Clone, Default, Deserialize, clap::Args)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    #[arg(long)]
    pub printer: Option<String>,
    #[arg(long)]
    pub outcome: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub limit: Option<usize>,
}
impl Filter {
    pub fn validate(&self) -> Result<(), String> {
        if self.limit.is_some_and(|n| n == 0 || n > KEEP) {
            return Err("Use a history limit from 1 to 10000.".into());
        }
        if self.outcome.as_deref().is_some_and(|o| {
            !matches!(
                o,
                "done"
                    | "cancelled"
                    | "failed"
                    | "stopped"
                    | "ended"
                    | "unresolved_after_restart"
                    | "bed_clear"
            )
        }) {
            return Err("Choose a history outcome: done, cancelled, failed, stopped, ended, unresolved_after_restart, or bed_clear.".into());
        }
        if let Some(since) = &self.since {
            parse_time(since)?;
        }
        Ok(())
    }
}
pub fn parse_time(value: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| "Use an RFC 3339 timestamp for history since.".into())
}
pub(crate) fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}
// Retain labels only, never local directory names or URL authorities/queries.
pub(crate) fn label(value: &str) -> String {
    if value.contains("://") {
        return "[redacted]".into();
    }
    let mut bytes = 0;
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take_while(|c| {
            bytes += c.len_utf8();
            bytes <= 256
        })
        .collect()
}
pub fn log_error() {
    eprintln!("Cannot write print history. Check the private data folder; printer actions are unaffected.");
}
fn storage_error(_: impl std::fmt::Display) -> Error {
    Error::Failed("Cannot store print history. Check permissions and available space in the private data folder.".into())
}
// OS-held locks disappear when a process exits, including after a crash. Keep the
// lock file itself: removing it would let two processes lock different inodes.
async fn lock(root: &Path) -> Result<std::fs::File, Error> {
    let path = root.join("history.lock");
    for _ in 0..100 {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.share_mode(0);
        }
        match options.open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    match rustix::fs::flock(
                        &file,
                        rustix::fs::FlockOperation::NonBlockingLockExclusive,
                    ) {
                        Ok(()) => return Ok(file),
                        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {}
                        Err(e) => return Err(storage_error(e)),
                    }
                }
                #[cfg(windows)]
                return Ok(file);
                #[cfg(not(any(unix, windows)))]
                return Err(storage_error("Unsupported history locking"));
            }
            // Another holder shows as a sharing (32) or lock (33) violation on Windows.
            #[cfg(windows)]
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || matches!(e.raw_os_error(), Some(32 | 33)) => {}
            Err(e) => return Err(storage_error(e)),
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Err(storage_error("History is busy"))
}
async fn read(root: &Path) -> Result<(Vec<Record>, u64, bool), Error> {
    let file = match tokio::fs::File::open(root.join("history.jsonl")).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0, false)),
        Err(e) => return Err(storage_error(e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(storage_error)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(storage_error("size limit"));
    }
    let mut records = Vec::new();
    let mut complete = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if line.last() != Some(&b'\n') {
            break;
        }
        if line.len() > MAX_LINE {
            return Err(storage_error("line limit"));
        }
        records.push(serde_json::from_slice(line).map_err(storage_error)?);
        complete += line.len() as u64;
        if records.len() > KEEP {
            return Err(storage_error("record limit"));
        }
    }
    Ok((records, complete, complete != bytes.len() as u64))
}
pub async fn query(root: &Path, filter: &Filter) -> Result<Vec<Record>, Error> {
    filter.validate().map_err(Error::Refused)?;
    let _lock = lock(root).await?;
    let (records, _, _) = read(root).await?;
    let since = filter
        .since
        .as_deref()
        .map(parse_time)
        .transpose()
        .map_err(Error::Refused)?;
    Ok(records
        .into_iter()
        .rev()
        .filter(|r| {
            filter.printer.as_ref().is_none_or(|p| p == &r.printer)
                && filter.outcome.as_ref().is_none_or(|o| o == &r.outcome)
                && since.is_none_or(|s| parse_time(&r.ended_at).is_ok_and(|t| t >= s))
        })
        .take(filter.limit.unwrap_or(100))
        .collect())
}
/// Counts and times for one printer, or for the whole farm.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Totals {
    /// Finished jobs of any outcome; bed-clear acknowledgements are not jobs.
    pub jobs: u64,
    pub done: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub stopped: u64,
    pub ended: u64,
    pub unresolved_after_restart: u64,
    /// Jobs with a recorded start (cancelled-before-Start jobs never ran).
    pub ran: u64,
    /// `done / ran`, absent when nothing ran.
    pub success_rate: Option<f64>,
    /// Jobs that carry `duration_seconds`.
    pub timed_jobs: u64,
    pub total_seconds: u64,
    pub average_seconds: Option<f64>,
    pub last_finished_at: Option<String>,
    pub bed_clears: u64,
}
impl Totals {
    fn add(&mut self, r: &Record) {
        if r.outcome == "bed_clear" {
            self.bed_clears += 1;
            return;
        }
        self.jobs += 1;
        match r.outcome.as_str() {
            "done" => self.done += 1,
            "failed" => self.failed += 1,
            "cancelled" => self.cancelled += 1,
            "stopped" => self.stopped += 1,
            "ended" => self.ended += 1,
            "unresolved_after_restart" => self.unresolved_after_restart += 1,
            _ => {}
        }
        self.ran += u64::from(r.started_at.is_some());
        if let Some(seconds) = r.duration_seconds {
            self.timed_jobs += 1;
            self.total_seconds = self.total_seconds.saturating_add(seconds);
        }
        let newer = match (&self.last_finished_at, parse_time(&r.ended_at)) {
            (_, Err(_)) => false,
            (None, Ok(_)) => true,
            (Some(last), Ok(t)) => parse_time(last).is_ok_and(|l| t > l),
        };
        if newer {
            self.last_finished_at = Some(r.ended_at.clone());
        }
    }
    fn finish(&mut self) {
        self.success_rate = (self.ran > 0).then(|| self.done as f64 / self.ran as f64);
        self.average_seconds =
            (self.timed_jobs > 0).then(|| self.total_seconds as f64 / self.timed_jobs as f64);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PrinterTotals {
    pub printer: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Summary {
    pub total: Totals,
    pub printers: Vec<PrinterTotals>,
}

/// Query fields of the summary route; the same names as the history filter.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryFilter {
    pub printer: Option<String>,
    pub since: Option<String>,
}
impl From<SummaryFilter> for Filter {
    fn from(f: SummaryFilter) -> Self {
        Filter {
            printer: f.printer,
            since: f.since,
            outcome: None,
            limit: Some(KEEP),
        }
    }
}

pub fn summarize(records: &[Record]) -> Summary {
    let mut total = Totals::default();
    let mut by_printer = std::collections::BTreeMap::<&str, Totals>::new();
    for r in records {
        total.add(r);
        by_printer.entry(&r.printer).or_default().add(r);
    }
    total.finish();
    Summary {
        total,
        printers: by_printer
            .into_iter()
            .map(|(printer, mut totals)| {
                totals.finish();
                PrinterTotals {
                    printer: printer.into(),
                    totals,
                }
            })
            .collect(),
    }
}

/// Summary of every retained record matching the printer and since filters.
pub async fn summary(root: &Path, filter: &SummaryFilter) -> Result<Summary, Error> {
    Ok(summarize(&query(root, &filter.clone().into()).await?))
}

fn hms(seconds: f64) -> String {
    let s = seconds.round() as u64;
    format!("{}h{:02}m{:02}s", s / 3600, s % 3600 / 60, s % 60)
}
impl Summary {
    /// Aligned human table, one row per printer and a final total row.
    pub fn table(&self) -> String {
        let mut rows = vec![[
            "PRINTER",
            "JOBS",
            "DONE",
            "FAILED",
            "CANCELLED",
            "STOPPED",
            "ENDED",
            "UNRESOLVED",
            "SUCCESS",
            "TOTAL TIME",
            "AVG TIME",
            "LAST FINISHED",
            "BED CLEARS",
        ]
        .map(String::from)];
        let row = |name: &str, t: &Totals| {
            [
                name.to_string(),
                t.jobs.to_string(),
                t.done.to_string(),
                t.failed.to_string(),
                t.cancelled.to_string(),
                t.stopped.to_string(),
                t.ended.to_string(),
                t.unresolved_after_restart.to_string(),
                t.success_rate
                    .map_or("-".into(), |r| format!("{:.1}%", r * 100.0)),
                if t.timed_jobs > 0 {
                    hms(t.total_seconds as f64)
                } else {
                    "-".into()
                },
                t.average_seconds.map_or("-".into(), hms),
                t.last_finished_at.clone().unwrap_or_else(|| "-".into()),
                t.bed_clears.to_string(),
            ]
        };
        rows.extend(self.printers.iter().map(|p| row(&p.printer, &p.totals)));
        rows.push(row("TOTAL", &self.total));
        let widths: Vec<usize> = (0..13)
            .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
            .collect();
        rows.iter()
            .map(|r| {
                let cells: Vec<String> = r
                    .iter()
                    .zip(&widths)
                    .map(|(cell, w)| format!("{cell:<w$}"))
                    .collect();
                cells.join("  ").trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

const CSV_HEADER: &str = "printer,printer_kind,job_id,job_name,file_name,file_size,source,outcome,started_at,ended_at,duration_seconds,estimate";
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.into()
    }
}
/// RFC 4180 CSV with a header row. Absent values are empty cells; the estimate
/// is its JSON text. Lines end with CRLF.
pub fn csv(records: &[Record]) -> String {
    let mut out = format!("{CSV_HEADER}\r\n");
    for r in records {
        let estimate = r
            .estimate
            .as_ref()
            .and_then(|e| serde_json::to_string(e).ok());
        let cells = [
            Some(r.printer.clone()),
            Some(r.printer_kind.clone()),
            r.job_id.clone(),
            r.job_name.clone(),
            r.file_name.clone(),
            r.file_size.map(|n| n.to_string()),
            r.source.clone(),
            Some(r.outcome.clone()),
            r.started_at.clone(),
            Some(r.ended_at.clone()),
            r.duration_seconds.map(|n| n.to_string()),
            estimate,
        ];
        let line: Vec<String> = cells
            .iter()
            .map(|c| c.as_deref().map(csv_field).unwrap_or_default())
            .collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}
pub(crate) async fn append(root: &Path, record: &Record) -> Result<(), Error> {
    let _lock = lock(root).await?;
    let (records, complete, torn) = read(root).await?;
    // The journal is authoritative after a crash between append and checkpoint.
    if record.job_id.is_some()
        && records.iter().any(|r| {
            r.printer == record.printer
                && r.job_id == record.job_id
                && r.started_at == record.started_at
        })
    {
        return Ok(());
    }
    let mut line = serde_json::to_vec(record).map_err(storage_error)?;
    line.push(b'\n');
    if line.len() > MAX_LINE {
        return Err(storage_error("line limit"));
    }
    let path = root.join("history.jsonl");
    if records.len() >= KEEP {
        let mut bytes = Vec::new();
        for old in records.into_iter().skip(1) {
            serde_json::to_writer(&mut bytes, &old).map_err(storage_error)?;
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(&line);
        crate::safety::write_state(&path, &bytes)
            .await
            .map_err(storage_error)?;
    } else {
        let mut options = tokio::fs::OpenOptions::new();
        // Not append mode: on Windows that grants no right to truncate the torn tail.
        options.create(true).write(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await.map_err(storage_error)?;
        if torn {
            file.set_len(complete).await.map_err(storage_error)?;
        }
        file.seek(std::io::SeekFrom::Start(complete))
            .await
            .map_err(storage_error)?;
        file.write_all(&line).await.map_err(storage_error)?;
        file.sync_all().await.map_err(storage_error)?;
        #[cfg(unix)]
        tokio::fs::File::open(root)
            .await
            .map_err(storage_error)?
            .sync_all()
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::backend::{Backend, JobState, Options};
    use crate::safety::Safety;
    use crate::simulated::{Clock, Simulated};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[derive(Default)]
    struct TestClock(AtomicU64);
    impl Clock for TestClock {
        fn now(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::SeqCst))
        }
    }
    fn backend(clock: Arc<TestClock>) -> Arc<Simulated> {
        let config = toml::from_str(
            r#"
[backend]
kind = "simulated"
[[printers]]
id = "bench"
name = "Bench"
model = "Synthetic"
api_key = "private-key"
password = "private-password"
url = "http://127.0.0.1/?token=private-token"
"#,
        )
        .unwrap();
        Arc::new(Simulated::new(config, clock).unwrap())
    }
    fn safety(inner: Arc<Simulated>, root: &Path) -> Safety {
        Safety::new(inner)
            .with_history(root.into())
            .with_store(root.join("safety.json"))
    }
    async fn prepare(safety: &Safety, root: &Path, id: &str) {
        let path = root.join("part.3mf");
        tokio::fs::write(&path, b"synthetic file contents")
            .await
            .unwrap();
        safety
            .prepare(id, &path, "bench", Options::default())
            .await
            .unwrap();
    }
    fn record(id: usize) -> Record {
        Record {
            printer: if id.is_multiple_of(2) { "a" } else { "b" }.into(),
            printer_kind: "simulated".into(),
            job_id: Some(id.to_string()),
            job_name: Some("Part".into()),
            file_name: Some("part.3mf".into()),
            file_size: Some(42),
            source: Some("part.3mf".into()),
            outcome: "done".into(),
            started_at: Some("2026-10-30T00:00:00Z".into()),
            ended_at: "2026-10-30T00:01:00Z".into(),
            duration_seconds: Some(60),
            estimate: None,
        }
    }
    #[tokio::test]
    async fn history_failure_never_changes_clear_stop_cancel_or_status() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt as _;
        let root = tempfile::tempdir().unwrap();
        let inner = backend(Arc::new(TestClock::default()));
        let safety = safety(inner.clone(), root.path());
        tokio::fs::write(
            root.path().join("history.jsonl"),
            b"corrupt complete line\n",
        )
        .await
        .unwrap();
        safety.clear("bench").await.unwrap();
        assert_eq!(
            safety.printer("bench").await.unwrap().needs_clearing,
            Some(false)
        );
        prepare(&safety, root.path(), "stop").await;
        assert_eq!(
            safety.start("stop").await.unwrap().state,
            JobState::Printing
        );
        safety
            .stop("bench", "stop", "part.3mf", None)
            .await
            .unwrap();
        assert_eq!(safety.job("stop").await.unwrap().state, JobState::Cancelled);
        safety.clear("bench").await.unwrap();
        prepare(&safety, root.path(), "cancel").await;
        safety.start("cancel").await.unwrap();
        assert_eq!(
            safety.cancel("cancel").await.unwrap().state,
            JobState::Cancelled
        );
        assert_eq!(
            safety.job("cancel").await.unwrap().state,
            JobState::Cancelled
        );
        safety.forget("cancel").await.unwrap();
        safety.flush_history().await;
        let mut config = crate::ServerConfig::default();
        config.storage.data_dir = root.path().into();
        let app = crate::router(inner, config);
        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/printers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Cannot store print history"));
    }
    #[tokio::test]
    async fn history_locked_journal_does_not_delay_machine_actions() {
        let root = tempfile::tempdir().unwrap();
        let safety = safety(backend(Arc::new(TestClock::default())), root.path());
        prepare(&safety, root.path(), "stop").await;
        safety.clear("bench").await.unwrap();
        safety.flush_history().await;
        safety.start("stop").await.unwrap();
        let held = lock(root.path()).await.unwrap();
        tokio::time::timeout(
            Duration::from_millis(500),
            safety.stop("bench", "stop", "part.3mf", None),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(Duration::from_millis(500), safety.job("stop"))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), safety.clear("bench"))
            .await
            .unwrap()
            .unwrap();
        prepare(&safety, root.path(), "cancel").await;
        tokio::time::timeout(Duration::from_millis(500), safety.cancel("cancel"))
            .await
            .unwrap()
            .unwrap();
        drop(held);
        safety.flush_history().await;
    }
    #[tokio::test]
    async fn pending_history_write_never_blocks_stop_cancel_clear_or_start() {
        let root = tempfile::tempdir().unwrap();
        let safety = safety(backend(Arc::new(TestClock::default())), root.path());
        prepare(&safety, root.path(), "stop").await;
        safety.clear("bench").await.unwrap();
        safety.start("stop").await.unwrap();
        safety.flush_history().await;
        let hook = safety.history_write_hook();
        hook.block.store(true, Ordering::Release);
        safety.history_source("stop", "original.3mf").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), hook.entered.notified())
            .await
            .unwrap();
        assert!(hook.waiting.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(2), async {
            safety
                .stop("bench", "stop", "part.3mf", None)
                .await
                .unwrap();
            assert_eq!(safety.job("stop").await.unwrap().state, JobState::Cancelled);
            safety.clear("bench").await.unwrap();
            prepare(&safety, root.path(), "cancel").await;
            safety.start("cancel").await.unwrap();
            assert_eq!(
                safety.cancel("cancel").await.unwrap().state,
                JobState::Cancelled
            );
        })
        .await
        .unwrap();
        assert!(hook.waiting.load(Ordering::Acquire));
        hook.release.notify_one();
        safety.flush_history().await;
        let rows = query(root.path(), &Filter::default()).await.unwrap();
        assert_eq!(rows.iter().filter(|r| r.outcome == "stopped").count(), 1);
        assert_eq!(rows.iter().filter(|r| r.outcome == "cancelled").count(), 1);
        let pending: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(root.path().join("history-pending.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(pending, serde_json::json!({}));
    }
    #[tokio::test]
    async fn pending_history_restart_reloads_its_own_file_without_safety_history() {
        let root = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::default());
        let inner = backend(clock.clone());
        let first = safety(inner.clone(), root.path());
        prepare(&first, root.path(), "restart").await;
        first.clear("bench").await.unwrap();
        first.start("restart").await.unwrap();
        first
            .history_source("restart", "original.3mf")
            .await
            .unwrap();
        first.flush_history().await;
        first.save(&root.path().join("safety.json")).await.unwrap();
        inner.save(&root.path().join("jobs.json")).unwrap();
        let state: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(root.path().join("safety.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(state.get("history").is_none());
        let pending: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(root.path().join("history-pending.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(pending["restart"]["source"], "original.3mf");
        assert!(pending["restart"]["started_at"].is_string());
        drop(first);
        clock.0.store(120, Ordering::SeqCst);
        let inner = backend(clock);
        inner
            .restore_for_history(&root.path().join("jobs.json"))
            .unwrap();
        let restored = safety(inner, root.path());
        restored
            .restore(&root.path().join("safety.json"))
            .await
            .unwrap();
        restored.poll_history(true).await.unwrap();
        restored.flush_history().await;
        let rows = query(
            root.path(),
            &Filter {
                outcome: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source.as_deref(), Some("original.3mf"));
        assert_eq!(
            rows[0].started_at.as_ref(),
            pending["restart"]["started_at"]
                .as_str()
                .map(str::to_owned)
                .as_ref()
        );
    }
    #[tokio::test]
    async fn pending_history_migrates_legacy_safety_entries_and_drops_them_on_save() {
        let root = tempfile::tempdir().unwrap();
        let mut legacy = record(1);
        legacy.printer = "bench".into();
        legacy.job_id = Some("lost".into());
        let state = serde_json::json!({"reviews": {}, "history": {"lost": legacy}});
        tokio::fs::write(
            root.path().join("safety.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .await
        .unwrap();
        let first = safety(backend(Arc::new(TestClock::default())), root.path());
        let hook = first.history_write_hook();
        hook.block.store(true, Ordering::Release);
        first
            .restore(&root.path().join("safety.json"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), hook.entered.notified())
            .await
            .unwrap();
        // A safety checkpoint before migration completes must still recover on a crash.
        first.save(&root.path().join("safety.json")).await.unwrap();
        let saved: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(root.path().join("safety.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["history"]["lost"]["job_id"], "lost");
        hook.release.notify_one();
        first.flush_history().await;
        first.save(&root.path().join("safety.json")).await.unwrap();
        let saved: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(root.path().join("safety.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(saved.get("history").is_none());
        drop(first);
        let restored = safety(backend(Arc::new(TestClock::default())), root.path());
        restored
            .restore(&root.path().join("safety.json"))
            .await
            .unwrap();
        restored.poll_history(true).await.unwrap();
        restored.flush_history().await;
        let rows = query(root.path(), &Filter::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "unresolved_after_restart");
        // A stale safety snapshot must not resurrect an already archived pending record.
        tokio::fs::write(
            root.path().join("stale.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .await
        .unwrap();
        let replay = safety(backend(Arc::new(TestClock::default())), root.path());
        replay
            .restore(&root.path().join("stale.json"))
            .await
            .unwrap();
        replay.poll_history(true).await.unwrap();
        assert_eq!(
            query(root.path(), &Filter::default()).await.unwrap().len(),
            1
        );
    }
    #[tokio::test]
    async fn pending_history_failed_stop_still_withdraws_clearance() {
        let root = tempfile::tempdir().unwrap();
        let safety = safety(backend(Arc::new(TestClock::default())), root.path());
        safety.clear("bench").await.unwrap();
        assert_eq!(
            safety.printer("bench").await.unwrap().needs_clearing,
            Some(false)
        );
        assert!(safety
            .stop("bench", "missing", "part.3mf", None)
            .await
            .is_err());
        assert_eq!(
            safety.printer("bench").await.unwrap().needs_clearing,
            Some(true)
        );
        safety.flush_history().await;
    }
    #[tokio::test]
    async fn history_deduplicates_only_the_same_printer_job_and_start() {
        let root = tempfile::tempdir().unwrap();
        let first = record(1);
        let mut other = first.clone();
        other.printer = "another-printer".into();
        append(root.path(), &first).await.unwrap();
        append(root.path(), &other).await.unwrap();
        let mut restarted = first.clone();
        restarted.started_at = Some("2026-10-30T00:02:00Z".into());
        append(root.path(), &restarted).await.unwrap();
        for row in [&first, &other, &restarted] {
            append(root.path(), row).await.unwrap();
        }
        let rows = query(root.path(), &Filter::default()).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().filter(|r| r.printer == first.printer).count(),
            2
        );
        assert_eq!(
            rows.iter().filter(|r| r.printer == other.printer).count(),
            1
        );
    }
    #[tokio::test]
    async fn history_records_each_terminal_outcome_once_and_clear() {
        let root = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::default());
        let inner = backend(clock.clone());
        let safety = safety(inner.clone(), root.path());
        for (id, state) in [
            ("done", JobState::Done),
            ("failed", JobState::Failed),
            ("ended", JobState::Ended),
        ] {
            prepare(&safety, root.path(), id).await;
            safety.clear("bench").await.unwrap();
            safety.start(id).await.unwrap();
            if id == "done" {
                clock.0.fetch_add(120, Ordering::SeqCst);
            } else {
                inner.history_finish(id, state);
            }
            safety.poll_history(false).await.unwrap();
            safety.job(id).await.unwrap();
        }
        prepare(&safety, root.path(), "cancelled").await;
        safety.cancel("cancelled").await.unwrap();
        prepare(&safety, root.path(), "stopped").await;
        safety.clear("bench").await.unwrap();
        safety.start("stopped").await.unwrap();
        safety
            .stop("bench", "stopped", "part.3mf", None)
            .await
            .unwrap();
        safety.flush_history().await;
        let rows = query(root.path(), &Filter::default()).await.unwrap();
        for outcome in ["done", "failed", "ended", "cancelled", "stopped"] {
            assert_eq!(rows.iter().filter(|r| r.outcome == outcome).count(), 1);
        }
        assert_eq!(rows.iter().filter(|r| r.outcome == "bed_clear").count(), 4);
        let done = rows.iter().find(|r| r.outcome == "done").unwrap();
        assert!(done.started_at.is_some());
        assert!(done.duration_seconds.is_some());
        assert_eq!(done.estimate.as_ref().unwrap().seconds, Some(60));
        assert_eq!(done.file_size, Some(23));
        let serialized = serde_json::to_string(&rows).unwrap();
        for private in [
            "private-key",
            "private-password",
            "private-token",
            "127.0.0.1",
            "synthetic file contents",
            "preview",
            "confirmation",
        ] {
            assert!(!serialized.contains(private));
        }
    }
    #[tokio::test]
    async fn history_restart_and_expired_simulation_keep_terminal_records() {
        let root = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::default());
        let inner = backend(clock.clone());
        let first = safety(inner.clone(), root.path());
        prepare(&first, root.path(), "old").await;
        first.clear("bench").await.unwrap();
        first.start("old").await.unwrap();
        first.flush_history().await;
        inner.save(&root.path().join("jobs.json")).unwrap();
        let stale = tokio::fs::read(root.path().join("safety.json"))
            .await
            .unwrap();
        let stale_pending = tokio::fs::read(root.path().join("history-pending.json"))
            .await
            .unwrap();
        drop(first);
        clock.0.store(7200, Ordering::SeqCst);
        let restored = backend(clock.clone());
        restored
            .restore_for_history(&root.path().join("jobs.json"))
            .unwrap();
        let next = safety(restored.clone(), root.path());
        next.restore(&root.path().join("safety.json"))
            .await
            .unwrap();
        next.poll_history(true).await.unwrap();
        restored.save(&root.path().join("jobs.json")).unwrap();
        assert!(restored.job("old").await.is_err());
        let rows = query(
            root.path(),
            &Filter {
                outcome: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        // Replay a checkpoint from before the append, as after a crash during checkpoint.
        tokio::fs::write(root.path().join("stale.json"), stale)
            .await
            .unwrap();
        next.flush_history().await;
        tokio::fs::write(root.path().join("history-pending.json"), stale_pending)
            .await
            .unwrap();
        let replay = safety(restored.clone(), root.path());
        replay
            .restore(&root.path().join("stale.json"))
            .await
            .unwrap();
        replay.poll_history(true).await.unwrap();
        replay.flush_history().await;
        assert_eq!(
            query(
                root.path(),
                &Filter {
                    printer: Some("bench".into()),
                    outcome: Some("done".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .len(),
            1
        );
        prepare(&next, root.path(), "lost").await;
        next.clear("bench").await.unwrap();
        next.start("lost").await.unwrap();
        next.flush_history().await;
        let empty = safety(backend(clock), root.path());
        empty
            .restore(&root.path().join("safety.json"))
            .await
            .unwrap();
        empty.poll_history(true).await.unwrap();
        empty.poll_history(true).await.unwrap();
        let rows = query(
            root.path(),
            &Filter {
                outcome: Some("unresolved_after_restart".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].job_id.as_deref(), Some("lost"));
    }
    #[tokio::test]
    async fn history_filters_limits_torn_tail_and_rotation() {
        let root = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        for id in 0..KEEP {
            serde_json::to_writer(&mut bytes, &record(id)).unwrap();
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(b"{\"torn\":");
        tokio::fs::write(root.path().join("history.jsonl"), bytes)
            .await
            .unwrap();
        assert_eq!(
            query(root.path(), &Filter::default()).await.unwrap().len(),
            100
        );
        append(root.path(), &record(KEEP)).await.unwrap();
        append(root.path(), &record(KEEP)).await.unwrap();
        let rows = query(
            root.path(),
            &Filter {
                limit: Some(KEEP),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), KEEP);
        assert_eq!(rows[0].job_id.as_deref(), Some("10000"));
        assert_eq!(rows.last().unwrap().job_id.as_deref(), Some("1"));
        let filter = Filter {
            printer: Some("a".into()),
            outcome: Some("done".into()),
            since: Some("2026-10-29T20:01:00-04:00".into()),
            limit: Some(2),
        };
        let rows = query(root.path(), &filter).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.printer == "a"));
        assert!(query(
            root.path(),
            &Filter {
                since: Some("2026-10-30T00:01:01Z".into()),
                ..filter.clone()
            }
        )
        .await
        .unwrap()
        .is_empty());
        for invalid in [
            Filter {
                limit: Some(0),
                ..Default::default()
            },
            Filter {
                since: Some("yesterday".into()),
                ..Default::default()
            },
            Filter {
                outcome: Some("unknown".into()),
                ..Default::default()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
        // A non-rotating append also repairs the torn tail before writing a line.
        tokio::fs::write(root.path().join("history.jsonl"), b"{\"torn\":")
            .await
            .unwrap();
        append(root.path(), &record(1)).await.unwrap();
        assert_eq!(
            query(root.path(), &Filter::default()).await.unwrap().len(),
            1
        );
        assert_eq!(label("https://host/path?token=secret"), "[redacted]");
        assert_eq!(label("C:\\private\\part.3mf"), "part.3mf");
    }
    #[tokio::test]
    async fn history_api_uses_filters_authentication_and_survives_restart() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt as _;
        let root = tempfile::tempdir().unwrap();
        append(root.path(), &record(1)).await.unwrap();
        for _ in 0..2 {
            let mut config = crate::ServerConfig::default();
            config.storage.data_dir = root.path().into();
            let app = crate::router(backend(Arc::new(TestClock::default())), config);
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/history?printer=b&outcome=done&limit=1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            let rows: Vec<Record> = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(rows.len(), 1);
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/history?limit=0")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let mut config = crate::ServerConfig::default();
        config.storage.data_dir = root.path().into();
        config.login = Some(("user".into(), "secret".into()));
        let app = crate::router(backend(Arc::new(TestClock::default())), config);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    fn sample(
        printer: &str,
        outcome: &str,
        started: bool,
        duration: Option<u64>,
        ended: &str,
    ) -> Record {
        let mut r = record(0);
        r.printer = printer.into();
        r.outcome = outcome.into();
        r.started_at = started.then(|| "2026-10-01T00:00:00Z".into());
        r.duration_seconds = duration;
        r.ended_at = ended.into();
        if outcome == "bed_clear" {
            r.job_id = None;
            r.job_name = None;
        }
        r
    }
    fn farm() -> Vec<Record> {
        vec![
            sample("a", "done", true, Some(100), "2026-10-01T01:00:00Z"),
            sample("a", "done", true, Some(300), "2026-10-03T01:00:00Z"),
            sample("a", "failed", true, None, "2026-10-02T01:00:00Z"),
            sample("a", "bed_clear", false, None, "2026-10-04T01:00:00Z"),
            sample("b", "cancelled", false, None, "2026-10-05T01:00:00Z"),
            sample("b", "stopped", true, Some(50), "2026-10-02T01:00:00Z"),
            sample("b", "ended", true, Some(10), "2026-10-02T02:00:00Z"),
            sample(
                "b",
                "unresolved_after_restart",
                true,
                None,
                "2026-10-02T03:00:00Z",
            ),
        ]
    }
    #[test]
    fn summary_counts_rates_times_and_bed_clears() {
        let s = summarize(&farm());
        let a = &s.printers[0];
        assert_eq!(a.printer, "a");
        assert_eq!((a.totals.jobs, a.totals.done, a.totals.failed), (3, 2, 1));
        assert_eq!(a.totals.bed_clears, 1);
        assert_eq!(a.totals.ran, 3);
        assert!((a.totals.success_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!((a.totals.timed_jobs, a.totals.total_seconds), (2, 400));
        assert_eq!(a.totals.average_seconds, Some(200.0));
        assert_eq!(
            a.totals.last_finished_at.as_deref(),
            Some("2026-10-03T01:00:00Z")
        );
        let b = &s.printers[1].totals;
        assert_eq!((b.jobs, b.cancelled, b.stopped, b.ended), (4, 1, 1, 1));
        assert_eq!(b.unresolved_after_restart, 1);
        assert_eq!(b.ran, 3);
        assert_eq!(b.success_rate, Some(0.0));
        assert_eq!(b.average_seconds, Some(30.0));
        assert_eq!(b.last_finished_at.as_deref(), Some("2026-10-05T01:00:00Z"));
        assert_eq!((s.total.jobs, s.total.done, s.total.bed_clears), (7, 2, 1));
        assert_eq!(s.total.total_seconds, 460);
        assert!((s.total.success_rate.unwrap() - 2.0 / 6.0).abs() < 1e-9);
        assert!(s.table().contains("TOTAL"));
    }
    #[test]
    fn summary_of_nothing_has_no_rates() {
        let s = summarize(&[sample(
            "a",
            "cancelled",
            false,
            None,
            "2026-10-01T00:00:00Z",
        )]);
        assert_eq!(s.total.jobs, 1);
        assert_eq!(s.total.success_rate, None);
        assert_eq!(s.total.average_seconds, None);
        assert_eq!(summarize(&[]).total, Totals::default().tap_finish());
    }
    impl Totals {
        fn tap_finish(mut self) -> Self {
            self.finish();
            self
        }
    }
    #[tokio::test]
    async fn summary_applies_printer_and_since_filters() {
        let root = tempfile::tempdir().unwrap();
        for r in farm() {
            append_unique(root.path(), r).await;
        }
        let all = summary(root.path(), &SummaryFilter::default())
            .await
            .unwrap();
        assert_eq!(all.total.jobs, 7);
        let only_b = SummaryFilter {
            printer: Some("b".into()),
            since: None,
        };
        let s = summary(root.path(), &only_b).await.unwrap();
        assert_eq!((s.printers.len(), s.total.jobs), (1, 4));
        let since = SummaryFilter {
            printer: None,
            since: Some("2026-10-03T00:00:00Z".into()),
        };
        let s = summary(root.path(), &since).await.unwrap();
        assert_eq!((s.total.jobs, s.total.done, s.total.bed_clears), (2, 1, 1));
        let bad = SummaryFilter {
            printer: None,
            since: Some("yesterday".into()),
        };
        assert!(summary(root.path(), &bad).await.is_err());
    }
    async fn append_unique(root: &Path, mut r: Record) {
        static N: AtomicU64 = AtomicU64::new(0);
        if r.job_id.is_some() {
            r.job_id = Some(N.fetch_add(1, Ordering::SeqCst).to_string());
        }
        append(root, &r).await.unwrap();
    }
    #[test]
    fn csv_has_header_and_quotes_commas_quotes_and_newlines() {
        let mut r = record(1);
        r.job_name = Some("a,b \"c\"\nd".into());
        r.job_id = None;
        r.estimate = Some(Estimate {
            seconds: Some(5),
            ..Estimate::default()
        });
        let text = csv(&[
            r,
            sample("a", "bed_clear", false, None, "2026-10-04T01:00:00Z"),
        ]);
        let mut lines = text.split("\r\n");
        assert_eq!(lines.next().unwrap(), CSV_HEADER);
        assert!(text.contains(",\"a,b \"\"c\"\"\nd\","));
        assert!(text.contains(",42,part.3mf,done,2026-10-30T00:00:00Z,2026-10-30T00:01:00Z,60,\"{\"\"seconds\"\":5}\"\r\n"));
        assert!(text.ends_with("\r\n"));
        assert_eq!(csv(&[]), format!("{CSV_HEADER}\r\n"));
    }
    #[tokio::test]
    async fn summary_and_csv_routes_use_authentication_and_content_types() {
        use axum::{
            body::Body,
            http::{header, Request, StatusCode},
        };
        use tower::ServiceExt as _;
        let root = tempfile::tempdir().unwrap();
        for r in farm() {
            append_unique(root.path(), r).await;
        }
        let get = |app: axum::Router, uri: &'static str| async move {
            let response = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let kind = response
                .headers()
                .get(header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap().to_string());
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, kind, String::from_utf8(bytes.to_vec()).unwrap())
        };
        let config = |login: bool| {
            let mut config = crate::ServerConfig::default();
            config.storage.data_dir = root.path().into();
            if login {
                config.login = Some(("user".into(), "secret".into()));
            }
            config
        };
        let app = crate::router(backend(Arc::new(TestClock::default())), config(false));
        let (status, kind, body) = get(app.clone(), "/api/v1/history/summary?printer=a").await;
        assert_eq!(status, StatusCode::OK);
        assert!(kind.unwrap().starts_with("application/json"));
        let s: Summary = serde_json::from_str(&body).unwrap();
        assert_eq!((s.total.jobs, s.printers.len()), (3, 1));
        let (status, _, _) = get(app.clone(), "/api/v1/history/summary?since=nope").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = get(app.clone(), "/api/v1/history/summary?outcome=done").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, kind, body) = get(app.clone(), "/api/v1/history?format=csv&printer=b").await;
        assert_eq!(status, StatusCode::OK);
        assert!(kind.unwrap().starts_with("text/csv"));
        assert!(body.starts_with(CSV_HEADER));
        assert_eq!(body.trim_end().split("\r\n").count(), 5);
        let (status, kind, _) = get(app.clone(), "/api/v1/history?format=json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(kind.unwrap().starts_with("application/json"));
        let (status, _, _) = get(app, "/api/v1/history?format=xml").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let app = crate::router(backend(Arc::new(TestClock::default())), config(true));
        for uri in ["/api/v1/history/summary", "/api/v1/history?format=csv"] {
            let (status, _, _) = get(app.clone(), uri).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
    }
}
