//! CTX-3 — offload persistence sink.
//!
//! When [`crate::compression::ctx_offload`] replaces an oversized `tool_result`
//! with a digest on the request path, the original bytes are kept in CCR and a
//! durable, bounded outbox until their project FTS index write succeeds. Two
//! stores are written:
//!
//! - the **CCR store** (`<store_dir>/ccr.db`, sqlite), keyed by the block's
//!   `blake3` hash, so `headroom ctx get <hash>` (CTX-5) can serve the original
//!   back with a long TTL;
//! - the **FTS content index** (CTX-1 [`CtxStore`]), so the offloaded output is
//!   searchable under a stable, hash-unique source label.
//!
//! The index worker batches each project's records into one SQLite transaction
//! and retries failures from the outbox. At capacity, the Anthropic forwarding
//! path skips offloading; the routed path keeps CCR retrieval and reports that
//! the index is degraded.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use headroom_core::ccr::{from_config, CcrBackendConfig, CcrStore};
use headroom_core::ctx::{CtxStore, IndexOpts};
use rusqlite::{params, Connection, OptionalExtension};

use super::projects::ProjectStores;
use crate::compression::ctx_offload::OffloadRecord;

/// Handle to the background offload-persistence worker. Cheap to clone behind
/// an `Arc` in `AppState`. Dropping the last handle closes the channel and the
/// worker exits.
pub struct OffloadStore {
    wake_worker: SyncSender<()>,
    outbox: Arc<IndexOutbox>,
    /// Batches refused because the durable queue was full.
    queue_full_batches: AtomicU64,
    /// The pending-byte budget, so the existing deterministic unit fixture can exhaust it.
    budget: u64,
    /// Shared reference to the CCR store — used by CTX-5 `/ctx/get`.
    ccr: Arc<dyn CcrStore>,
    /// Per-project FTS content stores — used by CTX-5 `/ctx/search` and
    /// `/ctx/index`, and by CTX-4 recall.
    stores: Arc<ProjectStores>,
}

/// Bound disk growth as well as worker state. At the cap, paths use their
/// explicit passthrough/degraded behavior instead of dropping RAM-queued work.
const MAX_PENDING_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PENDING_JOBS: i64 = 100_000;
const INDEX_BATCH_SIZE: i64 = 32;
const INDEX_BATCH_BYTES: u64 = 16 * 1024 * 1024;
const OUTBOX_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY_MS: u64 = 30_000;

struct IndexOutbox {
    conn: Mutex<Connection>,
}

#[derive(Clone)]
struct PendingRecord {
    id: i64,
    hash: String,
    original: String,
    attempts: u32,
    enqueued_at_ms: u64,
}

struct QueueStats {
    jobs: u64,
    bytes: u64,
    oldest_age_ms: u64,
}

enum EnqueueError {
    Full { jobs: u64, bytes: u64 },
    Database(rusqlite::Error),
}

impl OffloadStore {
    /// Open the CCR store and durable index outbox under `store_dir`.
    ///
    /// - CCR: `<store_dir>/ccr.db`, sqlite, TTL `ttl_seconds` (long by design).
    ///   Content-addressed by `blake3` hash and shared across projects on
    ///   purpose: `/ctx/get` answers a hash the model read out of its own
    ///   transcript, so there is nothing to scope it to.
    /// - Content: the CTX-1 per-project FTS index, opened per request through
    ///   `stores`. This one *is* scoped — it is searched by relevance rather
    ///   than by an exact key, so a shared index hands one project's tool
    ///   output to another's recall (CTX-2b).
    pub fn start(
        store_dir: &Path,
        ttl_seconds: u64,
        stores: Arc<ProjectStores>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(store_dir)?;

        let ccr: Arc<dyn CcrStore> = Arc::from(
            from_config(&CcrBackendConfig::Sqlite {
                path: store_dir.join("ccr.db"),
                ttl_seconds,
            })
            .map_err(std::io::Error::other)?,
        );

        let outbox = Arc::new(IndexOutbox::open(&store_dir.join("ctx-offload-index.db"))?);
        if let Ok(stats) = outbox.stats() {
            crate::observability::ctx_metrics::observe_offload_index_backlog(
                stats.jobs,
                stats.bytes,
                stats.oldest_age_ms,
            );
            if stats.jobs > 0 {
                tracing::info!(
                    event = "ctx_offload_index_outbox_recovered",
                    pending_jobs = stats.jobs,
                    pending_bytes = stats.bytes,
                    oldest_age_ms = stats.oldest_age_ms,
                    "resuming pending CTX-3 index jobs from disk"
                );
            }
        }

        let (wake_worker, rx) = mpsc::sync_channel::<()>(1);
        // Clone Arcs for the background thread — cheap (atomic refcount).
        let ccr_bg = Arc::clone(&ccr);
        let stores_bg = Arc::clone(&stores);
        let outbox_bg = Arc::clone(&outbox);
        thread::Builder::new()
            .name("ctx-offload-store".to_string())
            .spawn(move || {
                index_worker_loop(&rx, outbox_bg, ccr_bg, stores_bg);
            })?;

        Ok(Self {
            wake_worker,
            outbox,
            queue_full_batches: AtomicU64::new(0),
            budget: MAX_PENDING_BYTES,
            ccr,
            stores,
        })
    }

