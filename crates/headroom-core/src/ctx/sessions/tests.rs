//! Unit tests for the CTX-2a sessions store.

use super::*;
use rusqlite::Connection;
use tempfile::TempDir;

fn open_tmp() -> (TempDir, SessionsStore) {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("sessions.db");
    let store = SessionsStore::open(&db).unwrap();
    (dir, store)
}

#[test]
fn event_round_trip() {
    let (_d, store) = open_tmp();
    let id = store
        .insert_event(&NewEvent::new(
            "sess1",
            "git",
            "git",
            "git commit -m wip",
            2,
            "ctx-observer",
        ))
        .unwrap();
    assert!(id > 0);
    let events = store.get_events("sess1", 10).unwrap();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.session_id, "sess1");
    assert_eq!(e.category, "git");
    assert_eq!(e.type_, "git");
    assert_eq!(e.data, "git commit -m wip");
    assert_eq!(e.priority, 2);
    assert_eq!(e.attribution_source, "unknown");
    assert_eq!(e.project_dir, "");
    assert!(!e.data_hash.is_empty(), "data_hash auto-derived");
    assert!(!e.created_at.is_empty(), "created_at defaulted by DB");
}

#[test]
fn search_events_by_data_and_category() {
    let (_d, store) = open_tmp();
    store
        .insert_event(&NewEvent::new(
            "s",
            "error",
            "error_tool",
            "exit code 1: boom",
            2,
            "h",
        ))
        .unwrap();
    store
        .insert_event(&NewEvent::new(
            "s",
            "file",
            "file_edit",
            "src/main.rs",
            1,
            "h",
        ))
        .unwrap();

    // Match on data.
    let hits = store.search_events("boom", 10, "", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].category, "error");

    // Match on category name.
    let hits = store.search_events("file", 10, "", None).unwrap();
    assert_eq!(hits.len(), 1);

    // Category filter.
    let hits = store.search_events("", 10, "", Some("error")).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits.iter().all(|e| e.category == "error"));
}

#[test]
fn search_events_project_scope() {
    let (_d, store) = open_tmp();
    // Public bucket ('') is always visible; a specific project is scoped.
    let mut pub_ev = NewEvent::new("s", "intent", "intent", "public keyword", 1, "h");
    pub_ev.project_dir = "".into();
    store.insert_event(&pub_ev).unwrap();
    let mut proj_ev = NewEvent::new("s", "intent", "intent", "scoped keyword", 1, "h");
    proj_ev.project_dir = "/home/a".into();
    store.insert_event(&proj_ev).unwrap();

    // Query from a different project sees only the public row.
    let hits = store.search_events("keyword", 10, "/home/b", None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].data, "public keyword");

    // Query from the matching project sees both.
    let hits = store.search_events("keyword", 10, "/home/a", None).unwrap();
    assert_eq!(hits.len(), 2);
}

// ── Prefix chain ──

#[test]
fn prefix_chain_record_and_read() {
    let (_d, store) = open_tmp();
    assert!(store.last_prefix("c1").unwrap().is_none());
    store.record_prefix("c1", 2, "hashA").unwrap();
    store.record_prefix("c1", 4, "hashB").unwrap();
    let last = store.last_prefix("c1").unwrap().unwrap();
    assert_eq!(last.turn_n, 4);
    assert_eq!(last.prefix_hash, "hashB");
    assert_eq!(store.prefix_at("c1", 2).unwrap().as_deref(), Some("hashA"));
    assert_eq!(store.prefix_at("c1", 3).unwrap(), None);
}

#[test]
fn prefix_chain_upsert_overwrites() {
    let (_d, store) = open_tmp();
    store.record_prefix("c1", 2, "old").unwrap();
    store.record_prefix("c1", 2, "new").unwrap();
    assert_eq!(store.prefix_at("c1", 2).unwrap().as_deref(), Some("new"));
}

#[test]
fn session_db_path_sharded() {
    let base = Path::new("/tmp/base");
    let a = session_db_path(base, "/home/user/projA");
    let b = session_db_path(base, "/home/user/projB");
    assert_ne!(a, b);
    assert!(a.starts_with("/tmp/base/sessions"));
    let stem = a.file_stem().unwrap().to_string_lossy().to_string();
    assert_eq!(stem.len(), 16);
    assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
}

// ── TS compatibility: open a DB created with the literal TS CREATE statements ──

#[test]
fn opens_ts_created_sessions_db() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("ts.db");
    {
        // Literal TS CREATE statements copied verbatim from session/db.ts:827.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
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
            "#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_events (session_id, type, category, priority, data, source_hook, created_at)
             VALUES ('tssess', 'error_tool', 'error', 2, 'exit code 2: failed', 'ts-hook', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }

    // Open with SessionsStore — schema is IF NOT EXISTS so it adds only the
    // new conv_prefix_chain table without touching the TS tables — and read.
    let store = SessionsStore::open(&db).unwrap();
    let events = store.get_events("tssess", 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].category, "error");
    assert_eq!(events[0].data, "exit code 2: failed");
    assert_eq!(events[0].created_at, "2026-01-01T00:00:00Z");

    // And the new prefix-chain table is usable on the TS-created DB.
    store.record_prefix("c", 1, "h").unwrap();
    assert_eq!(store.prefix_at("c", 1).unwrap().as_deref(), Some("h"));
}

#[test]
fn resume_linking_recent_conversations_orders_by_recency() {
    let (_d, store) = open_tmp();
    store.record_conversation("skA", "conv1").unwrap();
    store.record_conversation("skA", "conv2").unwrap();
    // Re-touch conv1 so it is the most recent.
    store.record_conversation("skA", "conv1").unwrap();
    store.record_conversation("skB", "other").unwrap();

    // Newest-first, excluding the current conv, scoped to the session key.
    let recent = store
        .recent_conversations("skA", "conv_current", 10)
        .unwrap();
    assert_eq!(recent, vec!["conv1".to_string(), "conv2".to_string()]);

    // Excluding the current conversation drops it from the list.
    let recent = store.recent_conversations("skA", "conv1", 10).unwrap();
    assert_eq!(recent, vec!["conv2".to_string()]);

    // Different session key is isolated.
    let recent = store.recent_conversations("skB", "", 10).unwrap();
    assert_eq!(recent, vec!["other".to_string()]);
}

#[test]
fn injection_is_decided_once_and_never_overwritten() {
    let (_d, store) = open_tmp();
    assert_eq!(store.get_injection("conv1").unwrap(), None);

    store.put_injection("conv1", "FIRST").unwrap();
    assert_eq!(
        store.get_injection("conv1").unwrap().as_deref(),
        Some("FIRST")
    );

    // A second put for the same conv is a no-op (I4: decided once).
    store.put_injection("conv1", "SECOND").unwrap();
    assert_eq!(
        store.get_injection("conv1").unwrap().as_deref(),
        Some("FIRST")
    );
}
