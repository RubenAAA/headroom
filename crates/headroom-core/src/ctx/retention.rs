//! Age and size limits for the per-project ctx DBs.
//!
//! Capture appends to `sessions/<hash>.db` and offload to
//! `content/<hash>.db` on every turn, and nothing else ever deletes from
//! them. On 2026-10-12 one sessions DB held 4.1M events (10.5 GB) and the
//! ctx base came to 34 GB. These passes drop rows past an age limit, then
//! the oldest rows while the live data is still over a size limit, and
//! `VACUUM` once at least half the file is free pages, since deleting rows
//! alone never shrinks the file.
//!
//! They open the file with a raw connection, not [`super::SessionsStore`]
//! or [`super::CtxStore`]: opening those migrates the schema and can start
//! a 73-second index build on a large file, neither of which a cleanup
//! should do. A file without the expected table is left alone.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};

/// Matches the stores' own busy timeout: a live proxy may hold the write
/// lock, and the prune should wait it out rather than fail.
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

/// What one pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    /// Rows deleted from the main table (`session_events` or `sources`).
    pub rows: usize,
    /// Whether the file was rebuilt to give the space back.
    pub vacuumed: bool,
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE name = ?1")?
        .exists([name])
}

fn pragma(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
    conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
}

/// Bytes held by live pages: the file size minus the free list.
fn live_bytes(conn: &Connection) -> rusqlite::Result<u64> {
    let pages = pragma(conn, "page_count")? - pragma(conn, "freelist_count")?;
    Ok((pages.max(0) * pragma(conn, "page_size")?) as u64)
}

/// How many of `rows` to drop, oldest first, to bring `live` down to nine
/// tenths of `cap`, so a file at the cap is not trimmed again every day.
/// Assumes the rows hold the bytes evenly, which is close enough when one
/// table holds nearly all of them.
fn over_cap(rows: i64, live: u64, cap: u64) -> i64 {
    if live <= cap || rows <= 0 {
        return 0;
    }
    let keep = rows as f64 * 0.9 * cap as f64 / live as f64;
    (rows - keep.floor() as i64).max(1)
}

/// `VACUUM` once at least half the pages are free.
fn reclaim(conn: &Connection) -> rusqlite::Result<bool> {
    let free = pragma(conn, "freelist_count")?;
    if free == 0 || free * 2 < pragma(conn, "page_count")? {
        return Ok(false);
    }
    conn.execute_batch("VACUUM")?;
    Ok(true)
}

/// The `datetime('now', ?)` modifier for "`max_age_days` ago".
fn age_modifier(max_age_days: u32) -> String {
    format!("-{max_age_days} days")
}

/// Prune one sessions DB. Events, resume snapshots, tool counters and
/// session rows go by their own timestamps. A conversation's prefix chain
/// and injection go only when the conversation itself went quiet: the
/// injection is replayed byte for byte every turn, and dropping it from a
/// live conversation would change the prompt and cost a cache miss.
pub fn prune_sessions_db(
    path: &Path,
    max_age_days: u32,
    max_bytes: u64,
) -> rusqlite::Result<Pruned> {
    let conn = open(path)?;
    if !has_table(&conn, "session_events")? {
        return Ok(Pruned::default());
    }
    let age = age_modifier(max_age_days);
    let mut rows = conn.execute(
        "DELETE FROM session_events WHERE created_at < datetime('now', ?1)",
        params![age],
    )?;
    for (table, sql) in [
        (
            "session_resume",
            "DELETE FROM session_resume WHERE created_at < datetime('now', ?1)",
        ),
        (
            "tool_calls",
            "DELETE FROM tool_calls WHERE updated_at < datetime('now', ?1)",
        ),
        (
            "session_meta",
            "DELETE FROM session_meta WHERE COALESCE(last_event_at, started_at) < datetime('now', ?1)",
        ),
        (
            "conv_by_session_key",
            "DELETE FROM conv_by_session_key WHERE last_seen < datetime('now', ?1)",
        ),
        (
            "conv_prefix_chain",
            "DELETE FROM conv_prefix_chain WHERE conv_id IN (
               SELECT conv_id FROM conv_prefix_chain GROUP BY conv_id
               HAVING MAX(seen_at) < datetime('now', ?1))",
        ),
        (
            "conv_injection",
            "DELETE FROM conv_injection WHERE created_at < datetime('now', ?1)
               AND conv_id NOT IN (SELECT conv_id FROM conv_prefix_chain)",
        ),
    ] {
        if has_table(&conn, table)? {
            conn.execute(sql, params![age])?;
        }
    }
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM session_events", [], |r| r.get(0))?;
    let drop = over_cap(total, live_bytes(&conn)?, max_bytes);
    if drop > 0 {
        rows += conn.execute(
            "DELETE FROM session_events WHERE id IN
               (SELECT id FROM session_events ORDER BY id LIMIT ?1)",
            params![drop],
        )?;
    }
    Ok(Pruned {
        rows,
        vacuumed: reclaim(&conn)?,
    })
}

