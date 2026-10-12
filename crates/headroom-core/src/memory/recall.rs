//! Queryable recall store: FTS5 lossless storage behind the memory interface.
//!
//! P1 `headroom-recall` (`docs/notes/ideas/ctx-p1-recall-store.md`): the
//! context-mode `store.ts` port already lives here as
//! [`crate::ctx::CtxStore`] (dual porter/trigram FTS5, RRF merge), and the
//! proxy's `memory::ctx_backend` module already pairs it with
//! [`crate::ctx::MemoryRecordStore`] behind the proxy's async memory
//! trait. What was missing was the same pair behind the core interface —
//! this struct implements the sync [`MemoryBackend`] in this crate, and
//! doubles as the persistent CCR backend Phase B wants by also
//! implementing [`crate::ccr::CcrStore`].
//!
//! Storage layout mirrors the proxy backend exactly: the record (opaque
//! JSON) goes in the record table first and the searchable text in the FTS
//! index second, keyed by memory id. A crash between the two leaves a
//! memory retrievable by id but not yet searchable; the reverse order
//! would leave the index pointing at a row that does not exist.
//!
//! # What this slice does not do
//!
//! - No expiry. CCR entries stashed here never expire, unlike the idle-TTL
//!   memory/sqlite CCR backends. Retention policy is the enterprise
//!   variant in the idea file, not this slice.
//! - No save dedup. Re-saving identical text mints a fresh id; the proxy
//!   backend dedups at its layer.
//! - `relationships` on save are dropped: [`Memory`] has no relationship
//!   field, and entity edges carry the graph-shaped half already.

use std::collections::HashSet;
use std::path::Path;

use crate::ccr::CcrStore;
use crate::ctx::{CtxStore, IndexOpts, MemoryRecordStore, RRF_K, SearchOpts};

use super::backend::{MemoryBackend, MemorySearchResult, Relationship, UpdateError};
use super::models::Memory;

/// Owning user for CCR payloads stashed through the [`CcrStore`] impl.
///
/// Payloads live in the same tables as memories but under their own
/// partition, so a memory search for any real user never surfaces them —
/// and retrieve-by-query ([`RecallStore::search_payloads`]) searches this
/// partition only.
const CCR_USER: &str = "ccr";

/// Score for a memory reached by entity expansion rather than by matching
/// the query. Same value and rationale as the proxy backend: adjacency is
/// not a relevance measurement, so the number only has to clear the
/// default `min_similarity` (0.3); ordering comes from position instead,
/// since expanded hits are appended after the ranked ones.
const RELATED_SCORE: f64 = 0.35;

/// FTS5-backed [`MemoryBackend`] + [`CcrStore`] over one record table and
/// one search index. Both connections are mutex-guarded inside the two
/// stores, so this is cheap to share across threads.
pub struct RecallStore {
    records: MemoryRecordStore,
    index: CtxStore,
}

/// Map a fused RRF rank onto `(0, 1)`, rising with match strength.
///
/// [`SearchHit::rank`](crate::ctx::SearchHit) is a negated fused score on
/// a tiny scale — the best hit reachable is `2/(RRF_K + 1)` — so it is
/// scaled up before the squash. Same mapping as the proxy backend;
/// monotone, never clamped, so ordering survives.
fn rank_to_score(rank: f64) -> f64 {
    let strength = rank.abs() * (RRF_K + 1.0);
    strength / (1.0 + strength)
}

/// Whether `memory` passes the search-time entity/session filters. `None`
/// on either side means unfiltered.
fn visible(memory: &Memory, entities: Option<&[String]>, session_id: Option<&str>) -> bool {
    if let Some(sid) = session_id
        && memory.session_id.as_deref() != Some(sid)
    {
        return false;
    }
    if let Some(want) = entities {
        let hit = memory
            .entity_refs
            .iter()
            .any(|e| want.iter().any(|w| w.trim().eq_ignore_ascii_case(e.trim())));
        if !hit {
            return false;
        }
    }
    true
}

