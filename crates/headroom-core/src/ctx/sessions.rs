//! Sessions DB — a Rust port of the schema in `context-mode/src/session/db.ts`
//! (`SessionDB`), plus a new **prefix-chain** table used by the proxy's
//! conversation-identity classifier (CTX-2).
//!
//! The `session_events` / `session_meta` / `session_resume` / `tool_calls`
//! tables are **byte-compatible** with the TS store so an existing per-project
//! sessions DB at `<base>/sessions/<hash>.db` opens cleanly and every event
//! row keeps its shape (`StoredEvent`). The `conv_prefix_chain` table is new
//! (no TS equivalent) — designed here for the identity classifier.
//!
//! # Concurrency
//!
//! `rusqlite::Connection` is `!Sync`; the connection lives behind a `Mutex`
//! (same pattern as `ctx::store` and `ccr::backends::sqlite`). WAL +
//! `synchronous=NORMAL`. All access is synchronous — the proxy runs writes on a
//! detached background thread (never on the request path).
//!
//! # No silent fallbacks
//!
//! `open` propagates every rusqlite error; writes return `Result`. The caller
//! (the background observer) logs failures loudly; it never coerces an error
//! into a false success.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// How long a statement waits for the write lock before giving up.
///
/// Sized for the one writer that holds it for more than a moment: the
/// background dedup-index build, measured at 73 seconds on a copy of the
/// 9.7 GB DB. There was no timeout at all before, so an insert landing during
/// that build would have failed outright with SQLITE_BUSY. Now it queues, which
/// costs the capture worker a stall it can absorb — it is a background thread
/// with an unbounded queue. Reads never wait: WAL serves them from the
/// pre-build snapshot.
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

/// Row count above which the dedup index is built in the background instead of
/// inline. A build at this size is well under a second.
const DEDUP_INDEX_INLINE_ROW_LIMIT: i64 = 200_000;

/// Whether `idx_session_events_dedup` exists.
fn dedup_index_present(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master
              WHERE type = 'index' AND name = 'idx_session_events_dedup'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Whether `session_events` holds at most `limit` rows. Bounded by `LIMIT`, so
/// it costs the same on a 9.7 GB file as on an empty one — 14 ms, measured.
fn row_count_at_most(conn: &Connection, limit: i64) -> rusqlite::Result<bool> {
    let rows: i64 = conn.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM session_events LIMIT ?1)",
        params![limit + 1],
        |r| r.get(0),
    )?;
    Ok(rows <= limit)
}

/// Create the dedup index.
///
/// Deliberately **not unique**: the DBs that need it most already hold
/// millions of duplicates, so a unique index would refuse to build and take
/// the store down with it. The column set is still the dedup key.
fn create_dedup_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_session_events_dedup
           ON session_events(session_id, type, data_hash);",
    )
}

/// A stored event row from `session_events`. Mirrors `StoredEvent`
/// (session/db.ts:557) column-for-column.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub id: i64,
    pub session_id: String,
    pub type_: String,
    pub category: String,
    pub priority: i64,
    pub data: String,
    pub project_dir: String,
    pub attribution_source: String,
    pub attribution_confidence: f64,
    pub bytes_avoided: i64,
    pub bytes_returned: i64,
    pub source_hook: String,
    pub created_at: String,
    pub data_hash: String,
}

/// An event to insert. The write-time subset of [`StoredEvent`] (no `id` /
/// `created_at`, which the DB assigns). Defaults mirror the TS column
/// defaults so a minimally-populated event round-trips identically.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub session_id: String,
    pub type_: String,
    pub category: String,
    pub priority: i64,
    pub data: String,
    pub project_dir: String,
    pub attribution_source: String,
    pub attribution_confidence: f64,
    pub bytes_avoided: i64,
    pub bytes_returned: i64,
    pub source_hook: String,
    /// Empty string → auto-derive `sha256(data)[..16]` at insert time.
    pub data_hash: String,
}

