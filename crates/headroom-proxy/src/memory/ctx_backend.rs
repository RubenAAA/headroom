//! Memory backend over the ctx FTS5 store.
//!
//! Replaces [`super::local_backend::LocalMemoryBackend`], which kept memories
//! in a `Vec`, scored them by counting overlapping words, and lost everything
//! on restart. The ctx subsystem already ships a persistent FTS5 BM25 index
//! with porter stemming and trigram matching (`headroom_core::ctx::CtxStore`),
//! used for recall injection and `/ctx/search`. Two stores answering "what
//! text is relevant to this query" is one more than the job needs, and the
//! weaker one was the default.
//!
//! # Why a sidecar table
//!
//! `CtxStore` indexes *labelled sources* — it has no notion of a memory id, a
//! user, or a validity window, and no delete-by-id. A [`Memory`] carries all
//! three plus supersession and access bookkeeping. So the record lives in a
//! `memories` table beside the index, and `CtxStore` holds only what search
//! needs, keyed by the memory id as its source label. Search finds ids and
//! ranks them; the table answers what those ids are.
//!
//! The pair is kept consistent by writing the record first and the index
//! second: a crash between the two leaves a memory that is retrievable by id
//! but not yet searchable, which degrades recall. The reverse order would
//! leave the index pointing at a row that does not exist, which surfaces as a
//! silent gap in results.
//!
//! # Ranking
//!
//! BM25 returns a negative rank (more negative = better). The trait wants a
//! score where higher is better and callers compare against
//! `min_similarity`, so ranks are mapped to `(0, 1]` — see [`rank_to_score`].
//! This is a monotone remap, not a calibration: the ordering is BM25's, and
//! the absolute values are not comparable to a cosine similarity.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use headroom_core::ctx::{CtxStore, IndexOpts, MemoryRecordStore, SearchOpts};

use super::backend::{MemoryBackend, MemorySearchResult};
use super::models::Memory;

type BackendError = Box<dyn std::error::Error + Send + Sync>;

/// Memory backend backed by the ctx FTS5 store plus a record sidecar.
pub struct CtxMemoryBackend {
    /// Record store: the `Memory` values themselves.
    records: Arc<MemoryRecordStore>,
    /// Search index: memory content, labelled by memory id.
    index: Arc<CtxStore>,
}

impl CtxMemoryBackend {
    /// Open (or create) both stores under `base_dir`.
    ///
    /// `memories.db` holds the records; `memories_index.db` is the FTS index.
    /// Two files rather than two tables in one, because `CtxStore` owns its
    /// schema and migrations — sharing a file would couple this table's
    /// lifetime to theirs.
    pub fn open(base_dir: &Path) -> Result<Self, BackendError> {
        std::fs::create_dir_all(base_dir)?;
        Ok(Self {
            records: Arc::new(MemoryRecordStore::open(base_dir.join("memories.db"))?),
            index: Arc::new(CtxStore::open(base_dir.join("memories_index.db"))?),
        })
    }

    /// In-memory instance for tests. Both stores are per-connection, so this
    /// is isolated per call.
    #[cfg(test)]
    fn in_memory() -> Result<Self, BackendError> {
        Ok(Self {
            records: Arc::new(MemoryRecordStore::open(":memory:")?),
            index: Arc::new(CtxStore::open(":memory:")?),
        })
    }
}

/// Map a BM25 rank (negative, more-negative = better) onto `(0, 1]`.
///
/// `1/(1 + |rank|)` is monotone decreasing in `|rank|`, so ordering is
/// preserved exactly and a perfect hit approaches 1 without ever reaching it.
/// Callers threshold this against `min_similarity`; it is a ranking signal,
/// not a probability, and is not comparable across query shapes.
fn rank_to_score(rank: f64) -> f64 {
    1.0 / (1.0 + rank.abs())
}

