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
    /// Shared reference to the CCR store — used by CTX-5 `/ctx/get`.
    ccr: Arc<dyn CcrStore>,
    /// Per-project FTS content stores — used by CTX-5 `/ctx/search` and
    /// `/ctx/index`, and by CTX-4 recall.
    stores: Arc<ProjectStores>,
}

/// A batch of originals plus the project whose index they belong in.
struct Batch {
    records: Vec<OffloadRecord>,
    project_dir: String,
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
        // Clone Arcs for the background thread — cheap (atomic refcount).
        let ccr_bg = Arc::clone(&ccr);
        let stores_bg = Arc::clone(&stores);
        thread::Builder::new()
            .name("ctx-offload-store".to_string())
            .spawn(move || {
                for batch in rx {
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

        Ok(Self { tx, ccr, stores })
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

    /// Enqueue offloaded originals for persistence into `project_dir`'s index.
    /// Non-blocking: hands the batch to the worker and returns. A send failure
    /// means the worker is gone — logged, never fatal (the wire bytes are
    /// already correct).
    pub fn persist(&self, records: Vec<OffloadRecord>, project_dir: &str) {
        if records.is_empty() {
            return;
        }
        let batch = Batch {
            records,
            project_dir: project_dir.to_string(),
        };
        if self.tx.send(batch).is_err() {
            tracing::warn!(
                event = "ctx_offload_store_worker_gone",
                "CTX-3 offload-store worker unavailable; dropping persistence"
            );
        }
    }
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
}