impl RecallStore {
    /// Open (or create) the store under `base_dir`: `recall.db` holds the
    /// records, `recall_index.db` the FTS index. Two files rather than two
    /// tables in one because `CtxStore` owns its schema and migrations.
    /// Pass `":memory:"` for an ephemeral per-connection pair (tests).
    pub fn open(base_dir: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let dir = base_dir.as_ref();
        if dir == Path::new(":memory:") {
            return Ok(Self {
                records: MemoryRecordStore::open(":memory:")?,
                index: CtxStore::open(":memory:")?,
            });
        }
        // `Connection::open` does not create parents; map the io error the
        // way `private_db::open` does so this stays `rusqlite::Result`.
        std::fs::create_dir_all(dir).map_err(|e| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!("creating {}: {e}", dir.display())),
            )
        })?;
        Ok(Self {
            records: MemoryRecordStore::open(dir.join("recall.db"))?,
            index: CtxStore::open(dir.join("recall_index.db"))?,
        })
    }

    /// Retrieve CCR payloads by query instead of by hash: the top-`top_k`
    /// payloads in the CCR partition, as `(hash, content)` pairs. `hash`
    /// is the same id [`CcrStore::put`] stored under, so a query hit feeds
    /// straight back into [`CcrStore::get`].
    pub fn search_payloads(&self, query: &str, top_k: usize) -> Vec<(String, String)> {
        let limit = top_k.saturating_mul(4).max(top_k);
        self.ranked(query, CCR_USER, top_k, limit, None, None)
            .into_iter()
            .map(|r| (r.memory.id, r.memory.content))
            .collect()
    }

    /// Top-`top_k` current memories of `user_id` ranked against `query`,
    /// read from the first `limit` index hits. One memory indexes as
    /// several chunks and can match more than once, so only its best hit
    /// is kept. Index failures log and degrade to no results — a broken
    /// index must not fail recall outright.
    fn ranked(
        &self,
        query: &str,
        user_id: &str,
        top_k: usize,
        limit: usize,
        entities: Option<&[String]>,
        session_id: Option<&str>,
    ) -> Vec<MemorySearchResult> {
        let opts = SearchOpts {
            limit,
            ..Default::default()
        };
        let hits = match self.index.search(&[query.to_string()], &opts) {
            Ok(hits) => hits,
            Err(error) => {
                tracing::warn!(
                    event = "recall_search_failed",
                    error = %error,
                    "FTS search failed; returning no results"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for hit in hits {
            if !seen.insert(hit.source.clone()) {
                continue;
            }
            // The source label is the memory id.
            let Some(memory) = self.load(&hit.source) else {
                tracing::debug!(
                    event = "recall_index_orphan",
                    memory_id = %hit.source,
                    "search hit has no record; skipping"
                );
                continue;
            };
            if memory.user_id != user_id || !memory.is_current() {
                continue;
            }
            if !visible(&memory, entities, session_id) {
                continue;
            }
            out.push(MemorySearchResult {
                similarity: rank_to_score(hit.rank),
                memory,
                // Final positions are assigned by the caller after
                // filtering and expansion settle the list.
                rank: 0,
            });
            if out.len() >= top_k {
                break;
            }
        }
        out
    }

    /// All current memories of `user_id`, newest storage order, for the
    /// empty-query (list) path. BM25 has no answer to an empty query, so
    /// the enumeration comes from the records instead of the index.
    fn list_for_user(
        &self,
        user_id: &str,
        top_k: usize,
        entities: Option<&[String]>,
        session_id: Option<&str>,
    ) -> Vec<MemorySearchResult> {
        let ids = match self.records.ids_for_user(user_id) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    event = "recall_list_failed",
                    user_id = %user_id,
                    error = %error,
                    "record enumeration failed; returning no results"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for id in ids {
            let Some(memory) = self.load(&id) else {
                continue;
            };
            if !memory.is_current() || !visible(&memory, entities, session_id) {
                continue;
            }
            out.push(MemorySearchResult {
                // Nothing was ranked; the score only has to clear the
                // caller's `min_similarity` filter.
                similarity: 1.0,
                memory,
                rank: 0,
            });
            if out.len() >= top_k {
                break;
            }
        }
        out
    }

    /// Memories sharing an entity with `seeds`, to fill slots the ranked
    /// pass left empty. Nothing here is ranked — adjacency is a yes or
    /// no — so results keep store order and the caller decides how many
    /// to take.
    fn related_to(
        &self,
        seeds: &[MemorySearchResult],
        user_id: &str,
        want: usize,
        entities: Option<&[String]>,
        session_id: Option<&str>,
    ) -> Vec<MemorySearchResult> {
        if want == 0 {
            return Vec::new();
        }
        let mut seed_entities = Vec::new();
        let mut already = HashSet::new();
        for seed in seeds {
            already.insert(seed.memory.id.clone());
            seed_entities.extend(seed.memory.entity_refs.iter().cloned());
        }
        if seed_entities.is_empty() {
            return Vec::new();
        }
        let ids = match self.records.memories_for_entities(&seed_entities) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    event = "recall_expansion_failed",
                    error = %error,
                    "entity expansion failed; returning ranked hits only"
                );
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for id in ids {
            if already.contains(&id) {
                continue;
            }
            let Some(memory) = self.load(&id) else {
                continue;
            };
            if memory.user_id != user_id || !memory.is_current() {
                continue;
            }
            if !visible(&memory, entities, session_id) {
                continue;
            }
            out.push(MemorySearchResult {
                similarity: RELATED_SCORE,
                memory,
                rank: 0,
            });
            if out.len() >= want {
                break;
            }
        }
        out
    }

    /// Load one record by id. `None` covers missing and unreadable alike;
    /// the unreadable case logs, since a row that exists but cannot be
    /// read is data loss, not absence.
    fn load(&self, memory_id: &str) -> Option<Memory> {
        let json = match self.records.get(memory_id) {
            Ok(json) => json?,
            Err(error) => {
                tracing::warn!(
                    event = "recall_load_failed",
                    memory_id = %memory_id,
                    error = %error,
                    "record read failed"
                );
                return None;
            }
        };
        match serde_json::from_str(&json) {
            Ok(memory) => Some(memory),
            Err(error) => {
                tracing::warn!(
                    event = "recall_decode_failed",
                    memory_id = %memory_id,
                    error = %error,
                    "stored record is not a Memory"
                );
                None
            }
        }
    }

    /// Persist the record and its entity edges. `false` means the record
    /// itself did not land; an edge-only failure logs but still returns
    /// `true`, since edges feed expansion alone.
    fn store(&self, memory: &Memory) -> bool {
        let json = match serde_json::to_string(memory) {
            Ok(json) => json,
            Err(error) => {
                tracing::error!(
                    event = "recall_encode_failed",
                    memory_id = %memory.id,
                    error = %error,
                    "memory does not serialize; not stored"
                );
                return false;
            }
        };
        if let Err(error) = self.records.put(&memory.id, &memory.user_id, &json) {
            tracing::error!(
                event = "recall_store_failed",
                memory_id = %memory.id,
                error = %error,
                "record write failed"
            );
            return false;
        }
        if let Err(error) = self.records.set_entities(&memory.id, &memory.entity_refs) {
            tracing::warn!(
                event = "recall_edges_failed",
                memory_id = %memory.id,
                error = %error,
                "record stored but entity edges lost"
            );
        }
        true
    }

    /// Index a memory's content for search, labelled by its id.
    /// `index_content` replaces any prior source with the same label, so
    /// this doubles as the update path — and indexing empty content is
    /// the delete path, leaving a label that matches nothing.
    fn index(&self, memory: &Memory) -> bool {
        let opts = IndexOpts {
            // Plain text: a memory is a sentence or two, not markdown,
            // and the markdown chunker would treat a stray `#` as a
            // heading. Same choice as the proxy backend.
            plain_text_lines: Some(50),
            ..Default::default()
        };
        if let Err(error) = self.index.index_content(&memory.id, &memory.content, &opts) {
            tracing::warn!(
                event = "recall_index_failed",
                memory_id = %memory.id,
                error = %error,
                "record stored but not searchable"
            );
            return false;
        }
        true
    }
}

impl MemoryBackend for RecallStore {
    fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        entities: Option<&[String]>,
        _relationships: Option<&[Relationship]>,
        session_id: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Memory {
        let mut memory = Memory::new(content, user_id);
        memory.importance = importance;
        memory.session_id = session_id.map(str::to_string);
        if let Some(ents) = entities {
            memory.entity_refs = ents.to_vec();
        }
        // `Memory.metadata` is a string map; only an object merges into
        // it. Anything else has nowhere to go and is dropped.
        if let Some(serde_json::Value::Object(map)) = metadata {
            memory.metadata.extend(map.clone());
        }
        // Record first: a memory that exists but is not yet searchable is
        // a weaker failure than an index entry pointing at nothing.
        // (`save_memory` has no error channel, so a failed store logs at
        // error level inside `store` and the memory is returned anyway.)
        self.store(&memory);
        self.index(&memory);
        memory
    }

    fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        entities: Option<&[String]>,
        include_related: bool,
        top_k: usize,
        session_id: Option<&str>,
    ) -> Vec<MemorySearchResult> {
        let mut out = if query.trim().is_empty() {
            self.list_for_user(user_id, top_k, entities, session_id)
        } else {
            // Over-fetch: hits are filtered by user and validity below,
            // and the index cannot express either, so asking for exactly
            // `top_k` would return fewer after filtering.
            let limit = top_k.saturating_mul(4).max(top_k);
            let mut out = self.ranked(query, user_id, top_k, limit, entities, session_id);
            // Only once the ranked pass has had its attempt and still
            // left room. Expansion fills empty slots; it never competes
            // for full ones.
            if include_related && out.len() < top_k {
                let seeds = out.clone();
                out.extend(self.related_to(
                    &seeds,
                    user_id,
                    top_k - out.len(),
                    entities,
                    session_id,
                ));
            }
            out.truncate(top_k);
            out
        };
        for (i, r) in out.iter_mut().enumerate() {
            r.rank = i + 1;
        }
        out
    }

    fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        _reason: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<Memory, UpdateError> {
        let Some(mut existing) = self.load(memory_id) else {
            return Err(UpdateError::NotFound(memory_id.to_string()));
        };
        if let Some(expected) = user_id
            && existing.user_id != expected
        {
            return Err(UpdateError::Unauthorized {
                expected: expected.to_string(),
                got: existing.user_id.clone(),
            });
        }
        // Branching off a retired row would leave two current versions.
        if !existing.is_current() {
            return Err(UpdateError::Superseded {
                id: memory_id.to_string(),
                by: existing.superseded_by.clone(),
            });
        }
        let mut memory = Memory::new(new_content, existing.user_id.clone());
        memory.supersedes = Some(existing.id.clone());
        memory.session_id = existing.session_id.clone();
        memory.importance = existing.importance;
        memory.entity_refs.clone_from(&existing.entity_refs);
        memory.metadata.clone_from(&existing.metadata);
        // New record first, old one marked last: a failure part-way leaves
        // the old row current, where the reverse order left it pointing at
        // a successor that never landed and the content out of every read.
        if !self.store(&memory) {
            return Err(UpdateError::Backend(format!(
                "updated copy of {memory_id} failed to persist"
            )));
        }
        if !self.index(&memory) {
            return Err(UpdateError::Backend(format!(
                "updated copy of {memory_id} failed to index"
            )));
        }
        existing.valid_until = Some(chrono::Utc::now());
        existing.superseded_by = Some(memory.id.clone());
        // The old row keeps its index entry: validity is a read-time
        // filter, and re-indexing unchanged text would churn the FTS
        // tables for no new matchable content.
        if !self.store(&existing) {
            return Err(UpdateError::Backend(format!(
                "superseding {memory_id} failed to persist"
            )));
        }
        Ok(memory)
    }

    fn delete_memory(
        &self,
        memory_id: &str,
        _reason: Option<&str>,
        _user_id: Option<&str>,
    ) -> bool {
        let removed = match self.records.delete(memory_id) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(
                    event = "recall_delete_failed",
                    memory_id = %memory_id,
                    error = %error,
                    "record delete failed"
                );
                return false;
            }
        };
        if removed {
            let tombstone = Memory {
                id: memory_id.to_string(),
                ..Memory::default()
            };
            self.index(&tombstone);
        }
        removed
    }

    fn get_memory(&self, memory_id: &str) -> Option<Memory> {
        self.load(memory_id)
    }

    fn supports_graph(&self) -> bool {
        false
    }

    fn supports_vector_search(&self) -> bool {
        false
    }

    fn close(&self) {
        // Nothing to flush: every call commits inline, and the rusqlite
        // connections close on drop.
    }
}