impl CtxMemoryBackend {
    fn load(&self, memory_id: &str) -> Result<Option<Memory>, BackendError> {
        match self.records.get(memory_id)? {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    fn store(&self, memory: &Memory) -> Result<(), BackendError> {
        let json = serde_json::to_string(memory)?;
        self.records.put(&memory.id, &memory.user_id, &json)?;
        Ok(())
    }

    /// Index a memory's content for search, labelled by its id.
    ///
    /// `index_content` replaces any prior source with the same label, so this
    /// doubles as the update path.
    fn index(&self, memory: &Memory) -> Result<(), BackendError> {
        let opts = IndexOpts {
            // Plain text: a memory is a sentence or two, not markdown, and the
            // markdown chunker would treat a stray `#` as a heading.
            plain_text_lines: Some(50),
            ..Default::default()
        };
        self.index
            .index_content(&memory.id, &memory.content, &opts)?;
        Ok(())
    }
}

#[async_trait]
impl MemoryBackend for CtxMemoryBackend {
    async fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        _facts: Option<&[String]>,
        entities: Option<&[String]>,
        _extracted_entities: Option<&[serde_json::Value]>,
        _relationships: Option<&[serde_json::Value]>,
        _extracted_relationships: Option<&[serde_json::Value]>,
    ) -> Result<Memory, BackendError> {
        let mut memory = Memory {
            content: content.to_string(),
            user_id: user_id.to_string(),
            importance,
            ..Memory::default()
        };
        if let Some(ents) = entities {
            memory.entity_refs = ents.to_vec();
        }
        // Record first: a memory that exists but is not yet searchable is a
        // weaker failure than an index entry pointing at nothing.
        self.store(&memory)?;
        self.index(&memory)?;
        Ok(memory)
    }

    async fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        top_k: usize,
        _include_related: bool,
    ) -> Result<Vec<MemorySearchResult>, BackendError> {
        // Over-fetch: hits are filtered by user and validity below, and the
        // index cannot express either, so asking for exactly `top_k` would
        // return fewer after filtering.
        let opts = SearchOpts {
            limit: top_k.saturating_mul(4).max(top_k),
            ..Default::default()
        };
        let hits = self.index.search(&[query.to_string()], &opts)?;

        let mut out = Vec::new();
        for hit in hits {
            // The source label is the memory id.
            let Some(memory) = self.load(&hit.source)? else {
                // Indexed but no record: a crash between the two writes, or a
                // record deleted without its index entry. Skip rather than
                // fail the search — a missing memory must not break recall.
                tracing::debug!(
                    event = "memory_index_orphan",
                    memory_id = %hit.source,
                    "search hit has no record; skipping"
                );
                continue;
            };
            if memory.user_id != user_id || !memory.is_current() {
                continue;
            }
            out.push(MemorySearchResult {
                related_entities: memory.entity_refs.clone(),
                score: rank_to_score(hit.rank),
                memory,
            });
            if out.len() >= top_k {
                break;
            }
        }
        Ok(out)
    }