/// Delete sources picked by `which` (a `SELECT id FROM sources ...` taking
/// `?1`) with their chunks in both FTS tables.
fn delete_sources(
    conn: &Connection,
    which: &str,
    arg: &dyn rusqlite::ToSql,
) -> rusqlite::Result<usize> {
    conn.execute(
        &format!("DELETE FROM chunks WHERE source_id IN ({which})"),
        [arg],
    )?;
    conn.execute(
        &format!("DELETE FROM chunks_trigram WHERE source_id IN ({which})"),
        [arg],
    )?;
    conn.execute(&format!("DELETE FROM sources WHERE id IN ({which})"), [arg])
}

/// Prune one content DB: sources indexed before the age limit, then the
/// oldest while over the size limit. These are the cold tier behind an
/// expired CCR retrieval (`ccr.db` keeps a block a week idle), so a block
/// older than the limit stops being retrievable.
pub fn prune_content_db(
    path: &Path,
    max_age_days: u32,
    max_bytes: u64,
) -> rusqlite::Result<Pruned> {
    let conn = open(path)?;
    for table in ["sources", "chunks", "chunks_trigram"] {
        if !has_table(&conn, table)? {
            return Ok(Pruned::default());
        }
    }
    let count = || conn.query_row("SELECT COUNT(*) FROM sources", [], |r| r.get::<_, i64>(0));
    // FTS5 keeps deleted entries as tombstones, and unmerged segments, until
    // a merge, and their pages count as live. On a real 2.0 GB DB holding
    // 115 MB of text, sizing before a merge deleted down to 0.4 GB, not
    // 0.9 GB. A merge rewrites the whole index, so run it only when the file
    // is over the cap or a pass removed a tenth of the sources; FTS5's own
    // automerge handles the rest.
    let optimize = || {
        conn.execute_batch(
            "INSERT INTO chunks(chunks) VALUES('optimize');
             INSERT INTO chunks_trigram(chunks_trigram) VALUES('optimize');",
        )
    };
    let before = count()?;
    let mut rows = delete_sources(
        &conn,
        "SELECT id FROM sources WHERE indexed_at < datetime('now', ?1)",
        &age_modifier(max_age_days),
    )?;
    if rows as i64 * 10 >= before.max(1) || live_bytes(&conn)? > max_bytes {
        optimize()?;
    }
    let drop = over_cap(count()?, live_bytes(&conn)?, max_bytes);
    if drop > 0 {
        rows += delete_sources(&conn, "SELECT id FROM sources ORDER BY id LIMIT ?1", &drop)?;
        optimize()?;
    }
    Ok(Pruned {
        rows,
        vacuumed: reclaim(&conn)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{CtxStore, IndexOpts, NewEvent, SessionsStore};

    fn event(session: &str, data: &str) -> NewEvent {
        NewEvent::new(session, "tool", "tool", data, 2, "test")
    }

    fn backdate(path: &Path, sql: &str) {
        Connection::open(path).unwrap().execute(sql, []).unwrap();
    }

    fn count(path: &Path, sql: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    /// Old events go, new ones stay; a quiet conversation loses its chain
    /// and injection, a live one keeps both even when its injection is old.
    #[test]
    fn sessions_prune_drops_old_rows_and_quiet_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let store = SessionsStore::open(&path).unwrap();
            store.insert_event(&event("s1", "old")).unwrap();
            store.insert_event(&event("s1", "new")).unwrap();
            store.record_prefix("quiet", 1, "h1").unwrap();
            store.record_prefix("live", 1, "h2").unwrap();
            store.put_injection("quiet", "q").unwrap();
            store.put_injection("live", "l").unwrap();
        }
        backdate(
            &path,
            "UPDATE session_events SET created_at = '2020-01-01' WHERE data = 'old'",
        );
        backdate(
            &path,
            "UPDATE conv_prefix_chain SET seen_at = '2020-01-01' WHERE conv_id = 'quiet'",
        );
        backdate(&path, "UPDATE conv_injection SET created_at = '2020-01-01'");

        let pruned = prune_sessions_db(&path, 30, u64::MAX).unwrap();
        assert_eq!(pruned.rows, 1);
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM session_events WHERE data = 'new'"
            ),
            1
        );
        assert_eq!(count(&path, "SELECT COUNT(*) FROM session_events"), 1);
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM conv_prefix_chain WHERE conv_id = 'quiet'"
            ),
            0
        );
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM conv_prefix_chain WHERE conv_id = 'live'"
            ),
            1
        );
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM conv_injection WHERE conv_id = 'live'"
            ),
            1
        );
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM conv_injection WHERE conv_id = 'quiet'"
            ),
            0
        );
    }

    /// Over the size limit the oldest events go first, and the file shrinks.
    #[test]
    fn sessions_prune_trims_oldest_past_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let store = SessionsStore::open(&path).unwrap();
            for i in 0..400 {
                store
                    .insert_event(&event("s1", &format!("{i} {}", "x".repeat(4000))))
                    .unwrap();
            }
        }
        let before = std::fs::metadata(&path).unwrap().len();
        let pruned = prune_sessions_db(&path, 30, 400_000).unwrap();
        assert!(pruned.rows > 200, "{pruned:?}");
        assert!(pruned.vacuumed);
        let newest = count(&path, "SELECT MAX(id) FROM session_events");
        assert_eq!(newest, 400, "the newest event survives");
        assert!(std::fs::metadata(&path).unwrap().len() < before / 2);
    }

    /// Old sources and their chunks go; a recent one stays retrievable.
    #[test]
    fn content_prune_drops_old_sources_with_their_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        {
            let store = CtxStore::open(&path).unwrap();
            store
                .index_content("old", "ancient parser notes", &IndexOpts::default())
                .unwrap();
            store
                .index_content("new", "fresh parser notes", &IndexOpts::default())
                .unwrap();
        }
        backdate(
            &path,
            "UPDATE sources SET indexed_at = '2020-01-01' WHERE label = 'old'",
        );

        let pruned = prune_content_db(&path, 30, u64::MAX).unwrap();
        assert_eq!(pruned.rows, 1);
        assert_eq!(count(&path, "SELECT COUNT(*) FROM sources"), 1);
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM chunks WHERE content LIKE '%ancient%'"
            ),
            0
        );
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM chunks_trigram WHERE content LIKE '%ancient%'"
            ),
            0
        );
        assert_eq!(
            count(
                &path,
                "SELECT COUNT(*) FROM chunks WHERE content LIKE '%fresh%'"
            ),
            1
        );
    }

    /// A file that is not a ctx DB is left as it is.
    #[test]
    fn prune_skips_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.db");
        backdate(&path, "CREATE TABLE t (x)");
        assert_eq!(prune_sessions_db(&path, 30, 0).unwrap(), Pruned::default());
        assert_eq!(prune_content_db(&path, 30, 0).unwrap(), Pruned::default());
    }
}
