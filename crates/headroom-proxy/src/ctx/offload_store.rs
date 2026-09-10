//! CTX-3 — offload persistence sink.
//!
//! When [`crate::compression::ctx_offload`] replaces an oversized `tool_result`
//! with a digest on the request path, the original bytes are handed here to be
//! persisted **off** that path — never blocking or slowing the live request.
//! Two stores are written:
//!
//! - the **CCR store** (`<store_dir>/ccr.db`, sqlite), keyed by the block's
//!   `blake3` hash, so `headroom ctx get <hash>` (CTX-5) can serve the original
//!   back with a long TTL;
//! - the **FTS content index** (CTX-1 [`CtxStore`]), so the offloaded output is
//!   searchable, with the paired `tool_use` command as the chunk title.
//!
//! Storage is best-effort and decoupled from cache-safety: the digest on the
//! wire is recomputable from the block's own bytes (invariant I1/I2), so a
//! store miss/TTL-expiry affects only *retrieval*, never the wire bytes. Every
//! failure is logged loudly (no silent fallbacks) and swallowed.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use headroom_core::ccr::{from_config, CcrBackendConfig, CcrStore};
use headroom_core::ctx::{CtxStore, IndexOpts};

use super::projects::ProjectStores;
use crate::compression::ctx_offload::OffloadRecord;

/// Handle to the background offload-persistence worker. Cheap to clone behind
/// an `Arc` in `AppState`. Dropping the last handle closes the channel and the
/// worker exits.
pub struct OffloadStore {
    tx: Sender<Batch>,
    /// Bytes of parked originals the worker has not taken yet.
    queued_bytes: Arc<AtomicU64>,
    /// Batches shed because the queue was already at its budget.
    shed: AtomicU64,
    /// The budget itself, so a test can set one it can exhaust.
    budget: u64,
    /// Shared reference to the CCR store — used by CTX-5 `/ctx/get`.
    ccr: Arc<dyn CcrStore>,
    /// Per-project FTS content stores — used by CTX-5 `/ctx/search` and
    /// `/ctx/index`, and by CTX-4 recall.
    stores: Arc<ProjectStores>,
}

/// How many bytes of parked originals the FTS queue may hold before it sheds
/// rather than grows. Each record carries a full copy of the tool output it
/// replaced, and the worker chunks and indexes them one at a time while the
/// request path keeps handing it more — the queue was measured minutes deep on
/// live traffic (see `persist`). On 2026-09-10 that unbounded backlog was the
/// likeliest of two paths by which a proxy reached 26 GB RSS and the OOM
/// killer took the WSL VM with it.
const MAX_QUEUED_BYTES: u64 = 128 * 1024 * 1024;

/// A batch of originals plus the project whose index they belong in.
struct Batch {
    records: Vec<OffloadRecord>,
    project_dir: String,
    /// Bytes of originals in this batch, charged against `MAX_QUEUED_BYTES`
    /// while it waits and refunded when the worker takes it.
    bytes: u64,
}

impl OffloadStore {
    /// Open the CCR store under `store_dir` and spawn the worker.
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

        let (tx, rx) = mpsc::channel::<Batch>();
        let queued_bytes = Arc::new(AtomicU64::new(0));
        // Clone Arcs for the background thread — cheap (atomic refcount).
        let ccr_bg = Arc::clone(&ccr);
        let stores_bg = Arc::clone(&stores);
        let queued_bg = Arc::clone(&queued_bytes);
        thread::Builder::new()
            .name("ctx-offload-store".to_string())
            .spawn(move || {
                for batch in rx {
                    // Refund on receipt: the batch is off the queue, and what
                    // it still holds is one batch, not a backlog.
                    queued_bg.fetch_sub(batch.bytes, Ordering::Relaxed);
                    let Some(content) = stores_bg.content(&batch.project_dir) else {
                        // The registry logged why. Persistence is best-effort
                        // and the wire bytes are already correct.
                        continue;
                    };
                    for record in batch.records {
                        let bytes = record.original.len() as u64;
                        if persist_one(ccr_bg.as_ref(), &content, &record) {
                            crate::observability::ctx_metrics::observe_offloaded(bytes);
                        }
                    }
                }
            })?;