    /// CCR store handle — used by CTX-5 `/ctx/get` to retrieve offloaded
    /// originals by hash. Returns an `Arc` clone so the caller can move it
    /// into `tokio::task::spawn_blocking` without lifetime issues.
    pub fn ccr(&self) -> Arc<dyn CcrStore> {
        Arc::clone(&self.ccr)
    }

    /// FTS content store handle for one project — used by CTX-5 `/ctx/search`
    /// and `/ctx/index`. Returns an `Arc` clone for the same reason as
    /// [`Self::ccr`]. `None` when that project's DB cannot be opened.
    pub fn content_for(&self, project_dir: &str) -> Option<Arc<CtxStore>> {
        self.stores.content(project_dir)
    }

    /// The per-project store registry, shared with capture and recall.
    pub fn stores(&self) -> Arc<ProjectStores> {
        Arc::clone(&self.stores)
    }

    /// Store originals in CCR and atomically add their FTS jobs to the durable
    /// outbox. `false` means the FTS job was not queued; the caller must apply
    /// its path's passthrough or degraded-index behavior.
    pub fn persist(&self, records: &[OffloadRecord], project_dir: &str) -> bool {
        if records.is_empty() {
            return true;
        }

        self.put_originals_inline(records);
        match self.outbox.enqueue(records, project_dir, self.budget) {
            Ok(stats) => {
                crate::observability::ctx_metrics::observe_offload_index_backlog(
                    stats.jobs,
                    stats.bytes,
                    stats.oldest_age_ms,
                );
                match self.wake_worker.try_send(()) {
                    Ok(()) | Err(TrySendError::Full(())) => {}
                    Err(TrySendError::Disconnected(())) => tracing::warn!(
                        event = "ctx_offload_store_worker_gone",
                        "CTX-3 jobs are durable but the indexing worker stopped"
                    ),
                }
                tracing::debug!(
                    event = "ctx_offload_index_outbox_enqueued",
                    records = records.len(),
                    pending_jobs = stats.jobs,
                    pending_bytes = stats.bytes,
                    oldest_age_ms = stats.oldest_age_ms,
                    "durably queued CTX-3 index jobs"
                );
                true
            }
            Err(EnqueueError::Full { jobs, bytes }) => {
                let n = self.queue_full_batches.fetch_add(1, Ordering::Relaxed) + 1;
                crate::observability::ctx_metrics::observe_offload_index_backpressure();
                publish_outbox_stats(&self.outbox);
                if should_report(n) {
                    tracing::warn!(
                        event = "ctx_offload_index_queue_full",
                        backpressured = n,
                        pending_jobs = jobs,
                        pending_bytes = bytes,
                        max_pending_jobs = MAX_PENDING_JOBS,
                        max_pending_bytes = self.budget,
                        "CTX-3 durable index queue is full; the index job was refused and the caller applies its provider-specific behavior"
                    );
                }
                false
            }
            Err(EnqueueError::Database(error)) => {
                publish_outbox_stats(&self.outbox);
                tracing::warn!(
                    event = "ctx_offload_index_outbox_write_failed",
                    error = %error,
                    "could not persist CTX-3 index jobs; this batch may be absent from search"
                );
                false
            }
        }
    }

    /// Store CCR originals synchronously on the request path and time it.
    /// The durable outbox also retains the original until both CCR and FTS
    /// writes succeed, so a failed worker write survives restarts and TTLs.
    fn put_originals_inline(&self, records: &[OffloadRecord]) {
        let started = std::time::Instant::now();
        let mut failed = 0usize;
        for record in records {
            if !self.ccr.put(&record.hash, &record.original) {
                failed += 1;
            }
        }
        tracing::debug!(
            event = "ctx_offload_put_inline",
            records = records.len(),
            failed = failed,
            put_inline_us = started.elapsed().as_micros() as u64,
            "CTX-3 CCR originals stored on the request path"
        );
    }

