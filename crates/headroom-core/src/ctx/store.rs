//! FTS5 BM25-based content store for context-mode — a byte-compatible Rust
//! port of `context-mode/src/store.ts` (`ContentStore`).
//!
//! The schema, query pipeline (dual porter+trigram FTS5 tables merged via
//! Reciprocal Rank Fusion with `K = 60`, proximity re-rank, Levenshtein typo
//! correction against a `vocabulary` table), and the markdown/blank-line
//! chunker are ported 1:1 so an existing per-project DB created by the TS
//! implementation at `~/.claude-personal/context-mode/content/<hash>.db`
//! opens cleanly and returns the same top-k for the same query.
//!
//! # Concurrency
//!
//! `rusqlite::Connection` is `!Sync`, so the connection lives behind a
//! `Mutex` (mirroring `ccr/backends/sqlite.rs`). Search is a read; index is a
//! short write. Async / `spawn_blocking` discipline is the proxy's job — this
//! module is a synchronous rusqlite wrapper by contract.
//!
//! # No silent fallbacks
//!
//! `open` propagates every rusqlite error. Search/index return `Result`; the
//! caller decides how loudly to fail. We never coerce an error into an empty
//! result set behind the caller's back.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, params_from_iter, Connection};
use sha2::{Digest, Sha256};

// ─────────────────────────────────────────────────────────
// Constants (ported verbatim from store.ts)
// ─────────────────────────────────────────────────────────

/// Oversized chunks hurt BM25 length normalization; hard cap in UTF-8 bytes.
const MAX_CHUNK_BYTES: usize = 4096;

/// Blank-line sectioning heuristic bounds.
const MIN_BLANK_LINE_SECTIONS: usize = 3;
const MAX_BLANK_LINE_SECTIONS: usize = 200;
const BLANK_SECTION_STRATEGY_MAX_BYTES: usize = 5000;

/// Leading characters of a chunk's first line used as its title.
const CHUNK_TITLE_MAX_CHARS: usize = 80;

/// Whitespace-break preference threshold when byte-splitting a long line.
const WHITESPACE_BREAK_RATIO: f64 = 0.5;

/// Standard Reciprocal Rank Fusion constant (Cormack et al. 2009).
const RRF_K: f64 = 60.0;

/// STOPWORDS — verbatim from store.ts:51.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her", "was", "one",
    "our", "out", "has", "his", "how", "its", "may", "new", "now", "old", "see", "way", "who",
    "did", "get", "got", "let", "say", "she", "too", "use", "will", "with", "this", "that", "from",
    "they", "been", "have", "many", "some", "them", "than", "each", "make", "like", "just", "over",
    "such", "take", "into", "year", "your", "good", "could", "would", "about", "which", "their",
    "there", "other", "after", "should", "through", "also", "more", "most", "only", "very", "when",
    "what", "then", "these", "those", "being", "does", "done", "both", "same", "still", "while",
    "where", "here", "were", "much", // Common in code/changelogs
    "update", "updates", "updated", "deps", "dev", "tests", "test", "add", "added", "fix", "fixed",
    "run", "running", "using",
];

fn is_stopword(w: &str) -> bool {
    // STOPWORDS are all lowercase; caller lowercases before comparing.
    STOPWORDS.contains(&w)
}

// ─────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────

/// Sort mode for [`SearchOpts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// BM25-ranked results (RRF over porter+trigram). Default.
    #[default]
    Relevance,
    /// Chronological by timestamp (ascending), matching `unified.ts`.
    Timeline,
}

/// Content-type filter: chunks are tagged `"code"` or `"prose"` at index time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Code,
    Prose,
}

impl ContentType {
    fn as_str(self) -> &'static str {
        match self {
            ContentType::Code => "code",
            ContentType::Prose => "prose",
        }
    }
}

/// Search options. Mirrors the ContentStore-relevant subset of
/// `SearchAllSourcesOpts` in `unified.ts`.
#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    /// Max results returned (post-merge). TS default is 3; callers set this.
    pub limit: usize,
    /// Restrict to sources whose `label` LIKE-matches this string (metachars
    /// escaped — the match is literal-substring, per store.ts:1107).
    pub source: Option<String>,
    /// Restrict to `code` or `prose` chunks.
    pub content_type: Option<ContentType>,
    /// Relevance (default) or timeline sort.
    pub sort: SortMode,
}

/// Optional per-index metadata. `file_path` / `content_hash` populate the
/// stale-detection columns; `session_id` / `event_id` populate the
/// attribution columns (empty-string sentinel when omitted, per store.ts).
#[derive(Debug, Clone, Default)]
pub struct IndexOpts {
    pub file_path: Option<String>,
    pub content_hash: Option<String>,
    pub session_id: Option<String>,
    pub event_id: Option<String>,
    /// When `Some(lines_per_chunk)`, chunk with the plain-text/blank-line
    /// strategy (`#chunkPlainText`) instead of the markdown chunker. Used for
    /// command output / non-markdown captures. `None` = markdown (default).
    pub plain_text_lines: Option<usize>,
}

/// Summary of an `index_content` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSummary {
    pub source_id: i64,
    pub label: String,
    pub total_chunks: usize,
    pub code_chunks: usize,
}

/// Metadata for a source row — used by CTX-5 fetch cache freshness checks.
#[derive(Debug, Clone)]
pub struct SourceMeta {
    pub chunk_count: usize,
    pub indexed_at: String,
}

/// A single search result row, shaped like `SearchResult` in store.ts.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub content: String,
    /// The owning source's `label`.
    pub source: String,
    /// RRF rank (negative score; more-negative = better), matching TS.
    pub rank: f64,
    pub content_type: String,
    pub highlighted: String,
    pub timestamp: Option<String>,
    pub session_id: String,
    /// Which pipeline layer produced this hit: `"rrf"` or `"rrf-fuzzy"`.
    pub match_layer: &'static str,
}