        Ok(Self {
            tx,
            queued_bytes,
            shed: AtomicU64::new(0),
            budget: MAX_QUEUED_BYTES,
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

    /// Store offloaded originals: CCR inline, FTS index on the worker.
    ///
    /// The CCR put has to happen before this returns. Both halves used to run
    /// on the worker, and the worker also does the FTS indexing, which chunks
    /// each block and writes two full-text tables plus vocabulary. On live
    /// traffic that queue ran minutes deep: measured against each request's own
    /// offload event, the model asked for a block a median of 2s later while
    /// the CCR row landed a median of 589s later. Every retrieval of a block
    /// offloaded in the same turn therefore missed a store that was about to
    /// hold it — 28 of the 29 misses in a day of logs, and none of them an
    /// expiry. The put is a keyed upsert benchmarked at ~2µs, so it is
    /// affordable here; the indexing is what has to stay off the request path.
    ///
    /// A failed put is logged by the backend (`ccr_sqlite_put_failed`) and the
    /// record is enqueued regardless, so the worker's own put retries it. The
    /// request never fails for this: the wire bytes are already correct and a
    /// lost original costs retrieval, not correctness.
    pub fn persist(&self, records: Vec<OffloadRecord>, project_dir: &str) {
        if records.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let mut failed = 0usize;
        for record in &records {
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

        // Shed rather than queue when the worker is already `MAX_QUEUED_BYTES`
        // behind. What is lost is the FTS index entry, not the original: the
        // CCR put above already happened on this path, so `headroom ctx get`
        // still serves every one of these blocks by hash. Search misses them
        // until they are re-indexed.
        // Charge first, refund if that broke the budget — see the same guard in
        // `ctx::observer`: a read-then-add lets a burst of threads past a check
        // none of them would pass together.
        let bytes: u64 = records.iter().map(|r| r.original.len() as u64).sum();
        if self.queued_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes > self.budget {
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
            let n = self.shed.fetch_add(1, Ordering::Relaxed) + 1;
            if should_report(n) {
                tracing::warn!(
                    event = "ctx_offload_index_queue_full",
                    shed = n,
                    queued_bytes = self.queued_bytes.load(Ordering::Relaxed),
                    max_queued_bytes = self.budget,
                    "CTX-3 index queue at its byte budget; these originals stay retrievable by hash but unindexed"
                );
            }
            return;
        }

        let batch = Batch {
            records,
            project_dir: project_dir.to_string(),
            bytes,
        };
        if self.tx.send(batch).is_err() {
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
            tracing::warn!(
                event = "ctx_offload_store_worker_gone",
                "CTX-3 offload-store worker unavailable; dropping persistence"
            );
        }
    }

    /// Set the queue's byte budget. Tests only: a real backlog depends on
    /// losing a race with a worker that drains in microseconds, so the shed
    /// path is deterministic only when nothing fits.
    #[cfg(test)]
    fn with_budget(mut self, bytes: u64) -> Self {
        self.budget = bytes;
        self
    }

    /// Batches shed because the index queue was at its byte budget.
    pub fn shed_batches(&self) -> u64 {
        self.shed.load(Ordering::Relaxed)
    }

    /// Bytes of parked originals waiting on the index worker.
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes.load(Ordering::Relaxed)
    }
}

/// Report the first shed, then only at powers of ten — a full queue repeats
/// per request, and the count on each line carries what the repeats would.
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

/// Store one record in both backends. Off the request path; failures are
/// logged and swallowed so persistence never crashes the worker. Returns
/// `true` only if the record is durably recoverable via `/ctx/get` and
/// `/ctx/search` — the offload metrics (CTX-6) count on this, not on the
/// request-path transform, so `ctx_offloaded_bytes_total` reflects bytes
/// actually persisted rather than bytes merely enqueued.
fn persist_one(ccr: &dyn CcrStore, content: &CtxStore, record: &OffloadRecord) -> bool {
    // CCR: idempotent put keyed by the hash embedded in the wire digest.
    // `headroom ctx get` reads only from here, so a failed put means the
    // record isn't retrievable even if the FTS index write below succeeds.
    let ccr_ok = ccr.put(&record.hash, &record.original);

    // FTS index: title = paired tool_use command (deterministic); tie the
    // source to the hash via `content_hash` so re-indexing the same block is a
    // no-op dedup (index_content replaces by label). Use the plain-text chunker
    // since tool output is not markdown.
    let label = if record.title.is_empty() {
        format!("tool_result:{}", record.hash)
    } else {
        record.title.clone()
    };
    let opts = IndexOpts {
        content_hash: Some(record.hash.clone()),
        plain_text_lines: Some(50),
        ..Default::default()
    };
    if let Err(e) = content.index_content(&label, &record.original, &opts) {
        tracing::warn!(
            event = "ctx_offload_persist_partial",
            index_ok = false,
            ccr_ok,
            event_detail = "index_failed",
            hash = %record.hash,
            error = %e,
            "CTX-3 offload FTS index failed"
        );
        return false;
    }
    // The half-failure item 12.3 names: the index write landed, the CCR put did
    // not. `/ctx/search` will find this record and `/ctx/get` will not return
    // it, and until now the two halves shared one return value with no line
    // saying which one broke. Only the anomaly is logged — a record that
    // persisted correctly is the common case and would be one line per
    // offloaded block in a log that is never rotated.
    if !ccr_ok {
        tracing::warn!(
            event = "ctx_offload_persist_partial",
            index_ok = true,
            ccr_ok = false,
            hash = %record.hash,
            bytes = record.original.len(),
            "CTX-3 offload indexed but not stored: searchable, not retrievable"
        );
    }
    ccr_ok
}

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
        };
        persist_one(ccr.as_ref(), &content, &record);

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
        };
        store.persist(vec![record.clone()], "/home/dev/alpha");

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
        };
        store.persist(vec![record.clone()], "/home/dev/alpha");

        assert_eq!(store.shed_batches(), 1, "the batch should be shed");
        assert_eq!(store.queued_bytes(), 0, "a shed batch charges nothing");
        assert_eq!(
            store.ccr().get(&record.hash).as_deref(),
            Some(record.original.as_str()),
            "shedding the index write must not cost the original"
        );
    }
}