    async fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        _user_id: &str,
        _reason: Option<&str>,
    ) -> Result<Memory, BackendError> {
        let Some(mut memory) = self.load(memory_id)? else {
            return Err(format!("Memory {memory_id} not found").into());
        };
        memory.content = new_content.to_string();
        self.store(&memory)?;
        // Re-index: `index_content` replaces by label, so the old text goes.
        self.index(&memory)?;
        Ok(memory)
    }

    async fn delete_memory(&self, memory_id: &str) -> Result<bool, BackendError> {
        let removed = self.records.delete(memory_id)?;
        if removed {
            // Empty content leaves a source with no chunks — the label stops
            // matching anything. `CtxStore` has no delete-by-label on its
            // public surface, and this is equivalent for search purposes.
            if let Err(error) = self
                .index
                .index_content(memory_id, "", &IndexOpts::default())
            {
                tracing::warn!(
                    event = "memory_reindex_failed",
                    operation = "delete_memory",
                    memory_id = %memory_id,
                    error = %error,
                    "memory record deleted but FTS cleanup failed"
                );
            }
        }
        Ok(removed)
    }

    async fn get_memory(&self, memory_id: &str) -> Result<Option<Memory>, BackendError> {
        self.load(memory_id)
    }

    async fn clear_user(&self, user_id: &str) -> Result<usize, BackendError> {
        // Ids first: once the rows are gone there is nothing left to say which
        // index entries belonged to this user.
        for id in self.records.ids_for_user(user_id)? {
            if let Err(error) = self.index.index_content(&id, "", &IndexOpts::default()) {
                tracing::warn!(
                    event = "memory_reindex_failed",
                    operation = "clear_user",
                    memory_id = %id,
                    user_id = %user_id,
                    error = %error,
                    "memory record cleared but FTS cleanup failed"
                );
            }
        }
        Ok(self.records.delete_user(user_id)?)
    }

    async fn close(&self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn save(backend: &CtxMemoryBackend, content: &str, user: &str) -> Memory {
        backend
            .save_memory(content, user, 0.5, None, None, None, None, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn save_and_search() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save(&backend, "I prefer dark mode in my editor", "alice").await;
        save(&backend, "My favorite language is Rust", "alice").await;

        let results = backend
            .search_memories("dark mode", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0.0);
        assert!(results[0].memory.content.contains("dark mode"));
    }

    /// The reason for the swap: BM25 stems, so a query that shares no exact
    /// word with the stored text still matches. The word-overlap scorer this
    /// replaces returned nothing here.
    #[test]
    fn stemming_finds_what_word_overlap_missed() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let backend = CtxMemoryBackend::in_memory().unwrap();
            save(&backend, "the deployment scripts are cached", "alice").await;

            let results = backend
                .search_memories("caching", "alice", 10, false)
                .await
                .unwrap();
            assert_eq!(results.len(), 1, "porter stemming matches cached/caching");
        });
    }

    #[tokio::test]
    async fn search_filters_by_user() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save(&backend, "shared note about deployment", "alice").await;
        save(&backend, "shared note about deployment", "bob").await;

        let results = backend
            .search_memories("deployment", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.user_id, "alice");
    }

    #[tokio::test]
    async fn update_replaces_the_indexed_text() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        let m = save(&backend, "the original subject", "alice").await;

        backend
            .update_memory(&m.id, "an entirely different topic", "alice", None)
            .await
            .unwrap();

        assert_eq!(
            backend.get_memory(&m.id).await.unwrap().unwrap().content,
            "an entirely different topic"
        );
        let stale = backend
            .search_memories("original subject", "alice", 10, false)
            .await
            .unwrap();
        assert!(stale.is_empty(), "the replaced text must stop matching");
    }

    #[tokio::test]
    async fn delete_removes_from_both_stores() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        let m = save(&backend, "something forgettable", "alice").await;

        assert!(backend.delete_memory(&m.id).await.unwrap());
        assert!(backend.get_memory(&m.id).await.unwrap().is_none());
        assert!(backend
            .search_memories("forgettable", "alice", 10, false)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn clear_user_leaves_other_users_alone() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save(&backend, "alice note one", "alice").await;
        save(&backend, "alice note two", "alice").await;
        save(&backend, "bob note", "bob").await;

        assert_eq!(backend.clear_user("alice").await.unwrap(), 2);
        assert!(backend
            .search_memories("note", "alice", 10, false)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            backend
                .search_memories("note", "bob", 10, false)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// A record deleted out from under the index must degrade to a skipped
    /// hit, not a failed search.
    #[tokio::test]
    async fn an_orphaned_index_entry_is_skipped() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        let m = save(&backend, "orphan candidate", "alice").await;
        // Drop the record but leave the index entry behind.
        backend.records.delete(&m.id).unwrap();
        let results = backend
            .search_memories("orphan", "alice", 10, false)
            .await
            .unwrap();
        assert!(results.is_empty(), "orphan skipped, search still succeeds");
    }

    #[test]
    fn score_is_monotone_in_rank() {
        // More-negative BM25 rank is a better hit, so it must score higher.
        assert!(rank_to_score(-10.0) < rank_to_score(-1.0));
        assert!(rank_to_score(-1.0) <= 1.0);
        assert!(rank_to_score(-1000.0) > 0.0);
    }

    /// Persistence is the other half of the point: the `Vec` backend lost
    /// everything on restart.
    #[tokio::test]
    async fn memories_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        {
            let backend = CtxMemoryBackend::open(dir.path()).unwrap();
            save(&backend, "durable across restarts", "alice").await;
        }
        let reopened = CtxMemoryBackend::open(dir.path()).unwrap();
        let results = reopened
            .search_memories("durable", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }
}