// ─────────────────────────────────────────────────────────
// Internal chunk type
// ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Chunk {
    title: String,
    content: String,
    has_code: bool,
}

// ─────────────────────────────────────────────────────────
// The store
// ─────────────────────────────────────────────────────────

/// SQLite FTS5 content store.
pub struct CtxStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl CtxStore {
    /// Open or create the DB at `db_path`, creating the schema if missing.
    /// Tolerates a DB created by the TypeScript `ContentStore` (identical
    /// schema), so existing per-project content DBs open cleanly.
    pub fn open(db_path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = db_path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;

        // WAL + NORMAL: readers don't block writers, and a power-loss-truncated
        // row costs at most one search miss (same rationale as ccr/sqlite.rs).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Path the connection was opened against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Delete all data from every table. Returns the total number of chunks
    /// removed (from the porter FTS table). Used by the CTX-6 `/ctx/purge`
    /// endpoint.
    pub fn purge_all(&self) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().expect("ctx store mutex poisoned");
        let chunks_deleted: usize = conn
            .execute("DELETE FROM chunks", [])
            .unwrap_or(0);
        conn.execute("DELETE FROM chunks_trigram", [])?;
        conn.execute("DELETE FROM sources", [])?;
        conn.execute("DELETE FROM vocabulary", [])?;
        Ok(chunks_deleted)
    }

    /// Look up metadata for a source by label. Returns the chunk count and
    /// indexed_at timestamp, or `None` if the source doesn't exist. Used by
    /// CTX-5 fetch to check disk-cache freshness.
    pub fn source_meta(&self, label: &str) -> rusqlite::Result<Option<SourceMeta>> {
        let conn = self.conn.lock().expect("ctx store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT chunk_count, indexed_at FROM sources WHERE label = ?1 LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![label], |row| {
            Ok(SourceMeta {
                chunk_count: row.get(0)?,
                indexed_at: row.get(1)?,
            })
        })?;
        match rows.next() {
            Some(Ok(meta)) => Ok(Some(meta)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Create the schema. Byte-identical to `ContentStore.#initSchema`
    /// (store.ts:463) so a DB created by either implementation is
    /// interchangeable. `CREATE ... IF NOT EXISTS` makes this idempotent and a
    /// no-op against a TS-created DB.
    fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sources (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              label TEXT NOT NULL,
              chunk_count INTEGER NOT NULL DEFAULT 0,
              code_chunk_count INTEGER NOT NULL DEFAULT 0,
              indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
              file_path TEXT,
              content_hash TEXT
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
              title,
              content,
              source_id UNINDEXED,
              content_type UNINDEXED,
              source_category UNINDEXED,
              session_id UNINDEXED,
              event_id UNINDEXED,
              timestamp UNINDEXED,
              tokenize='porter unicode61'
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_trigram USING fts5(
              title,
              content,
              source_id UNINDEXED,
              content_type UNINDEXED,
              source_category UNINDEXED,
              session_id UNINDEXED,
              event_id UNINDEXED,
              timestamp UNINDEXED,
              tokenize='trigram'
            );

            CREATE TABLE IF NOT EXISTS vocabulary (
              word TEXT PRIMARY KEY
            );

            CREATE INDEX IF NOT EXISTS idx_sources_label ON sources(label);
            ",
        )
    }

    // ── Index ──

    /// Chunk `content` and insert into both FTS tables + vocabulary + a
    /// `sources` row, replacing any prior source with the same `label`
    /// (atomic dedup, per store.ts:1049). Returns the counts.
    pub fn index_content(
        &self,
        label: &str,
        content: &str,
        opts: &IndexOpts,
    ) -> rusqlite::Result<IndexSummary> {
        let chunks = match opts.plain_text_lines {
            Some(lines) => chunk_plain_text(content, lines, MAX_CHUNK_BYTES),
            None => chunk_markdown(content, MAX_CHUNK_BYTES),
        };
        let code_chunks = chunks.iter().filter(|c| c.has_code).count();
        let session_id = opts.session_id.clone().unwrap_or_default();
        let event_id = opts.event_id.clone().unwrap_or_default();

        let mut conn = self.conn.lock().expect("ctx store mutex poisoned");

        // ISO-ish timestamp from SQLite so it matches the format TS wrote and
        // sorts lexicographically. Seconds precision (TS used millis) — same
        // ordering for our purposes.
        let now: String = conn.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')",
            [],
            |r| r.get(0),
        )?;

        let tx = conn.transaction()?;

        // Atomic dedup: drop the prior source with this label from both FTS
        // tables and the sources table before re-inserting.
        tx.execute(
            "DELETE FROM chunks WHERE source_id IN (SELECT id FROM sources WHERE label = ?1)",
            params![label],
        )?;
        tx.execute(
            "DELETE FROM chunks_trigram WHERE source_id IN (SELECT id FROM sources WHERE label = ?1)",
            params![label],
        )?;
        tx.execute("DELETE FROM sources WHERE label = ?1", params![label])?;

        let source_id: i64 = if chunks.is_empty() {
            tx.execute(
                "INSERT INTO sources (label, chunk_count, code_chunk_count, file_path, content_hash)
                 VALUES (?1, 0, 0, ?2, ?3)",
                params![label, opts.file_path, opts.content_hash],
            )?;
            tx.last_insert_rowid()
        } else {
            tx.execute(
                "INSERT INTO sources (label, chunk_count, code_chunk_count, file_path, content_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    label,
                    chunks.len() as i64,
                    code_chunks as i64,
                    opts.file_path,
                    opts.content_hash
                ],
            )?;
            let source_id = tx.last_insert_rowid();

            {
                let mut ins_porter = tx.prepare(
                    "INSERT INTO chunks (title, content, source_id, content_type, source_category, session_id, event_id, timestamp)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7)",
                )?;
                let mut ins_trigram = tx.prepare(
                    "INSERT INTO chunks_trigram (title, content, source_id, content_type, source_category, session_id, event_id, timestamp)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7)",
                )?;
                for chunk in &chunks {
                    let ct = if chunk.has_code { "code" } else { "prose" };
                    ins_porter.execute(params![
                        chunk.title, chunk.content, source_id, ct, session_id, event_id, now
                    ])?;
                    ins_trigram.execute(params![
                        chunk.title, chunk.content, source_id, ct, session_id, event_id, now
                    ])?;
                }
            }
            source_id
        };