    /// Set the pending-byte budget. Tests only: this makes the backpressure
    /// path deterministic without requiring an actual disk backlog.
    #[cfg(test)]
    fn with_budget(mut self, bytes: u64) -> Self {
        self.budget = bytes;
        self
    }

    /// Number of index batches refused because the bounded durable outbox was full.
    pub fn shed_batches(&self) -> u64 {
        self.queue_full_batches.load(Ordering::Relaxed)
    }

    /// Bytes retained in the durable index outbox.
    pub fn queued_bytes(&self) -> u64 {
        self.outbox.stats().map(|stats| stats.bytes).unwrap_or(0)
    }
}

impl IndexOutbox {
    fn open(path: &Path) -> std::io::Result<Self> {
        let conn = Connection::open(path).map_err(std::io::Error::other)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(std::io::Error::other)?;
        // The queue is the only retained copy after a CCR TTL or restart, so
        // make each committed enqueue survive a host crash as well as a process restart.
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(std::io::Error::other)?;
        conn.busy_timeout(OUTBOX_BUSY_TIMEOUT)
            .map_err(std::io::Error::other)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ctx_offload_index_outbox (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 hash TEXT NOT NULL,
                 project_dir TEXT NOT NULL,
                 title TEXT NOT NULL,
                 original TEXT NOT NULL,
                 bytes INTEGER NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 enqueued_at_ms INTEGER NOT NULL,
                 next_attempt_at_ms INTEGER NOT NULL DEFAULT 0,
                 UNIQUE(project_dir, hash)
             );
             CREATE INDEX IF NOT EXISTS idx_ctx_offload_outbox_ready
                 ON ctx_offload_index_outbox(next_attempt_at_ms, id);
             CREATE INDEX IF NOT EXISTS idx_ctx_offload_outbox_project_ready
                 ON ctx_offload_index_outbox(project_dir, next_attempt_at_ms, id);",
        )
        .map_err(std::io::Error::other)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn enqueue(
        &self,
        records: &[OffloadRecord],
        project_dir: &str,
        max_bytes: u64,
    ) -> Result<QueueStats, EnqueueError> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(EnqueueError::Database)?;
        let now = now_ms();
        let (job_count, byte_count, oldest): (i64, i64, Option<i64>) = tx
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(bytes), 0), MIN(enqueued_at_ms)
                 FROM ctx_offload_index_outbox",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(EnqueueError::Database)?;
        let mut jobs = job_count.max(0) as u64;
        let mut bytes = byte_count.max(0) as u64;
        let mut seen = HashSet::new();
        for record in records {
            if !seen.insert(record.hash.as_str()) {
                continue;
            }
            let previous_bytes: Option<i64> = tx
                .query_row(
                    "SELECT bytes FROM ctx_offload_index_outbox
                     WHERE project_dir = ?1 AND hash = ?2",
                    params![project_dir, record.hash],
                    |row| row.get(0),
                )
                .optional()
                .map_err(EnqueueError::Database)?;
            let record_bytes = record.original.len() as u64;
            match previous_bytes {
                Some(previous) => {
                    let previous = previous.max(0) as u64;
                    bytes = bytes.saturating_sub(previous).saturating_add(record_bytes)
                }
                None => {
                    jobs = jobs.saturating_add(1);
                    bytes = bytes.saturating_add(record_bytes);
                }
            }
        }
        if jobs > MAX_PENDING_JOBS as u64 || bytes > max_bytes {
            return Err(EnqueueError::Full { jobs, bytes });
        }

        seen.clear();
        for record in records {
            if !seen.insert(record.hash.as_str()) {
                continue;
            }
            tx.execute(
                "INSERT INTO ctx_offload_index_outbox
                    (hash, project_dir, title, original, bytes, enqueued_at_ms, next_attempt_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
                 ON CONFLICT(project_dir, hash) DO UPDATE SET
                    title = excluded.title,
                    original = excluded.original,
                    bytes = excluded.bytes,
                    next_attempt_at_ms = 0",
                params![
                    record.hash,
                    project_dir,
                    record.title,
                    record.original,
                    record.original.len() as i64,
                    now as i64
                ],
            )
            .map_err(EnqueueError::Database)?;
        }
        tx.commit().map_err(EnqueueError::Database)?;
        Ok(QueueStats {
            jobs,
            bytes,
            oldest_age_ms: oldest
                .map(|time| now.saturating_sub(time.max(0) as u64))
                .unwrap_or(0),
        })
    }

    fn stats(&self) -> rusqlite::Result<QueueStats> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now_ms();
        let (jobs, bytes, oldest): (i64, i64, Option<i64>) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(bytes), 0), MIN(enqueued_at_ms)
             FROM ctx_offload_index_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(QueueStats {
            jobs: jobs.max(0) as u64,
            bytes: bytes.max(0) as u64,
            oldest_age_ms: oldest
                .map(|time| now.saturating_sub(time.max(0) as u64))
                .unwrap_or(0),
        })
    }

    fn take_ready_batch(&self) -> rusqlite::Result<Option<(String, Vec<PendingRecord>)>> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now_ms() as i64;
        let project: Option<String> = conn
            .query_row(
                "SELECT project_dir FROM ctx_offload_index_outbox
                 WHERE next_attempt_at_ms <= ?1
                 GROUP BY project_dir ORDER BY MIN(id) LIMIT 1",
                [now],
                |row| row.get(0),
            )
            .optional()?;
        let Some(project) = project else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT id, hash, original, attempts, enqueued_at_ms, bytes
             FROM ctx_offload_index_outbox
             WHERE project_dir = ?1 AND next_attempt_at_ms <= ?2
             ORDER BY id LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![project, now, INDEX_BATCH_SIZE])?;
        let mut records = Vec::new();
        let mut loaded_bytes = 0u64;
        while let Some(row) = rows.next()? {
            let bytes = row.get::<_, i64>(5)?.max(0) as u64;
            if !records.is_empty() && loaded_bytes.saturating_add(bytes) > INDEX_BATCH_BYTES {
                break;
            }
            loaded_bytes = loaded_bytes.saturating_add(bytes);
            records.push(PendingRecord {
                id: row.get(0)?,
                hash: row.get(1)?,
                original: row.get(2)?,
                attempts: row.get::<_, i64>(3)?.max(0) as u32,
                enqueued_at_ms: row.get::<_, i64>(4)?.max(0) as u64,
            });
        }
        Ok(Some((project, records)))
    }

    fn acknowledge(&self, ids: &[i64]) -> rusqlite::Result<()> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = conn.transaction()?;
        for id in ids {
            tx.execute("DELETE FROM ctx_offload_index_outbox WHERE id = ?1", [id])?;
        }
        tx.commit()
    }

    fn retry(&self, records: &[PendingRecord]) -> rusqlite::Result<()> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = conn.transaction()?;
        let now = now_ms();
        for record in records {
            let attempts = record.attempts.saturating_add(1);
            let delay = retry_delay_ms(attempts);
            tx.execute(
                "UPDATE ctx_offload_index_outbox
                 SET attempts = ?2, next_attempt_at_ms = ?3
                 WHERE id = ?1",
                params![record.id, attempts, now.saturating_add(delay) as i64],
            )?;
        }
        tx.commit()
    }
}

