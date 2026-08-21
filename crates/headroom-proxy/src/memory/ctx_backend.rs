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

/// How deep to read the shared index when a partition's own matches are
/// buried under another project's. Sized to sweep the whole store rather
/// than to tune a page: the filter, not the ranking, decides what survives.
const WIDE_SEARCH_LIMIT: usize = 2000;

/// Score given to a memory reached by a shared entity rather than by matching
/// the query.
///
/// Adjacency is not a relevance measurement, so there is no honest number to
/// compute here. The only job is to clear `min_similarity` (0.3 by default)
/// so these are not filtered out on arrival. Rank order comes from position
/// instead: expanded hits are appended after the matched ones and the list is
/// never re-sorted, so a related memory cannot displace a real match even
/// when BM25 scored that match below this value.
const RELATED_SCORE: f64 = 0.35;

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
        let backend = Self {
            records: Arc::new(MemoryRecordStore::open(base_dir.join("memories.db"))?),
            index: Arc::new(CtxStore::open(base_dir.join("memories_index.db"))?),
        };
        backend.backfill_entities()?;
        Ok(backend)
    }

    /// Build the entity edges for memories saved before the table existed.
    ///
    /// Without this the join answers nothing on any store already in use —
    /// the entities are in the records, just not anywhere queryable. Runs once
    /// per store, guarded by the schema version, and reads every record to do
    /// it. That is a full scan, which is affordable here only because these
    /// stores hold thousands of rows rather than millions.
    ///
    /// One unreadable or unwritable record is logged and skipped, since a
    /// single bad row must not cost the whole expansion. A failure to read the
    /// table at all propagates: that means the store is broken, and `open`
    /// already reports those rather than returning a backend that cannot work.
    fn backfill_entities(&self) -> Result<(), BackendError> {
        if !self.records.needs_entity_backfill()? {
            return Ok(());
        }
        let mut filled = 0usize;
        for (id, json) in self.records.all_records()? {
            let Ok(memory) = serde_json::from_str::<Memory>(&json) else {
                continue;
            };
            if memory.entity_refs.is_empty() {
                continue;
            }
            if let Err(e) = self.records.set_entities(&id, &memory.entity_refs) {
                tracing::warn!(
                    event = "memory_entity_backfill_failed",
                    memory_id = %id,
                    error = %e,
                    "skipping one record"
                );
                continue;
            }
            filled += 1;
        }
        self.records.mark_entity_backfill_done()?;
        if filled > 0 {
            tracing::info!(
                event = "memory_entity_backfill",
                memories = filled,
                "built entity edges for existing memories"
            );
        }
        Ok(())
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

/// Map a `SearchHit::rank` onto `(0, 1)`, rising with match strength.
///
/// The name says BM25, but `CtxStore::search` fuses two ranked lists and
/// overwrites `rank` with the negated RRF score (`store.rs`, `rrf_search`).
/// That number lives on a completely different scale: the best hit reachable
/// — first place in both lists — is `2/(RRF_K + 1)`, about 0.033, and a
/// single-list leader is half that.
///
/// Feeding those straight into `strength / (1 + strength)` capped the output
/// at 0.032 while `min_similarity` defaults to 0.3, so *every* memory search
/// was discarded in full: 7,265 consecutive `all_below_min_similarity` events
/// in the live log with not one success. Not a threshold that wanted tuning —
/// a threshold above the highest value the function could return.
///
/// So the fused score is scaled by `RRF_K + 1` before the squash, putting a
/// single-list leader at 0.5 and a both-list leader at 0.67, and letting a
/// threshold like 0.3 mean roughly "inside the first hundred or so fused
/// positions". Still a ranking signal rather than a probability, and still
/// not comparable across query shapes — but now on a scale a caller can
/// threshold against.
fn rank_to_score(rank: f64) -> f64 {
    // Strictly monotone in `|rank|` with no clamp, so ordering survives even
    // for a raw BM25 rank — a clamp would flatten every strong hit onto 1.0
    // and lose the ordering the caller depends on.
    let strength = rank.abs() * (headroom_core::ctx::RRF_K + 1.0);
    strength / (1.0 + strength)
}

impl CtxMemoryBackend {
    /// Top `top_k` matches belonging to `user_id`, read from the first
    /// `limit` ranked hits of the shared index.
    fn ranked_for_user(
        &self,
        query: &str,
        user_id: &str,
        top_k: usize,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>, BackendError> {
        let opts = SearchOpts {
            limit,
            ..Default::default()
        };
        let hits = self.index.search(&[query.to_string()], &opts)?;

        let mut out = Vec::new();
        // A memory is indexed as several chunks, so one memory can match more
        // than once — and did, filling two of three answer slots with the same
        // text at ranks 0.031 and 0.032. Keep the best hit only.
        let mut seen = std::collections::HashSet::new();
        for hit in hits {
            if !seen.insert(hit.source.clone()) {
                continue;
            }
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
            // The project's own partition, plus the shared one that holds
            // facts about the user rather than about a codebase.
            let visible = memory.user_id == user_id
                || memory.user_id == super::router::shared_partition(user_id);
            if !visible || !memory.is_current() {
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

    fn load(&self, memory_id: &str) -> Result<Option<Memory>, BackendError> {
        match self.records.get(memory_id)? {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    fn store(&self, memory: &Memory) -> Result<(), BackendError> {
        let json = serde_json::to_string(memory)?;
        self.records.put(&memory.id, &memory.user_id, &json)?;
        // The entity list lives in the JSON too, but only the edge table can be
        // queried by entity. Rewritten on every store so the two never drift.
        self.records.set_entities(&memory.id, &memory.entity_refs)?;
        Ok(())
    }

    /// Memories sharing an entity with `seeds`, to fill slots BM25 left empty.
    ///
    /// The one hop that `include_related` promises: search finds memories by
    /// the words they contain, this finds the ones that talk about the same
    /// subject in different words. Nothing here is ranked — adjacency is a yes
    /// or no — so results keep input order and the caller decides how many to
    /// take.
    fn related_to(
        &self,
        seeds: &[MemorySearchResult],
        user_id: &str,
        want: usize,
    ) -> Result<Vec<MemorySearchResult>, BackendError> {
        if want == 0 {
            return Ok(Vec::new());
        }
        let mut entities: Vec<String> = Vec::new();
        let mut already: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for seed in seeds {
            already.insert(seed.memory.id.as_str());
            entities.extend(seed.memory.entity_refs.iter().cloned());
        }
        if entities.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for id in self.records.memories_for_entities(&entities)? {
            if already.contains(id.as_str()) {
                continue;
            }
            let Some(memory) = self.load(&id)? else {
                // Edge outliving its record. Same call as an orphaned index
                // entry: skip it, never fail the search over it.
                continue;
            };
            let visible = memory.user_id == user_id
                || memory.user_id == super::router::shared_partition(user_id);
            if !visible || !memory.is_current() {
                continue;
            }
            out.push(MemorySearchResult {
                related_entities: memory.entity_refs.clone(),
                score: RELATED_SCORE,
                memory,
            });
            if out.len() >= want {
                break;
            }
        }
        Ok(out)
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
        include_related: bool,
    ) -> Result<Vec<MemorySearchResult>, BackendError> {
        // `memory_list` asks for everything by searching for nothing. BM25 has
        // no answer to that — an empty query matches no documents — so the
        // enumeration comes from the records instead of the index.
        if query.trim().is_empty() {
            let mut out = Vec::new();
            let shared = super::router::shared_partition(user_id);
            let mut ids = self.records.ids_for_user(user_id)?;
            if shared != user_id {
                ids.extend(self.records.ids_for_user(shared)?);
            }
            for id in ids {
                let Some(memory) = self.load(&id)? else {
                    continue;
                };
                if !memory.is_current() {
                    continue;
                }
                out.push(MemorySearchResult {
                    related_entities: memory.entity_refs.clone(),
                    // Nothing was ranked, so score is meaningless here — but it
                    // must clear `min_similarity` (0.3 by default) or callers
                    // that filter would drop every listed memory.
                    score: 1.0,
                    memory,
                });
                if out.len() >= top_k {
                    break;
                }
            }
            return Ok(out);
        }

        // Over-fetch: hits are filtered by user and validity below, and the
        // index cannot express either, so asking for exactly `top_k` would
        // return fewer after filtering.
        let narrow = top_k.saturating_mul(4).max(top_k);
        let out = self.ranked_for_user(query, user_id, top_k, narrow)?;
        if out.len() >= top_k {
            return Ok(out);
        }

        // One index serves every project, but the partition filter runs after
        // ranking — so a project holding 37 memories can be pushed off the
        // first page entirely by one holding 155, and come back empty while a
        // perfectly good match sits at rank 50. Widen when the narrow page did
        // not fill, which costs a second query only on the searches that need
        // one.
        let wide = self.ranked_for_user(query, user_id, top_k, WIDE_SEARCH_LIMIT)?;
        let mut out = if wide.len() > out.len() { wide } else { out };

        // Only once the text index has had both attempts and still left room.
        // Expansion fills empty slots; it never competes for full ones.
        if include_related && out.len() < top_k {
            let want = top_k - out.len();
            out.extend(self.related_to(&out, user_id, want)?);
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

    async fn save_with_entities(
        backend: &CtxMemoryBackend,
        content: &str,
        user: &str,
        entities: &[&str],
    ) -> Memory {
        let entities: Vec<String> = entities.iter().map(|e| (*e).to_string()).collect();
        backend
            .save_memory(content, user, 0.5, None, Some(&entities), None, None, None)
            .await
            .unwrap()
    }

    /// The point of the join: a memory whose text shares no word with the
    /// query still arrives, because it names an entity the match named.
    #[tokio::test]
    async fn related_memories_arrive_through_a_shared_entity() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save_with_entities(
            &backend,
            "TLS records get corrupted under WSL2",
            "alice",
            &["WSL2"],
        )
        .await;
        save_with_entities(
            &backend,
            "the loopback interface drops large frames",
            "alice",
            &["WSL2"],
        )
        .await;

        let found = |results: &[MemorySearchResult], needle: &str| {
            results.iter().any(|r| r.memory.content.contains(needle))
        };

        let without = backend
            .search_memories("TLS corruption", "alice", 5, false)
            .await
            .unwrap();
        assert!(found(&without, "TLS records"), "the direct match is missing");
        assert!(
            !found(&without, "loopback"),
            "expansion must not happen when it was not asked for"
        );

        let with = backend
            .search_memories("TLS corruption", "alice", 5, true)
            .await
            .unwrap();
        assert!(found(&with, "TLS records"), "the direct match is missing");
        assert!(
            found(&with, "loopback"),
            "the memory sharing entity WSL2 should have been pulled in"
        );
    }

    /// Case is an accident of how a save was worded, not a different subject.
    #[tokio::test]
    async fn entity_matching_ignores_case() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save_with_entities(&backend, "bolt runs on port 7687", "alice", &["Neo4j"]).await;
        save_with_entities(&backend, "the graph store needs APOC", "alice", &["neo4j"]).await;

        let results = backend
            .search_memories("bolt port", "alice", 5, true)
            .await
            .unwrap();
        assert!(
            results.iter().any(|r| r.memory.content.contains("APOC")),
            "Neo4j and neo4j are one entity"
        );
    }

    /// Expansion fills leftover slots; it never costs a real match its place.
    #[tokio::test]
    async fn expansion_never_displaces_a_ranked_match() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save_with_entities(&backend, "redis listens on 6379", "alice", &["redis"]).await;
        save_with_entities(&backend, "the cache eviction is LRU", "alice", &["redis"]).await;

        let results = backend
            .search_memories("redis 6379", "alice", 1, true)
            .await
            .unwrap();
        assert_eq!(results.len(), 1, "top_k must still be honoured");
        assert!(
            results[0].memory.content.contains("6379"),
            "the ranked match must keep the only slot"
        );
    }

    /// Another project's memories stay out, expansion or not.
    #[tokio::test]
    async fn expansion_respects_the_user_partition() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save_with_entities(&backend, "postgres runs on 5432", "alice", &["postgres"]).await;
        save_with_entities(&backend, "bob's secret postgres note", "bob", &["postgres"]).await;

        let results = backend
            .search_memories("postgres 5432", "alice", 5, true)
            .await
            .unwrap();
        assert!(
            !results.iter().any(|r| r.memory.content.contains("bob's")),
            "expansion must not cross the partition boundary"
        );
    }

    /// Deleting a memory must not leave an edge that resurrects it.
    #[tokio::test]
    async fn deleting_a_memory_removes_its_entity_edges() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save_with_entities(&backend, "qdrant holds the vectors", "alice", &["qdrant"]).await;
        let doomed =
            save_with_entities(&backend, "qdrant port is 6333", "alice", &["qdrant"]).await;

        backend.delete_memory(&doomed.id).await.unwrap();

        let ids = backend
            .records
            .memories_for_entities(&["qdrant".to_string()])
            .unwrap();
        assert!(
            !ids.contains(&doomed.id),
            "the deleted memory still has an edge"
        );

        let results = backend
            .search_memories("vectors", "alice", 5, true)
            .await
            .unwrap();
        assert!(
            !results.iter().any(|r| r.memory.id == doomed.id),
            "a deleted memory came back through expansion"
        );
    }

    /// Memories written before the edge table existed still get expanded.
    #[tokio::test]
    async fn existing_memories_are_backfilled() {
        let backend = CtxMemoryBackend::in_memory().unwrap();
        save_with_entities(&backend, "the mirror is an env var", "alice", &["headroom"]).await;
        save_with_entities(&backend, "downloads must use https", "alice", &["headroom"]).await;

        // Wipe the edges and rearm the marker, leaving the records intact —
        // exactly the shape of a store that predates this feature.
        for (id, _) in backend.records.all_records().unwrap() {
            backend.records.set_entities(&id, &[]).unwrap();
        }
        backend.records.reset_entity_backfill().unwrap();
        assert!(
            backend
                .records
                .memories_for_entities(&["headroom".to_string()])
                .unwrap()
                .is_empty(),
            "the edges should be gone before the backfill runs"
        );

        backend.backfill_entities().unwrap();

        assert_eq!(
            backend
                .records
                .memories_for_entities(&["headroom".to_string()])
                .unwrap()
                .len(),
            2,
            "the backfill should have rebuilt both edges from the records"
        );
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
        // The assertion used to read `<`, contradicting the line above it and
        // certifying the inversion that made every threshold in the caller
        // read backwards.
        assert!(rank_to_score(-10.0) > rank_to_score(-1.0));
        // The bug that made every memory search return nothing: the best hit
        // RRF can produce must clear the default `min_similarity` of 0.3.
        let best_both_lists = 2.0 / (headroom_core::ctx::RRF_K + 1.0);
        let best_single_list = 1.0 / (headroom_core::ctx::RRF_K + 1.0);
        assert!(
            rank_to_score(-best_both_lists) > 0.3,
            "the best possible hit must be retrievable, got {}",
            rank_to_score(-best_both_lists)
        );
        assert!(
            rank_to_score(-best_single_list) > 0.3,
            "a single-list leader must be retrievable too, got {}",
            rank_to_score(-best_single_list)
        );
        assert!(rank_to_score(-1.0) <= 1.0);
        assert!(rank_to_score(-1000.0) > 0.0);
        assert!(
            rank_to_score(0.0) < rank_to_score(-0.5),
            "no match scores lowest"
        );
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