        // Vocabulary extraction from the raw text (store.ts:1622).
        {
            let mut ins_vocab =
                tx.prepare("INSERT OR IGNORE INTO vocabulary (word) VALUES (?1)")?;
            for word in extract_vocabulary(content) {
                ins_vocab.execute(params![word])?;
            }
        }

        tx.commit()?;

        Ok(IndexSummary {
            source_id,
            label: label.to_string(),
            total_chunks: chunks.len(),
            code_chunks,
        })
    }

    // ── Search ──

    /// Search `queries` and return up to `opts.limit` merged hits.
    ///
    /// Each query runs the full `searchWithFallback` pipeline (RRF → fuzzy
    /// RRF). Multiple queries are merged by `source::title` key (best rank
    /// wins), then sorted per `opts.sort`. A single query reproduces the TS
    /// `searchAllSources` ContentStore path exactly.
    pub fn search(&self, queries: &[String], opts: &SearchOpts) -> rusqlite::Result<Vec<SearchHit>> {
        let conn = self.conn.lock().expect("ctx store mutex poisoned");

        let mut merged: HashMap<String, SearchHit> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for query in queries {
            let hits = search_with_fallback(&conn, query, opts)?;
            for hit in hits {
                let key = format!("{}::{}", hit.source, hit.title);
                match merged.get_mut(&key) {
                    Some(existing) => {
                        // Keep the better (more-negative) rank across queries.
                        if hit.rank < existing.rank {
                            *existing = hit;
                        }
                    }
                    None => {
                        order.push(key.clone());
                        merged.insert(key, hit);
                    }
                }
            }
        }

        let mut results: Vec<SearchHit> = order
            .into_iter()
            .filter_map(|k| merged.remove(&k))
            .collect();

        // Timestamp normalization (unified.ts:164): SQLite `datetime('now')`
        // yields "YYYY-MM-DD HH:MM:SS" (no T/Z); ISO has both. Normalize the
        // no-T form so cross-source lexical sort is consistent.
        for r in &mut results {
            if let Some(ts) = &r.timestamp {
                if !ts.contains('T') {
                    r.timestamp = Some(format!("{}Z", ts.replacen(' ', "T", 1)));
                }
            }
        }

        if opts.sort == SortMode::Timeline {
            results.sort_by(|a, b| {
                a.timestamp
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.timestamp.as_deref().unwrap_or(""))
            });
        }

        results.truncate(opts.limit);
        Ok(results)
    }
}

// ─────────────────────────────────────────────────────────
// Search pipeline (free functions — pure over a &Connection)
// ─────────────────────────────────────────────────────────

/// Port of `ContentStore.searchWithFallback` (store.ts:1340). RRF first; if it
/// returns nothing, fuzzy-correct the query against the vocabulary and re-run.
///
/// NOTE: the `#refreshStaleSources` auto-refresh step (store.ts:1349) and the
/// `sessionIdAllowSet` project filter are intentionally omitted from CTX-1 —
/// they are proxy/session concerns, not storage parity. // CTX-1b: wire
/// project-scope filtering once sessions.rs lands.
fn search_with_fallback(
    conn: &Connection,
    query: &str,
    opts: &SearchOpts,
) -> rusqlite::Result<Vec<SearchHit>> {
    let limit = opts.limit.max(1);

    // Step 1: RRF fusion.
    let rrf = rrf_search(conn, query, limit, opts)?;
    if !rrf.is_empty() {
        let mut top: Vec<SearchHit> = rrf.into_iter().take(limit).collect();
        apply_proximity_reranking(&mut top, query);
        for h in &mut top {
            h.match_layer = "rrf";
        }
        return Ok(top);
    }

    // Step 2: fuzzy correction → RRF re-run.
    let words: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|w| w.chars().count() >= 3 && !is_stopword(w))
        .map(|w| w.to_string())
        .collect();
    let original = words.join(" ");
    let corrected_words: Vec<String> = words
        .iter()
        .map(|w| fuzzy_correct(conn, w).unwrap_or_else(|| w.clone()))
        .collect();
    let corrected = corrected_words.join(" ");

    if corrected != original {
        let fuzzy = rrf_search(conn, &corrected, limit, opts)?;
        if !fuzzy.is_empty() {
            let mut top: Vec<SearchHit> = fuzzy.into_iter().take(limit).collect();
            apply_proximity_reranking(&mut top, &corrected);
            for h in &mut top {
                h.match_layer = "rrf-fuzzy";
            }
            return Ok(top);
        }
    }

    Ok(Vec::new())
}