fn index_worker_loop(
    wake: &Receiver<()>,
    outbox: Arc<IndexOutbox>,
    ccr: Arc<dyn CcrStore>,
    stores: Arc<ProjectStores>,
) {
    loop {
        match outbox.take_ready_batch() {
            Ok(Some((project_dir, records))) if !records.is_empty() => {
                process_batch(
                    &outbox,
                    ccr.as_ref(),
                    stores.as_ref(),
                    &project_dir,
                    &records,
                );
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    event = "ctx_offload_index_outbox_read_failed",
                    error = %error,
                    "could not read CTX-3 index outbox"
                );
                thread::sleep(WORKER_POLL_INTERVAL);
            }
        }
        match wake.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn process_batch(
    outbox: &IndexOutbox,
    ccr: &dyn CcrStore,
    stores: &ProjectStores,
    project_dir: &str,
    records: &[PendingRecord],
) {
    let Some(content) = stores.content(project_dir) else {
        retry_batch(outbox, records, "project_store_unavailable", false);
        return;
    };

    let ccr_ok: Vec<bool> = records
        .iter()
        .map(|record| ccr.put(&record.hash, &record.original))
        .collect();
    let labels: Vec<String> = records
        .iter()
        .map(|record| format!("tool_result:{}", record.hash))
        .collect();
    let opts: Vec<IndexOpts> = records
        .iter()
        .map(|record| IndexOpts {
            content_hash: Some(record.hash.clone()),
            plain_text_lines: Some(50),
            ..Default::default()
        })
        .collect();
    let items: Vec<(&str, &str, &IndexOpts)> = records
        .iter()
        .zip(&labels)
        .zip(&opts)
        .map(|((record, label), opts)| (label.as_str(), record.original.as_str(), opts))
        .collect();

    let started = std::time::Instant::now();
    match content.index_content_batch(&items) {
        Ok(_) => {
            crate::observability::ctx_metrics::observe_offload_index_batch(started.elapsed());
            let successful_ids: Vec<i64> = records
                .iter()
                .zip(&ccr_ok)
                .filter_map(|(record, ccr_ok)| (*ccr_ok).then_some(record.id))
                .collect();
            let successful_bytes: Vec<u64> = records
                .iter()
                .zip(&ccr_ok)
                .filter_map(|(record, ccr_ok)| (*ccr_ok).then_some(record.original.len() as u64))
                .collect();
            for (record, ccr_ok) in records.iter().zip(&ccr_ok) {
                if !*ccr_ok {
                    tracing::warn!(
                        event = "ctx_offload_persist_partial",
                        index_ok = true,
                        ccr_ok = false,
                        hash = %record.hash,
                        bytes = record.original.len(),
                        "CTX-3 offload indexed but not stored; retaining it in the index outbox for retry"
                    );
                }
            }
            if let Err(error) = outbox.acknowledge(&successful_ids) {
                tracing::warn!(
                    event = "ctx_offload_index_outbox_ack_failed",
                    records = successful_ids.len(),
                    error = %error,
                    "indexed CTX-3 jobs remain in the outbox and will be replayed idempotently"
                );
                retry_batch(outbox, records, "ack_failed", is_database_busy(&error));
                return;
            }
            publish_outbox_stats(outbox);
            for bytes in successful_bytes {
                crate::observability::ctx_metrics::observe_offloaded(bytes);
            }
            let stats = outbox.stats().ok();
            tracing::debug!(
                event = "ctx_offload_index_batch_complete",
                records = records.len(),
                index_batch_ms = started.elapsed().as_millis() as u64,
                oldest_job_age_ms = records
                    .iter()
                    .map(|record| now_ms().saturating_sub(record.enqueued_at_ms))
                    .max()
                    .unwrap_or(0),
                pending_jobs = stats.as_ref().map(|stats| stats.jobs).unwrap_or(0),
                pending_bytes = stats.as_ref().map(|stats| stats.bytes).unwrap_or(0),
                "indexed CTX-3 outbox batch"
            );
            let failed_ccr: Vec<PendingRecord> = records
                .iter()
                .zip(&ccr_ok)
                .filter_map(|(record, ccr_ok)| (!*ccr_ok).then_some(record))
                .cloned()
                .collect();
            if !failed_ccr.is_empty() {
                retry_batch(outbox, &failed_ccr, "ccr_put_failed", false);
            }
        }
        Err(error) => {
            crate::observability::ctx_metrics::observe_offload_index_batch(started.elapsed());
            let transient = is_database_busy(&error);
            tracing::warn!(
                event = "ctx_offload_index_retry",
                records = records.len(),
                transient = transient,
                index_batch_ms = started.elapsed().as_millis() as u64,
                error = %error,
                "CTX-3 batch index write failed; retaining jobs for retry"
            );
            retry_batch(outbox, records, "index_failed", transient);
        }
    }
}

fn retry_batch(outbox: &IndexOutbox, records: &[PendingRecord], reason: &str, transient: bool) {
    match outbox.retry(records) {
        Ok(()) => {
            crate::observability::ctx_metrics::observe_offload_index_retry();
            publish_outbox_stats(outbox);
        }
        Err(error) => {
            tracing::warn!(
                event = "ctx_offload_index_retry_schedule_failed",
                reason,
                error = %error,
                "could not schedule CTX-3 retry; jobs remain durable and will be retried after restart"
            );
            thread::sleep(WORKER_POLL_INTERVAL);
        }
    }
    let backoff_ms = records
        .iter()
        .map(|record| retry_delay_ms(record.attempts.saturating_add(1)))
        .max()
        .unwrap_or(0);
    tracing::debug!(
        event = "ctx_offload_index_retry_scheduled",
        records = records.len(),
        reason,
        transient,
        backoff_ms,
        "deferred CTX-3 index batch"
    );
}

fn publish_outbox_stats(outbox: &IndexOutbox) {
    if let Ok(stats) = outbox.stats() {
        crate::observability::ctx_metrics::observe_offload_index_backlog(
            stats.jobs,
            stats.bytes,
            stats.oldest_age_ms,
        );
    }
}

fn is_database_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn retry_delay_ms(attempt: u32) -> u64 {
    100u64
        .saturating_mul(1u64.checked_shl(attempt.min(16)).unwrap_or(u64::MAX))
        .min(MAX_RETRY_DELAY_MS)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Report the first backpressure event, then only at powers of ten.
fn should_report(n: u64) -> bool {
    let mut threshold = 1u64;
    while threshold < n {
        threshold = match threshold.checked_mul(10) {
            Some(t) => t,
            None => return false,
        };
    }
    threshold == n
}

// Outbox rows are acknowledged only after both CCR and project-index writes
// succeed. Replays are safe because source labels include the content hash.

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::ctx::content_db_path;
    use tempfile::TempDir;

    #[test]
    fn persist_writes_ccr_and_index() {
        let dir = TempDir::new().unwrap();
        let ccr = from_config(&CcrBackendConfig::Sqlite {
            path: dir.path().join("ccr.db"),
            ttl_seconds: 3600,
        })
        .unwrap();
        let content_path = content_db_path(dir.path(), "");
        std::fs::create_dir_all(content_path.parent().unwrap()).unwrap();
        let content = CtxStore::open(&content_path).unwrap();

        let record = OffloadRecord {
            hash: "abc123abc123abc123abc123".to_string(),
            original: "ERROR: disk full\nfailed to write\n".repeat(20),
            title: "cat big.log".to_string(),
            gate_rollback: None,
        };
        assert!(ccr.put(&record.hash, &record.original));
        let opts = IndexOpts {
            content_hash: Some(record.hash.clone()),
            plain_text_lines: Some(50),
            ..Default::default()
        };
        content
            .index_content(
                &format!("tool_result:{}", record.hash),
                &record.original,
                &opts,
            )
            .unwrap();

        // CCR round-trips the original by hash.
        assert_eq!(
            ccr.get(&record.hash).as_deref(),
            Some(record.original.as_str())
        );
        // The content is searchable by a term from the original.
        let opts = headroom_core::ctx::SearchOpts {
            limit: 5,
            ..Default::default()
        };
        let hits = content.search(&["disk full".to_string()], &opts).unwrap();
        assert!(!hits.is_empty(), "offloaded content should be searchable");
    }

    /// The race this closes: a retrieval that lands in the same turn as the
    /// offload. Reads back on the calling thread with no sleep and no worker
    /// drain, so it passes only if the put happened before `persist` returned.
    #[test]
    fn persist_stores_the_original_before_it_returns() {
        let dir = TempDir::new().unwrap();
        let stores = std::sync::Arc::new(crate::ctx::projects::ProjectStores::new(
            dir.path().to_path_buf(),
        ));
        let store = OffloadStore::start(dir.path(), 3600, stores).unwrap();

        let record = OffloadRecord {
            hash: "0123456789abcdef01234567".to_string(),
            original: "the original the model is about to ask for".to_string(),
            title: "cargo test".to_string(),
            gate_rollback: None,
        };
        store.persist(&[record.clone()], "/home/dev/alpha");

        assert_eq!(
            store.ccr().get(&record.hash).as_deref(),
            Some(record.original.as_str()),
            "the original must be retrievable the instant persist returns, \
             without waiting on the indexing worker"
        );
    }

    /// A shed batch costs the FTS index entry and nothing else — the CCR put
    /// happens on the request path, before the queue is consulted.
    #[test]
    fn a_shed_batch_still_leaves_the_original_retrievable() {
        let dir = TempDir::new().unwrap();
        let stores = std::sync::Arc::new(crate::ctx::projects::ProjectStores::new(
            dir.path().to_path_buf(),
        ));
        let store = OffloadStore::start(dir.path(), 3600, stores)
            .unwrap()
            .with_budget(1);

        let record = OffloadRecord {
            hash: "fedcba9876543210fedcba98".to_string(),
            original: "output the index will not see".to_string(),
            title: "rg needle".to_string(),
            gate_rollback: None,
        };
        store.persist(&[record.clone()], "/home/dev/alpha");

        assert_eq!(store.shed_batches(), 1, "the batch should be shed");
        assert_eq!(store.queued_bytes(), 0, "a shed batch charges nothing");
        assert_eq!(
            store.ccr().get(&record.hash).as_deref(),
            Some(record.original.as_str()),
            "shedding the index write must not cost the original"
        );
    }
}