impl NewEvent {
    /// Minimal constructor with the TS column defaults
    /// (`attribution_source='unknown'`, `priority=2`, zeros, empty project).
    pub fn new(
        session_id: impl Into<String>,
        category: impl Into<String>,
        type_: impl Into<String>,
        data: impl Into<String>,
        priority: i64,
        source_hook: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            type_: type_.into(),
            category: category.into(),
            priority,
            data: data.into(),
            project_dir: String::new(),
            attribution_source: "unknown".to_string(),
            attribution_confidence: 0.0,
            bytes_avoided: 0,
            bytes_returned: 0,
            source_hook: source_hook.into(),
            data_hash: String::new(),
        }
    }
}

/// What [`SessionsStore::insert_event`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventInsert {
    /// The row id — newly written, or the existing row when `duplicate`.
    pub id: i64,
    /// True when a row with the same `(session_id, type, data_hash)` was
    /// already there and nothing was written.
    pub duplicate: bool,
}

/// A recorded prefix-chain turn (the newest known turn for a conversation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixTurn {
    pub turn_n: u64,
    pub prefix_hash: String,
}

/// SQLite-backed sessions store.
pub struct SessionsStore {
    conn: Mutex<Connection>,
    path: PathBuf,
    /// Whether `idx_session_events_dedup` exists yet.
    ///
    /// [`SessionsStore::insert_event`] only pays for its duplicate lookup when
    /// this is set. Shared with the background index builder, which flips it
    /// the moment the index lands, so a store that opened without one starts
    /// deduplicating mid-life without being reopened.
    has_dedup_index: Arc<AtomicBool>,
}

impl SessionsStore {
    /// Lock the connection, recovering a poisoned mutex instead of propagating
    /// it.
    ///
    /// A panic while the lock was held used to make every later call panic too,
    /// for the rest of the process — one bad request took the whole store down
    /// and kept it down. Recovery is sound here because no operation spans a
    /// transaction: each is a single statement, so the connection is still
    /// usable and the earlier panic cost at most its own query.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SessionsStore {
    /// Open or create the DB at `db_path`, creating the schema if missing.
    /// Tolerates a DB created by the TypeScript `SessionDB` (identical schema)
    /// and adds the `conv_prefix_chain` table if absent.
    pub fn open(db_path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = db_path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        // A background `CREATE INDEX` holds the write lock for as long as it
        // takes to build. Without a timeout every concurrent insert would fail
        // instantly with SQLITE_BUSY instead of waiting the build out. Reads
        // are unaffected — WAL serves them from the pre-build snapshot.
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init_schema(&conn)?;
        let has_dedup_index = Arc::new(AtomicBool::new(dedup_index_present(&conn)?));
        let store = Self {
            conn: Mutex::new(conn),
            path,
            has_dedup_index,
        };
        store.ensure_dedup_index();
        Ok(store)
    }

    /// Make sure the dedup index exists, without making anyone wait for it.
    ///
    /// A small table is indexed on the spot: the build is microseconds, and
    /// doing it here means the store deduplicates from its very first insert
    /// rather than from whenever a thread happens to finish. A large one is
    /// handed to a background thread on its own connection, because the build
    /// on the 9.7 GB DB that prompted all this took 73 seconds and
    /// `ProjectStores::sessions` is reachable from the request path.
    ///
    /// Until that thread finishes, `insert_event` skips its lookup entirely
    /// (see there for why an unindexed probe is not worth its cost). The
    /// high-water mark in the capture path is what keeps duplicates out
    /// meanwhile; this index only backstops it.
    fn ensure_dedup_index(&self) {
        if self.has_dedup_index.load(Ordering::Acquire) {
            return;
        }
        let small = {
            let conn = self.conn();
            row_count_at_most(&conn, DEDUP_INDEX_INLINE_ROW_LIMIT)
        };
        match small {
            Ok(true) => {
                let conn = self.conn();
                match create_dedup_index(&conn) {
                    Ok(()) => self.has_dedup_index.store(true, Ordering::Release),
                    Err(e) => tracing::warn!(
                        event = "ctx_dedup_index_failed",
                        db = %self.path.display(),
                        error = %e,
                    ),
                }
            }
            Ok(false) => self.spawn_dedup_index_build(),
            Err(e) => tracing::warn!(
                event = "ctx_dedup_index_failed",
                db = %self.path.display(),
                error = %e,
            ),
        }
    }