/// Port of `ContentStore.#rrfSearch` (store.ts:1244). Merges porter-OR and
/// trigram-OR result lists via Reciprocal Rank Fusion (`K = 60`).
fn rrf_search(
    conn: &Connection,
    query: &str,
    limit: usize,
    opts: &SearchOpts,
) -> rusqlite::Result<Vec<SearchHit>> {
    let fetch_limit = (limit * 2).max(10);
    let porter = fts_search(conn, FtsTable::Porter, query, fetch_limit, opts)?;
    let trigram = fts_search(conn, FtsTable::Trigram, query, fetch_limit, opts)?;

    // key -> (hit, score). Preserve first-seen order for stable output before
    // the score sort (matches JS Map iteration order).
    let mut score_map: HashMap<String, (SearchHit, f64)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for (i, r) in porter.into_iter().enumerate() {
        let key = format!("{}::{}", r.source, r.title);
        let add = 1.0 / (RRF_K + i as f64 + 1.0);
        match score_map.get_mut(&key) {
            Some(e) => e.1 += add,
            None => {
                order.push(key.clone());
                score_map.insert(key, (r, add));
            }
        }
    }
    for (i, r) in trigram.into_iter().enumerate() {
        let key = format!("{}::{}", r.source, r.title);
        let add = 1.0 / (RRF_K + i as f64 + 1.0);
        match score_map.get_mut(&key) {
            Some(e) => e.1 += add,
            None => {
                order.push(key.clone());
                score_map.insert(key, (r, add));
            }
        }
    }

    let mut scored: Vec<(SearchHit, f64)> =
        order.into_iter().filter_map(|k| score_map.remove(&k)).collect();

    // Sort by score descending. Rust's sort is stable, so equal scores keep
    // insertion order — matching the JS `.sort((a,b)=>b.score-a.score)` on a
    // Map-ordered array.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    Ok(scored
        .into_iter()
        .take(limit)
        .map(|(mut hit, score)| {
            hit.rank = -score;
            hit
        })
        .collect())
}

#[derive(Clone, Copy)]
enum FtsTable {
    Porter,
    Trigram,
}

