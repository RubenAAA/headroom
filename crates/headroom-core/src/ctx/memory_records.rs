//! Record sidecar for the FTS-backed memory backend.
//!
//! [`CtxStore`](super::CtxStore) indexes *labelled sources*: it can rank text
//! against a query, but it has no notion of a memory id, an owning user, or a
//! validity window, and no delete-by-id. The memory subsystem needs all of
//! those. So the searchable text goes in the index (labelled by memory id) and
//! the record itself goes here, keyed by the same id.
//!
//! This store is deliberately dumb — opaque JSON blobs plus the two columns
//! worth querying (`id`, `user_id`). Deciding what a memory *is* belongs to
//! the proxy's memory subsystem; this only has to hold one and give it back.
//! Keeping it here rather than in the proxy follows the crate boundary every
//! other SQLite store already respects: the proxy owns async and policy, core
//! owns the connections.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

/// SQLite-backed store of serialized memory records.
pub struct MemoryRecordStore {
    conn: Mutex<Connection>,
}

impl MemoryRecordStore {
    /// Open (or create) the store at `db_path`. Pass `":memory:"` for a
    /// per-connection ephemeral store.
    pub fn open(db_path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS memories (
              id         TEXT PRIMARY KEY,
              user_id    TEXT NOT NULL,
              record     TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_memories_user ON memories(user_id);
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Recover the lock rather than propagating a poisoned mutex: a panic in
    /// one caller must not disable memory for the process.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Insert or replace one record.
    pub fn put(&self, id: &str, user_id: &str, record: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO memories (id, user_id, record)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 record  = excluded.record,
                 user_id = excluded.user_id",
            params![id, user_id, record],
        )?;
        Ok(())
    }

    /// The serialized record for `id`, if present.
    pub fn get(&self, id: &str) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT record FROM memories WHERE id = ?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
    }

    /// Remove one record. `true` when a row was actually removed.
    pub fn delete(&self, id: &str) -> rusqlite::Result<bool> {
        let removed = self
            .conn()
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    }

    /// Every record id belonging to `user_id`. Returned before a bulk delete
    /// so the caller can clear the matching index entries too.
    pub fn ids_for_user(&self, user_id: &str) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM memories WHERE user_id = ?1")?;
        let rows = stmt.query_map(params![user_id], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Remove every record for `user_id`, returning how many went.
    pub fn delete_user(&self, user_id: &str) -> rusqlite::Result<usize> {
        self.conn()
            .execute("DELETE FROM memories WHERE user_id = ?1", params![user_id])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemoryRecordStore {
        MemoryRecordStore::open(":memory:").unwrap()
    }

    #[test]
    fn put_get_roundtrip() {
        let s = store();
        s.put("m1", "alice", r#"{"content":"hello"}"#).unwrap();
        assert_eq!(
            s.get("m1").unwrap(),
            Some(r#"{"content":"hello"}"#.to_string())
        );
        assert_eq!(s.get("missing").unwrap(), None);
    }

    #[test]
    fn put_replaces_in_place() {
        let s = store();
        s.put("m1", "alice", "first").unwrap();
        s.put("m1", "alice", "second").unwrap();
        assert_eq!(s.get("m1").unwrap(), Some("second".to_string()));
        assert_eq!(s.ids_for_user("alice").unwrap().len(), 1);
    }

    #[test]
    fn delete_reports_whether_a_row_went() {
        let s = store();
        s.put("m1", "alice", "x").unwrap();
        assert!(s.delete("m1").unwrap());
        assert!(!s.delete("m1").unwrap(), "second delete removes nothing");
    }

    #[test]
    fn user_scoping() {
        let s = store();
        s.put("m1", "alice", "a").unwrap();
        s.put("m2", "alice", "b").unwrap();
        s.put("m3", "bob", "c").unwrap();

        let mut ids = s.ids_for_user("alice").unwrap();
        ids.sort();
        assert_eq!(ids, vec!["m1", "m2"]);

        assert_eq!(s.delete_user("alice").unwrap(), 2);
        assert_eq!(s.ids_for_user("alice").unwrap().len(), 0);
        assert_eq!(s.ids_for_user("bob").unwrap().len(), 1);
    }
}