    /// Build the dedup index on a second connection, off any caller's thread.
    ///
    /// Logged once per store open, never per insert: the condition lasts for
    /// one build and a line per refused lookup would bury it.
    fn spawn_dedup_index_build(&self) {
        tracing::info!(
            event = "ctx_dedup_index_missing",
            db = %self.path.display(),
            rows_at_least = DEDUP_INDEX_INLINE_ROW_LIMIT,
            "sessions DB has no dedup index; building it in the background, \
             duplicate detection is off until it lands"
        );
        let path = self.path.clone();
        let flag = Arc::clone(&self.has_dedup_index);
        let spawned = thread::Builder::new()
            .name("ctx-dedup-index".to_string())
            .spawn(move || {
                let started = Instant::now();
                let build = Connection::open(&path).and_then(|conn| {
                    conn.busy_timeout(BUSY_TIMEOUT)?;
                    create_dedup_index(&conn)
                });
                match build {
                    Ok(()) => {
                        flag.store(true, Ordering::Release);
                        tracing::info!(
                            event = "ctx_dedup_index_built",
                            db = %path.display(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "sessions dedup index built; duplicate detection is on"
                        );
                    }
                    Err(e) => tracing::warn!(
                        event = "ctx_dedup_index_failed",
                        db = %path.display(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error = %e,
                    ),
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(
                event = "ctx_dedup_index_failed",
                db = %self.path.display(),
                error = %e,
                "could not spawn the dedup index builder"
            );
        }
    }

    /// Whether `insert_event` will look for a duplicate before writing.
    pub fn dedup_active(&self) -> bool {
        self.has_dedup_index.load(Ordering::Acquire)
    }

    /// Put the store back in the state it has while the background index build
    /// is still running: index gone, lookup off.
    #[cfg(test)]
    fn drop_dedup_index_for_test(&self) {
        self.conn()
            .execute_batch("DROP INDEX IF EXISTS idx_session_events_dedup;")
            .unwrap();
        self.has_dedup_index.store(false, Ordering::Release);
    }

    /// Path the connection was opened against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the schema. The `session_events` / `session_meta` /
    /// `session_resume` / `tool_calls` blocks are copied verbatim from
    /// `SessionDB` (session/db.ts:827) so a DB is interchangeable between the
    /// two implementations. `conv_prefix_chain` is the CTX-2 addition.
    fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_events (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              session_id TEXT NOT NULL,
              type TEXT NOT NULL,
              category TEXT NOT NULL,
              priority INTEGER NOT NULL DEFAULT 2,
              data TEXT NOT NULL,
              project_dir TEXT NOT NULL DEFAULT '',
              attribution_source TEXT NOT NULL DEFAULT 'unknown',
              attribution_confidence REAL NOT NULL DEFAULT 0,
              bytes_avoided INTEGER NOT NULL DEFAULT 0,
              bytes_returned INTEGER NOT NULL DEFAULT 0,
              source_hook TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              data_hash TEXT NOT NULL DEFAULT ''
            );

            CREATE INDEX IF NOT EXISTS idx_session_events_session ON session_events(session_id);
            CREATE INDEX IF NOT EXISTS idx_session_events_type ON session_events(session_id, type);
            CREATE INDEX IF NOT EXISTS idx_session_events_priority ON session_events(session_id, priority);
            CREATE INDEX IF NOT EXISTS idx_session_events_project ON session_events(session_id, project_dir);

            CREATE TABLE IF NOT EXISTS session_meta (
              session_id TEXT PRIMARY KEY,
              project_dir TEXT NOT NULL,
              started_at TEXT NOT NULL DEFAULT (datetime('now')),
              last_event_at TEXT,
              event_count INTEGER NOT NULL DEFAULT 0,
              compact_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS session_resume (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              session_id TEXT NOT NULL UNIQUE,
              snapshot TEXT NOT NULL,
              event_count INTEGER NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              consumed INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS tool_calls (
              session_id TEXT NOT NULL,
              tool TEXT NOT NULL,
              calls INTEGER NOT NULL DEFAULT 0,
              bytes_returned INTEGER NOT NULL DEFAULT 0,
              updated_at TEXT NOT NULL DEFAULT (datetime('now')),
              PRIMARY KEY (session_id, tool)
            );

            CREATE INDEX IF NOT EXISTS idx_tool_calls_session ON tool_calls(session_id);

            -- CTX-2 addition: rolling prefix-chain for conversation identity.
            -- One row per (conversation, turn); `prefix_hash` is the rolling
            -- hash of the message prefix seen at that turn. No TS equivalent.
            CREATE TABLE IF NOT EXISTS conv_prefix_chain (
              conv_id     TEXT NOT NULL,
              turn_n      INTEGER NOT NULL,
              prefix_hash TEXT NOT NULL,
              seen_at     TEXT NOT NULL DEFAULT (datetime('now')),
              PRIMARY KEY (conv_id, turn_n)
            );
            CREATE INDEX IF NOT EXISTS idx_conv_prefix_conv ON conv_prefix_chain(conv_id);

            -- CTX-4 addition: session_key -> recent conversation ids, so a NEW
            -- conv_id carrying a resume/compaction marker can be linked back to
            -- the prior conversation under the same client session key. No TS
            -- equivalent (the TS hooks key resume off the transcript file).
            -- `seq` is a strictly-monotonic recency counter (clock-independent,
            -- so ordering is deterministic even for touches within one second).
            CREATE TABLE IF NOT EXISTS conv_by_session_key (
              session_key TEXT NOT NULL,
              conv_id     TEXT NOT NULL,
              seq         INTEGER NOT NULL,
              last_seen   TEXT NOT NULL DEFAULT (datetime('now')),
              PRIMARY KEY (session_key, conv_id)
            );
            CREATE INDEX IF NOT EXISTS idx_conv_by_sk
              ON conv_by_session_key(session_key, seq);

            -- CTX-4 addition: the injection bytes decided once per conversation
            -- and replayed verbatim every subsequent turn (invariant I4).
            CREATE TABLE IF NOT EXISTS conv_injection (
              conv_id       TEXT PRIMARY KEY,
              injected_text TEXT NOT NULL,
              created_at    TEXT NOT NULL DEFAULT (datetime('now'))
            );
            ",
        )
    }

    /// Rows above which the dedup index is left to offline maintenance.
    // ── Events ──

    /// Insert one event. Mirrors `insertEvent` (session/db.ts:915). An empty
    /// `data_hash` is auto-filled with `sha256(data)[..16]`.
    ///
    /// Idempotent on `(session_id, type, data_hash)`: when a matching row
    /// already exists nothing is written and the existing id comes back with
    /// [`EventInsert::duplicate`] set. This is a backstop, not the fix — the
    /// capture path is supposed to hand over only messages it has not captured
    /// before, and `ctx::identity::prefix_hash` explains what broke that. It
    /// stays because a resume, a compaction, or any branch legitimately makes
    /// the extractor re-read history it has already seen.
    pub fn insert_event(&self, ev: &NewEvent) -> rusqlite::Result<EventInsert> {
        let data_hash = if ev.data_hash.is_empty() {
            data_hash(&ev.data)
        } else {
            ev.data_hash.clone()
        };
        let conn = self.conn();
        // Only probe when the index is there to make the probe cheap. Without
        // it the lookup falls back to `idx_session_events_type`, which narrows
        // to one (session, type) but then reads `data_hash` from the table row
        // by row — so the cost grows with the session's own history, which on
        // the conversations that matter is exactly where it must not. Measured
        // against a copy of the 9.7 GB DB, on its worst (session, type) group
        // of 27,224 rows: 23.4 ms per miss unindexed, 0.003 ms indexed.
        let existing: Option<i64> = if self.has_dedup_index.load(Ordering::Acquire) {
            conn.query_row(
                "SELECT id FROM session_events
                  WHERE session_id = ?1 AND type = ?2 AND data_hash = ?3
                  LIMIT 1",
                params![ev.session_id, ev.type_, data_hash],
                |r| r.get(0),
            )
            .optional()?
        } else {
            None
        };
        if let Some(id) = existing {
            return Ok(EventInsert {
                id,
                duplicate: true,
            });
        }
        conn.execute(
            "INSERT INTO session_events (
               session_id, type, category, priority, data,
               project_dir, attribution_source, attribution_confidence,
               bytes_avoided, bytes_returned,
               source_hook, data_hash
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                ev.session_id,
                ev.type_,
                ev.category,
                ev.priority,
                ev.data,
                ev.project_dir,
                ev.attribution_source,
                ev.attribution_confidence,
                ev.bytes_avoided,
                ev.bytes_returned,
                ev.source_hook,
                data_hash,
            ],
        )?;
        Ok(EventInsert {
            id: conn.last_insert_rowid(),
            duplicate: false,
        })
    }

    /// Search events, mirroring `searchEvents` (session/db.ts:1086): matches
    /// `data` or `category` LIKE `query`, scoped to `project_dir` (or the
    /// public `''` bucket), optionally filtered to one `category`, ordered by
    /// id ascending.
    pub fn search_events(
        &self,
        query: &str,
        limit: usize,
        project_dir: &str,
        category: Option<&str>,
    ) -> rusqlite::Result<Vec<StoredEvent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, type, category, priority, data,
                    project_dir, attribution_source, attribution_confidence,
                    bytes_avoided, bytes_returned, source_hook, created_at, data_hash
             FROM session_events
             WHERE (project_dir = ?1 OR project_dir = '')
               AND (data LIKE '%' || ?2 || '%' ESCAPE '\\' OR category LIKE '%' || ?2 || '%' ESCAPE '\\')
               AND (?3 IS NULL OR category = ?3)
             ORDER BY id ASC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![project_dir, query, category, limit as i64],
            row_to_event,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// All events for a session, ordered by id ascending. Mirrors `getEvents`.
    pub fn get_events(&self, session_id: &str, limit: usize) -> rusqlite::Result<Vec<StoredEvent>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, type, category, priority, data,
                    project_dir, attribution_source, attribution_confidence,
                    bytes_avoided, bytes_returned, source_hook, created_at, data_hash
             FROM session_events WHERE session_id = ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], row_to_event)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ── Prefix chain (CTX-2) ──

    /// Record (or overwrite) the prefix hash for one conversation turn.
    pub fn record_prefix(
        &self,
        conv_id: &str,
        turn_n: u64,
        prefix_hash: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO conv_prefix_chain (conv_id, turn_n, prefix_hash)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(conv_id, turn_n) DO UPDATE SET
                 prefix_hash = excluded.prefix_hash,
                 seen_at     = datetime('now')",
            params![conv_id, turn_n as i64, prefix_hash],
        )?;
        Ok(())
    }