/// One FTS5 table query with `bm25(<table>, 5.0, 1.0)` ranking and the
/// optional source/content_type filters. Ports `search` / `searchTrigram`
/// (store.ts:1122 / :1158) in "OR" mode (the mode `#rrfSearch` uses).
fn fts_search(
    conn: &Connection,
    table: FtsTable,
    query: &str,
    limit: usize,
    opts: &SearchOpts,
) -> rusqlite::Result<Vec<SearchHit>> {
    let (table_name, match_expr) = match table {
        FtsTable::Porter => ("chunks", sanitize_query(query, true)),
        FtsTable::Trigram => ("chunks_trigram", sanitize_trigram_query(query, true)),
    };
    // Trigram sanitizer can yield an empty MATCH string → no query (store.ts:1167).
    if match_expr.is_empty() {
        return Ok(Vec::new());
    }

    // Build the SQL with the optional source/content_type predicates. Values
    // are always bound (never interpolated) — only the SQL shape varies.
    let mut sql = format!(
        "SELECT {t}.title, {t}.content, {t}.content_type, {t}.timestamp, sources.label,
                bm25({t}, 5.0, 1.0) AS rank,
                highlight({t}, 1, char(2), char(3)) AS highlighted,
                {t}.session_id
         FROM {t}
         JOIN sources ON sources.id = {t}.source_id
         WHERE {t} MATCH ?1",
        t = table_name
    );
    let mut vals: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(match_expr)];

    let mut next_idx = 2;
    if let Some(src) = &opts.source {
        sql.push_str(&format!(" AND sources.label LIKE ?{next_idx} ESCAPE '\\'"));
        vals.push(rusqlite::types::Value::Text(source_filter_param(src)));
        next_idx += 1;
    }
    if let Some(ct) = opts.content_type {
        sql.push_str(&format!(" AND {table_name}.content_type = ?{next_idx}"));
        vals.push(rusqlite::types::Value::Text(ct.as_str().to_string()));
        next_idx += 1;
    }
    sql.push_str(&format!(" ORDER BY rank LIMIT ?{next_idx}"));
    vals.push(rusqlite::types::Value::Integer(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(vals.iter()), |row| {
        Ok(SearchHit {
            title: row.get(0)?,
            content: row.get(1)?,
            content_type: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            timestamp: row.get::<_, Option<String>>(3)?,
            source: row.get(4)?,
            rank: row.get(5)?,
            highlighted: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            session_id: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            match_layer: "rrf",
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Port of `ContentStore.#sourceFilterParam` (like mode): escape LIKE
/// metacharacters so a user label is matched as a literal substring.
fn source_filter_param(source: &str) -> String {
    let escaped = source
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Port of `ContentStore.fuzzyCorrect` (store.ts:1195). Returns the nearest
/// vocabulary word within the length-dependent edit-distance budget, or `None`
/// if the query word is itself in-vocabulary or nothing is close enough.
/// (No process-local LRU cache — CTX-1 recomputes; correctness is identical.)
fn fuzzy_correct(conn: &Connection, query: &str) -> Option<String> {
    let word = query.to_lowercase();
    let word = word.trim();
    if word.chars().count() < 3 {
        return None;
    }
    let len = word.chars().count();
    let max_dist = max_edit_distance(len);

    let mut stmt = conn
        .prepare("SELECT word FROM vocabulary WHERE length(word) BETWEEN ?1 AND ?2")
        .ok()?;
    let candidates: Vec<String> = stmt
        .query_map(
            params![(len - max_dist) as i64, (len + max_dist) as i64],
            |r| r.get::<_, String>(0),
        )
        .ok()?
        .filter_map(Result::ok)
        .collect();

    let mut best: Option<String> = None;
    let mut best_dist = max_dist + 1;
    for cand in candidates {
        if cand == word {
            // Exact match: the word is in-vocab, no correction.
            return None;
        }
        let dist = levenshtein(word, &cand);
        if dist < best_dist {
            best_dist = dist;
            best = Some(cand);
        }
    }
    if best_dist <= max_dist {
        best
    } else {
        None
    }
}

fn max_edit_distance(word_len: usize) -> usize {
    if word_len <= 4 {
        1
    } else if word_len <= 12 {
        2
    } else {
        3
    }
}

/// Standard Levenshtein edit distance over Unicode scalar values.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut curr = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            curr[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1]
            } else {
                1 + prev[j].min(curr[j - 1]).min(prev[j - 1])
            };
        }
        prev = curr;
    }
    prev[b.len()]
}

// ── Proximity reranking (store.ts:1288) ──

/// In-place re-rank: layer a title-match + proximity + phrase-frequency boost
/// on top of the RRF rank. Ported from `#applyProximityReranking`.
///
/// Positions/spans use UTF-8 byte offsets and byte length rather than the
/// TS UTF-16 code-unit offsets; for ASCII (the common case) they coincide.
/// // CTX-1b: match UTF-16 semantics exactly if a non-ASCII regression appears.
fn apply_proximity_reranking(results: &mut [SearchHit], query: &str) {
    let all_terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|w| w.chars().count() >= 2)
        .map(|w| w.to_string())
        .collect();
    let filtered: Vec<String> = all_terms
        .iter()
        .filter(|w| !is_stopword(w))
        .cloned()
        .collect();
    let terms = if filtered.is_empty() {
        all_terms
    } else {
        filtered
    };

    let mut scored: Vec<(f64, f64, SearchHit)> = results
        .iter()
        .cloned()
        .map(|r| {
            let title_lower = r.title.to_lowercase();
            let title_hits = terms.iter().filter(|t| title_lower.contains(*t)).count();
            let title_weight = if r.content_type == "code" { 0.6 } else { 0.3 };
            let title_boost = if title_hits > 0 {
                title_weight * (title_hits as f64 / terms.len().max(1) as f64)
            } else {
                0.0
            };

            let mut proximity_boost = 0.0;
            let mut phrase_boost = 0.0;
            if terms.len() >= 2 {
                let content = r.content.to_lowercase();
                let positions: Vec<Vec<usize>> =
                    terms.iter().map(|t| find_all_positions(&content, t)).collect();
                if !positions.iter().any(|p| p.is_empty()) {
                    let min_span = find_min_span(&positions);
                    proximity_boost = 1.0 / (1.0 + min_span as f64 / content.len().max(1) as f64);
                    let adjacent = count_adjacent_pairs(&positions, &terms, 30);
                    phrase_boost = 0.5 * (adjacent as f64 / 4.0).min(1.0);
                }
            }
            (title_boost + proximity_boost + phrase_boost, r.rank, r)
        })
        .collect();

    // sort: boost desc, then rank asc (store.ts:1334).
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    for (slot, (_, _, hit)) in results.iter_mut().zip(scored) {
        *slot = hit;
    }
}

/// All byte offsets of `term` in `text` (store.ts:266).
fn find_all_positions(text: &str, term: &str) -> Vec<usize> {
    if term.is_empty() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut start = 0;
    while let Some(rel) = text[start..].find(term) {
        let idx = start + rel;
        positions.push(idx);
        start = idx + 1;
    }
    positions
}

/// Minimum window covering ≥1 position from each list (store.ts:317).
fn find_min_span(position_lists: &[Vec<usize>]) -> usize {
    if position_lists.is_empty() {
        return usize::MAX;
    }
    if position_lists.len() == 1 {
        return 0;
    }
    let mut ptrs = vec![0usize; position_lists.len()];
    let mut min_span = usize::MAX;
    loop {
        let mut cur_min = usize::MAX;
        let mut cur_max = 0usize;
        let mut min_idx = 0;
        for (i, list) in position_lists.iter().enumerate() {
            let val = list[ptrs[i]];
            if val < cur_min {
                cur_min = val;
                min_idx = i;
            }
            if val > cur_max {
                cur_max = val;
            }
        }
        let span = cur_max - cur_min;
        if span < min_span {
            min_span = span;
        }
        ptrs[min_idx] += 1;
        if ptrs[min_idx] >= position_lists[min_idx].len() {
            break;
        }
    }
    min_span
}

/// Count matched adjacent pairs across consecutive terms (store.ts:287).
fn count_adjacent_pairs(position_lists: &[Vec<usize>], terms: &[String], gap: usize) -> usize {
    if position_lists.len() < 2 || terms.len() < 2 {
        return 0;
    }
    let mut total = 0;
    let pairs = position_lists.len().min(terms.len()) - 1;
    for i in 0..pairs {
        let left = &position_lists[i];
        let right = &position_lists[i + 1];
        let left_len = terms[i].len();
        let mut j = 0;
        for &p in left {
            let min_start = p + left_len;
            let max_start = min_start + gap;
            while j < right.len() && right[j] < min_start {
                j += 1;
            }
            if j < right.len() && right[j] <= max_start {
                total += 1;
                j += 1;
            }
        }
    }
    total
}

// ─────────────────────────────────────────────────────────
// Query sanitization (store.ts:90 / :113)
// ─────────────────────────────────────────────────────────

/// Remove case-insensitive duplicate tokens, preserving first-seen casing.
fn dedupe_tokens(tokens: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for t in tokens {
        let key = t.to_lowercase();
        if seen.insert(key) {
            out.push(t);
        }
    }
    out
}

const FTS_OPERATORS: &[&str] = &["AND", "OR", "NOT", "NEAR"];

/// Port of `sanitizeQuery`. `or_mode=true` joins with ` OR `, else with ` `.
pub fn sanitize_query(query: &str, or_mode: bool) -> String {
    // Replace ['"(){}[]*:^~] with spaces, then split on whitespace.
    let replaced: String = query
        .chars()
        .map(|c| match c {
            '\'' | '"' | '(' | ')' | '{' | '}' | '[' | ']' | '*' | ':' | '^' | '~' => ' ',
            other => other,
        })
        .collect();
    let words = dedupe_tokens(
        replaced
            .split_whitespace()
            .filter(|w| !w.is_empty() && !FTS_OPERATORS.contains(&w.to_uppercase().as_str()))
            .map(|w| w.to_string())
            .collect(),
    );
    if words.is_empty() {
        return "\"\"".to_string();
    }
    let meaningful: Vec<String> = words
        .iter()
        .filter(|w| !is_stopword(&w.to_lowercase()))
        .cloned()
        .collect();
    let final_words = if meaningful.is_empty() { words } else { meaningful };
    join_quoted(&final_words, or_mode)
}

/// Port of `sanitizeTrigramQuery`. Returns "" when the query has < 3 usable
/// chars — the caller treats that as "no trigram query".
pub fn sanitize_trigram_query(query: &str, or_mode: bool) -> String {
    // Remove ["'(){}[]*:^~] entirely (no space substitution), then trim.
    let cleaned: String = query
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '(' | ')' | '{' | '}' | '[' | ']' | '*' | ':' | '^' | '~'))
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() < 3 {
        return String::new();
    }
    let words = dedupe_tokens(
        cleaned
            .split_whitespace()
            .filter(|w| w.chars().count() >= 3)
            .map(|w| w.to_string())
            .collect(),
    );
    if words.is_empty() {
        return String::new();
    }
    let meaningful: Vec<String> = words
        .iter()
        .filter(|w| !is_stopword(&w.to_lowercase()))
        .cloned()
        .collect();
    let final_words = if meaningful.is_empty() { words } else { meaningful };
    join_quoted(&final_words, or_mode)
}

