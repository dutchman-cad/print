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
}