    /// The newest recorded turn for a conversation, if any.
    pub fn last_prefix(&self, conv_id: &str) -> rusqlite::Result<Option<PrefixTurn>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT turn_n, prefix_hash FROM conv_prefix_chain
             WHERE conv_id = ?1 ORDER BY turn_n DESC LIMIT 1",
            params![conv_id],
            |r| {
                Ok(PrefixTurn {
                    turn_n: r.get::<_, i64>(0)? as u64,
                    prefix_hash: r.get(1)?,
                })
            },
        )
        .optional()
    }

    /// The oldest recorded turn for a conversation, if any.
    ///
    /// Says how deep the conversation already was when this proxy first saw
    /// it. A conversation first met past the first-sight limit was declined an
    /// injection by design and will never have a row, which is not the same
    /// fault as a row that went missing.
    pub fn first_prefix_turn(&self, conv_id: &str) -> rusqlite::Result<Option<u64>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT turn_n FROM conv_prefix_chain
             WHERE conv_id = ?1 ORDER BY turn_n ASC LIMIT 1",
            params![conv_id],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )
        .optional()
    }

    /// The recorded prefix hash at a specific turn, if any.
    pub fn prefix_at(&self, conv_id: &str, turn_n: u64) -> rusqlite::Result<Option<String>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT prefix_hash FROM conv_prefix_chain WHERE conv_id = ?1 AND turn_n = ?2",
            params![conv_id, turn_n as i64],
            |r| r.get::<_, String>(0),
        )
        .optional()
    }

    // ── Resume-linking (CTX-4) ──

    /// Record that `conv_id` was seen under `session_key`, refreshing its
    /// recency. Upsert so a long-running conversation keeps floating to the top.
    pub fn record_conversation(&self, session_key: &str, conv_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO conv_by_session_key (session_key, conv_id, seq)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(seq), 0) + 1 FROM conv_by_session_key))
             ON CONFLICT(session_key, conv_id) DO UPDATE SET
                 seq = (SELECT COALESCE(MAX(seq), 0) + 1 FROM conv_by_session_key),
                 last_seen = datetime('now')",
            params![session_key, conv_id],
        )?;
        Ok(())
    }

    /// Conversations recently seen under `session_key`, newest first, excluding
    /// `exclude` (the current conversation). Used to link a resume/compaction
    /// request to the prior conversation it continues.
    pub fn recent_conversations(
        &self,
        session_key: &str,
        exclude: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT conv_id FROM conv_by_session_key
             WHERE session_key = ?1 AND conv_id <> ?2
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_key, exclude, limit as i64], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ── Injection persistence (CTX-4, invariant I4) ──

    /// Persist the injection bytes for a conversation. Decided ONCE: a second
    /// call for the same `conv_id` is a no-op (never overwrites), so the
    /// replayed prefix can never oscillate.
    pub fn put_injection(&self, conv_id: &str, injected_text: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO conv_injection (conv_id, injected_text)
             VALUES (?1, ?2)
             ON CONFLICT(conv_id) DO NOTHING",
            params![conv_id, injected_text],
        )?;
        Ok(())
    }

    /// The injection bytes previously decided for a conversation, if any.
    pub fn get_injection(&self, conv_id: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT injected_text FROM conv_injection WHERE conv_id = ?1",
            params![conv_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
    }
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<StoredEvent> {
    Ok(StoredEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        type_: row.get(2)?,
        category: row.get(3)?,
        priority: row.get(4)?,
        data: row.get(5)?,
        project_dir: row.get(6)?,
        attribution_source: row.get(7)?,
        attribution_confidence: row.get(8)?,
        bytes_avoided: row.get(9)?,
        bytes_returned: row.get(10)?,
        source_hook: row.get(11)?,
        created_at: row.get(12)?,
        data_hash: row.get(13)?,
    })
}

/// `sha256(data)` truncated to 16 hex chars — the event de-dup key.
pub fn data_hash(data: &str) -> String {
    let digest = Sha256::digest(data.as_bytes());
    let mut s = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Resolve the per-project sessions DB path: `<base>/sessions/<hash>.db`,
/// sharded by the same canonical project-dir hash as the content store
/// (`hash_project_dir_canonical`). No worktree suffix (CTX-1b: port the
/// worktree-suffix + legacy-rename migration from session/db.ts when needed).
pub fn session_db_path(base: &Path, project_dir: &str) -> PathBuf {
    base.join("sessions").join(format!(
        "{}.db",
        super::hash_project_dir_canonical(project_dir)
    ))
}

#[cfg(test)]
mod tests;