fn join_quoted(words: &[String], or_mode: bool) -> String {
    let quoted: Vec<String> = words.iter().map(|w| format!("\"{w}\"")).collect();
    quoted.join(if or_mode { " OR " } else { " " })
}

// ─────────────────────────────────────────────────────────
// Vocabulary extraction (store.ts:1622)
// ─────────────────────────────────────────────────────────

/// Split on runs of non-(letter|number|`_`|`-`), lowercase, keep unique tokens
/// of length ≥ 3 that are not stopwords. Order-preserving de-dup.
fn extract_vocabulary(content: &str) -> Vec<String> {
    let lower = content.to_lowercase();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        if cur.chars().count() >= 3 && !is_stopword(cur) && seen.insert(cur.clone()) {
            out.push(cur.clone());
        }
        cur.clear();
    };
    for c in lower.chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out, &mut seen);
        }
    }
    flush(&mut cur, &mut out, &mut seen);
    out
}

// ─────────────────────────────────────────────────────────
// Chunker (store.ts:1646)
// ─────────────────────────────────────────────────────────

/// Port of `ContentStore.#chunkMarkdown`. Splits by markdown headings, keeps
/// code blocks intact, breaks on horizontal rules, and sub-splits any chunk
/// exceeding `max_chunk_bytes` at paragraph boundaries.
fn chunk_markdown(text: &str, max_chunk_bytes: usize) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut current_content: Vec<String> = Vec::new();
    let mut current_heading = String::new();

    // flush closure implemented inline (Rust closures can't borrow the way we
    // need across the mutation), so we call a helper.
    macro_rules! flush {
        () => {{
            flush_chunk(
                &mut chunks,
                &mut current_content,
                &heading_stack,
                &current_heading,
                max_chunk_bytes,
            );
        }};
    }

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Horizontal rule: ^[-_*]{3,}\s*$
        if is_horizontal_rule(line) {
            flush!();
            i += 1;
            continue;
        }

        // Heading: ^(#{1,4})\s+(.+)$
        if let Some((level, heading)) = parse_heading(line) {
            flush!();
            while let Some((lvl, _)) = heading_stack.last() {
                if *lvl >= level {
                    heading_stack.pop();
                } else {
                    break;
                }
            }
            heading_stack.push((level, heading.clone()));
            current_heading = heading;
            current_content.push(line.to_string());
            i += 1;
            continue;
        }

        // Code fence: ^(`{3,})(.*)?$
        if let Some(fence_len) = code_fence_len(line) {
            let fence: String = "`".repeat(fence_len);
            current_content.push(line.to_string());
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                current_content.push(l.to_string());
                if l.starts_with(&fence) && l.trim() == fence {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        current_content.push(line.to_string());
        i += 1;
    }
    flush!();

    chunks
}

fn flush_chunk(
    chunks: &mut Vec<Chunk>,
    current_content: &mut Vec<String>,
    heading_stack: &[(usize, String)],
    current_heading: &str,
    max_chunk_bytes: usize,
) {
    let joined = current_content.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        current_content.clear();
        return;
    }
    let title = build_title(heading_stack, current_heading);
    let has_code = current_content.iter().any(|l| starts_with_fence(l));

    if joined.len() <= max_chunk_bytes {
        chunks.push(Chunk {
            title,
            content: joined.to_string(),
            has_code,
        });
        current_content.clear();
        return;
    }

    // Oversized — split at paragraph boundaries (\n\n+).
    let paragraphs = split_paragraphs(joined);
    let multi = paragraphs.len() > 1;
    let mut accumulator: Vec<String> = Vec::new();
    let mut part_index = 1;

    let flush_acc =
        |accumulator: &mut Vec<String>, part_index: &mut usize, chunks: &mut Vec<Chunk>| {
            if accumulator.is_empty() {
                return;
            }
            let part = accumulator.join("\n\n");
            let part = part.trim();
            if part.is_empty() {
                accumulator.clear();
                return;
            }
            let part_title = if multi {
                format!("{title} ({part_index})")
            } else {
                title.clone()
            };
            *part_index += 1;
            chunks.push(Chunk {
                title: part_title,
                content: part.to_string(),
                has_code: part.contains("```"),
            });
            accumulator.clear();
        };

    for para in paragraphs {
        accumulator.push(para.clone());
        let candidate = accumulator.join("\n\n");
        if candidate.len() > max_chunk_bytes && accumulator.len() > 1 {
            accumulator.pop();
            flush_acc(&mut accumulator, &mut part_index, chunks);
            accumulator = vec![para];
        }
    }
    flush_acc(&mut accumulator, &mut part_index, chunks);

    current_content.clear();
}

/// Largest prefix of `s` whose UTF-8 byte length ≤ `max_bytes`, never cutting a
/// char. Guarantees forward progress (returns ≥1 char even if it overshoots).
/// Port of `#byteCappedPrefix` (store.ts:1772).
fn byte_capped_prefix(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut prefix = String::new();
    let mut bytes = 0;
    for ch in s.chars() {
        let cb = ch.len_utf8();
        if bytes + cb > max_bytes {
            break;
        }
        prefix.push(ch);
        bytes += cb;
    }
    if prefix.is_empty() {
        // A single code point wider than the cap: advance by one char.
        if let Some(c) = s.chars().next() {
            prefix.push(c);
        }
    }
    prefix
}