impl CcrStore for RecallStore {
    /// Stash `payload` under `hash` in the CCR partition. Entries never
    /// expire — this is the lossless half of the idea, not an idle-TTL
    /// cache. `true` means the payload is retrievable via `get`; a failed
    /// index write still returns `true` (and logs), since only
    /// by-query recall degrades, not by-hash retrieval.
    fn put(&self, hash: &str, payload: &str) -> bool {
        let mut memory = Memory::new(payload, CCR_USER);
        memory.id = hash.to_string();
        if !self.store(&memory) {
            return false;
        }
        self.index(&memory);
        true
    }

    fn get(&self, hash: &str) -> Option<String> {
        self.load(hash).map(|m| m.content)
    }

    /// Entry count via a full id scan — the record store exposes no
    /// `COUNT(*)`, and this is informational (tests + telemetry) only.
    fn len(&self) -> usize {
        match self.records.all_records() {
            Ok(rows) => rows.len(),
            Err(error) => {
                tracing::warn!(
                    event = "recall_count_failed",
                    error = %error,
                    "record count failed; reporting 0"
                );
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> RecallStore {
        RecallStore::open(":memory:").expect("in-memory recall store")
    }

    #[test]
    fn save_maps_fields_onto_the_record() {
        let store = store();
        let entities = vec!["tokio".to_string()];
        let metadata = serde_json::json!({"source": "test"});
        let saved = store.save_memory(
            "the runtime is tokio",
            "alice",
            0.9,
            Some(&entities),
            None,
            Some("s1"),
            Some(&metadata),
        );
        assert_eq!(saved.content, "the runtime is tokio");
        assert_eq!(saved.user_id, "alice");
        assert_eq!(saved.importance, 0.9);
        assert_eq!(saved.entity_refs, vec!["tokio"]);
        assert_eq!(saved.session_id.as_deref(), Some("s1"));
        assert_eq!(
            saved.metadata.get("source").and_then(|v| v.as_str()),
            Some("test")
        );

        let loaded = store.get_memory(&saved.id).expect("stored memory");
        assert_eq!(loaded.content, "the runtime is tokio");
        assert_eq!(loaded.entity_refs, vec!["tokio"]);
    }

    #[test]
    fn search_finds_text_and_filters_by_user() {
        let store = store();
        store.save_memory(
            "shared note about deployment",
            "alice",
            0.5,
            None,
            None,
            None,
            None,
        );
        store.save_memory(
            "shared note about deployment",
            "bob",
            0.5,
            None,
            None,
            None,
            None,
        );

        let results = store.search_memories("deployment", "alice", None, false, 10, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.user_id, "alice");
        assert_eq!(results[0].rank, 1);
        assert!(
            results[0].similarity > 0.0,
            "a real match must score above zero"
        );
    }

    #[test]
    fn search_respects_top_k_and_session() {
        let store = store();
        for i in 0..5 {
            let session = if i < 3 { Some("s1") } else { Some("s2") };
            store.save_memory(
                &format!("deploy checklist item {i}"),
                "alice",
                0.5,
                None,
                None,
                session,
                None,
            );
        }
        let results = store.search_memories("deploy checklist", "alice", None, false, 2, None);
        assert_eq!(results.len(), 2);

        let scoped =
            store.search_memories("deploy checklist", "alice", None, false, 10, Some("s2"));
        assert_eq!(scoped.len(), 2);
        assert!(
            scoped
                .iter()
                .all(|r| r.memory.session_id.as_deref() == Some("s2")),
            "session filter must hold on every hit"
        );
    }

    /// The trigram half of the idea: identifiers and codes survive
    /// tokenization that would split them. `E0432` inside `error[E0432]`
    /// is one porter token; only the trigram index matches the bare code.
    #[test]
    fn trigram_finds_an_error_code_inside_punctuation() {
        let store = store();
        store.save_memory(
            "build fails with error[E0432]: unresolved import tokio::fs",
            "alice",
            0.5,
            None,
            None,
            None,
            None,
        );
        let results = store.search_memories("E0432", "alice", None, false, 10, None);
        assert_eq!(results.len(), 1);
        assert!(results[0].memory.content.contains("E0432"));
    }

    #[test]
    fn empty_query_lists_the_users_current_memories() {
        let store = store();
        store.save_memory("alice note one", "alice", 0.5, None, None, None, None);
        store.save_memory("alice note two", "alice", 0.5, None, None, None, None);
        store.save_memory("bob note", "bob", 0.5, None, None, None, None);

        let results = store.search_memories("  ", "alice", None, false, 10, None);
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| (r.similarity - 1.0).abs() < f64::EPSILON)
        );
        assert_eq!(
            results[0].memory.content, "alice note two",
            "newest first, even within one second"
        );
        let newest = store.search_memories("", "alice", None, false, 1, None);
        assert_eq!(newest[0].memory.content, "alice note two");
    }

    #[test]
    fn related_memories_arrive_through_a_shared_entity() {
        let store = store();
        let wsl2 = vec!["WSL2".to_string()];
        store.save_memory(
            "TLS records get corrupted under WSL2",
            "alice",
            0.5,
            Some(&wsl2),
            None,
            None,
            None,
        );
        store.save_memory(
            "the loopback interface drops large frames",
            "alice",
            0.5,
            Some(&wsl2),
            None,
            None,
            None,
        );

        let without = store.search_memories("TLS corruption", "alice", None, false, 5, None);
        assert_eq!(without.len(), 1);

        let with = store.search_memories("TLS corruption", "alice", None, true, 5, None);
        assert_eq!(with.len(), 2, "the WSL2-sharing memory should join");
        assert!((with[1].similarity - RELATED_SCORE).abs() < f64::EPSILON);
    }

    #[test]
    fn update_supersedes_and_rejects_bad_callers() {
        let store = store();
        let original = store.save_memory("the old text", "alice", 0.5, None, None, None, None);

        assert!(matches!(
            store.update_memory("nope", "x", None, None),
            Err(UpdateError::NotFound(_))
        ));
        assert!(matches!(
            store.update_memory(&original.id, "x", None, Some("mallory")),
            Err(UpdateError::Unauthorized { .. })
        ));

        let updated = store
            .update_memory(&original.id, "the new text", None, Some("alice"))
            .expect("update");
        assert_eq!(updated.content, "the new text");
        assert_eq!(updated.supersedes.as_deref(), Some(original.id.as_str()));

        let retired = store.get_memory(&original.id).expect("old row kept");
        assert!(!retired.is_current());
        assert_eq!(retired.superseded_by.as_deref(), Some(updated.id.as_str()));
        assert!(matches!(
            store.update_memory(&original.id, "a fork", None, None),
            Err(UpdateError::Superseded { by: Some(by), .. }) if by == updated.id
        ));

        // The retired row stops matching; the new text matches. "old
        // text" still hits the new row through the shared word "text",
        // so check ids, not emptiness.
        let hits = store.search_memories("old text", "alice", None, false, 10, None);
        assert!(hits.iter().all(|r| r.memory.id != original.id), "{hits:?}");
        assert_eq!(
            store
                .search_memories("new text", "alice", None, false, 10, None)
                .len(),
            1
        );
    }

    #[test]
    fn delete_removes_record_and_index_entry() {
        let store = store();
        let m = store.save_memory(
            "something forgettable",
            "alice",
            0.5,
            None,
            None,
            None,
            None,
        );

        assert!(store.delete_memory(&m.id, None, None));
        assert!(!store.delete_memory(&m.id, None, None));
        assert!(store.get_memory(&m.id).is_none());
        assert!(
            store
                .search_memories("forgettable", "alice", None, false, 10, None)
                .is_empty()
        );
    }

    /// The CCR seam: by-hash put/get plus retrieve-by-query over the same
    /// payloads — and the CCR partition stays out of memory searches.
    #[test]
    fn ccr_payloads_roundtrip_by_hash_and_by_query() {
        let store = store();
        assert!(store.put("hash-1", "the ctx store compacts tool results"));
        assert!(store.put("hash-2", "unrelated telemetry line"));

        assert_eq!(
            store.get("hash-1").as_deref(),
            Some("the ctx store compacts tool results")
        );
        assert_eq!(store.get("missing"), None);
        assert_eq!(store.len(), 2);

        let hits = store.search_payloads("compacts tool results", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "hash-1");

        assert!(
            store
                .search_memories("compacts tool results", "alice", None, false, 10, None)
                .is_empty(),
            "CCR payloads must not leak into user memory searches"
        );
    }

    #[test]
    fn memories_survive_reopening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = {
            let store = RecallStore::open(dir.path()).expect("open recall store");
            let saved = store.save_memory(
                "durable across restarts",
                "alice",
                0.5,
                None,
                None,
                None,
                None,
            );
            assert!(store.put("ccr-1", "durable ccr payload"));
            saved.id
        };
        let reopened = RecallStore::open(dir.path()).expect("reopen recall store");
        assert!(reopened.get_memory(&id).is_some());
        assert_eq!(
            reopened
                .search_memories("durable", "alice", None, false, 10, None)
                .len(),
            1
        );
        assert_eq!(
            reopened.get("ccr-1").as_deref(),
            Some("durable ccr payload")
        );
    }
}