/// Split a single oversized plain-text chunk into byte-capped sub-chunks by
/// accumulating lines. Port of `#splitOversizedPlainChunk` (store.ts:1793).
fn split_oversized_plain_chunk(
    lines: &[&str],
    title_prefix: &str,
    max_chunk_bytes: usize,
) -> Vec<(String, String)> {
    let mut sub_chunks: Vec<(String, String)> = Vec::new();
    let mut accumulator: Vec<String> = Vec::new();
    let mut part_index = 1usize;

    let flush_acc = |accumulator: &mut Vec<String>,
                     part_index: &mut usize,
                     sub_chunks: &mut Vec<(String, String)>| {
        if accumulator.is_empty() {
            return;
        }
        let content = accumulator.join("\n");
        let part_title = if *part_index == 1 {
            title_prefix.to_string()
        } else {
            format!("{title_prefix} ({part_index})")
        };
        sub_chunks.push((part_title, content));
        *part_index += 1;
        accumulator.clear();
    };

    for line in lines {
        // A single line over the cap: split it by byte-capped prefix first.
        if line.len() > max_chunk_bytes {
            flush_acc(&mut accumulator, &mut part_index, &mut sub_chunks);
            let mut remaining = line.to_string();
            let mut line_part = 1usize;
            while !remaining.is_empty() {
                let mut slice = byte_capped_prefix(&remaining, max_chunk_bytes);
                if slice.len() < remaining.len() {
                    let last_space = slice.rfind(' ').map(|i| i as isize).unwrap_or(-1);
                    let last_newline = slice.rfind('\n').map(|i| i as isize).unwrap_or(-1);
                    let break_point = last_space.max(last_newline);
                    if break_point > (slice.len() as f64 * WHITESPACE_BREAK_RATIO) as isize {
                        slice = slice[..break_point as usize].to_string();
                    }
                }
                let title = if part_index == 1 && line_part == 1 {
                    title_prefix.to_string()
                } else {
                    format!("{title_prefix} ({part_index}.{line_part})")
                };
                let consumed = slice.len();
                sub_chunks.push((title, slice));
                remaining = remaining[consumed..].to_string();
                line_part += 1;
                part_index += 1;
            }
            continue;
        }

        let candidate = if accumulator.is_empty() {
            (*line).to_string()
        } else {
            format!("{}\n{}", accumulator.join("\n"), line)
        };
        if candidate.len() > max_chunk_bytes && !accumulator.is_empty() {
            flush_acc(&mut accumulator, &mut part_index, &mut sub_chunks);
        }
        accumulator.push((*line).to_string());
    }
    flush_acc(&mut accumulator, &mut part_index, &mut sub_chunks);
    sub_chunks
}

/// Chunk plain (non-markdown) text. Tries a blank-line section strategy for
/// naturally-sectioned output, else fixed-size line groups with 2-line overlap.
/// Port of `#chunkPlainText` (store.ts:1858). All emitted chunks are `prose`.
fn chunk_plain_text(
    text: &str,
    lines_per_chunk: usize,
    max_chunk_bytes: usize,
) -> Vec<Chunk> {
    // Blank-line splitting: \n\s*\n (a newline, optional inline whitespace, newline).
    let sections = split_blank_line_sections(text);
    if sections.len() >= MIN_BLANK_LINE_SECTIONS
        && sections.len() <= MAX_BLANK_LINE_SECTIONS
        && sections
            .iter()
            .all(|s| s.len() < BLANK_SECTION_STRATEGY_MAX_BYTES)
    {
        let mut out = Vec::new();
        for (i, section) in sections.iter().enumerate() {
            let trimmed = section.trim();
            if trimmed.is_empty() {
                continue;
            }
            let first_line = trimmed.split('\n').next().unwrap_or("");
            let title = take_chars(first_line, CHUNK_TITLE_MAX_CHARS);
            let title = if title.is_empty() {
                format!("Section {}", i + 1)
            } else {
                title
            };
            if trimmed.len() <= max_chunk_bytes {
                out.push(Chunk {
                    title,
                    content: trimmed.to_string(),
                    has_code: false,
                });
            } else {
                let lines: Vec<&str> = trimmed.split('\n').collect();
                for (t, c) in split_oversized_plain_chunk(&lines, &title, max_chunk_bytes) {
                    out.push(Chunk { title: t, content: c, has_code: false });
                }
            }
        }
        return out;
    }

    let lines: Vec<&str> = text.split('\n').collect();

    if lines.len() <= lines_per_chunk {
        if text.len() <= max_chunk_bytes {
            return vec![Chunk {
                title: "Output".to_string(),
                content: text.to_string(),
                has_code: false,
            }];
        }
        return split_oversized_plain_chunk(&lines, "Output", max_chunk_bytes)
            .into_iter()
            .map(|(t, c)| Chunk { title: t, content: c, has_code: false })
            .collect();
    }

    // Fixed-size line groups with 2-line overlap.
    let overlap = 2usize;
    let step = lines_per_chunk.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let end = (i + lines_per_chunk).min(lines.len());
        let slice = &lines[i..end];
        if slice.is_empty() {
            break;
        }
        let start_line = i + 1;
        let end_line = end;
        let first_line = take_chars(slice[0].trim(), CHUNK_TITLE_MAX_CHARS);
        let joined = slice.join("\n");
        let fallback_title = format!("Lines {start_line}-{end_line}");
        let title = if first_line.is_empty() { fallback_title.clone() } else { first_line };
        if joined.len() <= max_chunk_bytes {
            chunks.push(Chunk { title, content: joined, has_code: false });
        } else {
            let base = if title.is_empty() { fallback_title } else { title };
            for (t, c) in split_oversized_plain_chunk(slice, &base, max_chunk_bytes) {
                chunks.push(Chunk { title: t, content: c, has_code: false });
            }
        }
        i += step;
    }
    chunks
}

/// Split on `\n\s*\n` — a blank line optionally containing inline whitespace.
fn split_blank_line_sections(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut pending_newline = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // A blank (whitespace-only) line begins a separator. The JS regex
        // `\n\s*\n` is greedy over `\s`, so a run of consecutive blank lines
        // collapses into ONE separator — skip the whole run.
        if line.trim().is_empty() {
            out.push(std::mem::take(&mut cur));
            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
            pending_newline = false;
            continue;
        }
        if pending_newline {
            cur.push('\n');
        }
        cur.push_str(line);
        pending_newline = true;
        i += 1;
    }
    out.push(cur);
    out
}

/// First `max_chars` Unicode scalar values of `s`.
fn take_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Port of `#buildTitle` (store.ts:2062).
fn build_title(heading_stack: &[(usize, String)], current_heading: &str) -> String {
    if heading_stack.is_empty() {
        if current_heading.is_empty() {
            "Untitled".to_string()
        } else {
            current_heading.to_string()
        }
    } else {
        heading_stack
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    }
}

/// `^[-_*]{3,}\s*$` — 3+ of {-,_,*} (mixed allowed) then optional whitespace.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '_' | '*'))
}

/// `^(#{1,4})\s+(.+)$` → (level, trimmed heading text).
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 4 {
        return None;
    }
    let rest = &line[hashes..];
    // Require at least one whitespace char after the hashes.
    let first = rest.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    let heading = rest.trim();
    if heading.is_empty() {
        return None;
    }
    Some((hashes, heading.to_string()))
}

/// `^(`{3,})` → number of leading backticks (≥3), else None.
fn code_fence_len(line: &str) -> Option<usize> {
    let n = line.chars().take_while(|&c| c == '`').count();
    if n >= 3 {
        Some(n)
    } else {
        None
    }
}

/// `/^`{3,}/.test(line)` — used for has_code detection.
fn starts_with_fence(line: &str) -> bool {
    line.chars().take_while(|&c| c == '`').count() >= 3
}

/// Split on runs of 2+ newlines (`\n\n+`), matching JS `.split(/\n\n+/)`.
fn split_paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // Count consecutive newlines.
            let mut j = i;
            while j < bytes.len() && bytes[j] == b'\n' {
                j += 1;
            }
            let run = j - i;
            if run >= 2 {
                out.push(std::mem::take(&mut cur));
                i = j;
                continue;
            } else {
                cur.push('\n');
                i += 1;
            }
        } else {
            // Push the UTF-8 char at i.
            let ch_len = utf8_char_len(bytes[i]);
            cur.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    out.push(cur);
    out
}

fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// ─────────────────────────────────────────────────────────
// Path sharding (session/db.ts)
// ─────────────────────────────────────────────────────────

/// Normalize a project dir the way `normalizeWorktreePath` does: `\`→`/`,
/// collapse an all-slash string to `/`, and strip trailing slashes.
fn normalize_worktree_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if !normalized.is_empty() && normalized.chars().all(|c| c == '/') {
        return "/".to_string();
    }
    // Windows drive root `C:/` handling is a no-op on our Linux target; strip
    // trailing slashes (keeping the general case).
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// SHA-256 of the case-folded normalized project dir, truncated to 16 hex
/// chars (`hashProjectDirCanonical`, session/db.ts:430). Linux is
/// case-sensitive so no lowercasing is applied (matches the TS platform gate).
pub fn hash_project_dir_canonical(project_dir: &str) -> String {
    let normalized = normalize_worktree_path(project_dir);
    // On Linux the canonical hash preserves casing (only darwin/win32 fold).
    let folded = if cfg!(any(target_os = "macos", target_os = "windows")) {
        normalized.to_lowercase()
    } else {
        normalized
    };
    let digest = Sha256::digest(folded.as_bytes());
    let hex = to_hex(&digest);
    hex[..16].to_string()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Resolve the per-project content DB path: `<base>/content/<hash>.db`.
///
/// Unlike the TS `resolveContentStorePath`, this does not perform the one-shot
/// legacy-hash rename migration (that is a filesystem side effect best done
/// once, outside the storage layer). On Linux the canonical and legacy hashes
/// are equal anyway, so the returned path matches the TS result. // CTX-1b:
/// port the mac/win legacy-rename migration if we ever run there.
pub fn content_db_path(base: &Path, project_dir: &str) -> PathBuf {
    base.join("content")
        .join(format!("{}.db", hash_project_dir_canonical(project_dir)))
}

/// Default base dir: `~/.claude-personal/context-mode`. Returns `None` if
/// `$HOME` is unset (we never guess a home path).
pub fn default_base_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".claude-personal")
            .join("context-mode")
    })
}

/// Test-only hook exposing the private markdown chunker as
/// `(title, content, has_code)` tuples. Kept in the library crate (not the
/// test module) so the tests can exercise chunking without making `Chunk`
/// public.
#[cfg(test)]
pub(crate) fn test_chunk(text: &str, max_chunk_bytes: usize) -> Vec<(String, String, bool)> {
    chunk_markdown(text, max_chunk_bytes)
        .into_iter()
        .map(|c| (c.title, c.content, c.has_code))
        .collect()
}

/// Test-only hook for the plain-text/blank-line chunker.
#[cfg(test)]
pub(crate) fn test_chunk_plain(
    text: &str,
    lines_per_chunk: usize,
    max_chunk_bytes: usize,
) -> Vec<(String, String, bool)> {
    chunk_plain_text(text, lines_per_chunk, max_chunk_bytes)
        .into_iter()
        .map(|c| (c.title, c.content, c.has_code))
        .collect()
}

#[cfg(test)]
mod tests;
