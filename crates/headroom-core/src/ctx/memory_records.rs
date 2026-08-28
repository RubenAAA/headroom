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
//!
//! # Entity edges
//!
//! A memory carries a list of entities it mentions. Those already live inside
//! the JSON, so a second copy in `memory_entities` needs a reason: the JSON
//! cannot be indexed, and the question worth asking of entities runs the other
//! way — not "which entities does this memory mention" but "which memories
//! mention this entity". One row per (memory, entity) with an index on the
//! entity column answers that with a join instead of a scan over every record.
//!
//! Entities are matched case-insensitively, so the edge table holds a
//! lowercased form. The record keeps whatever the caller wrote, because that
//! is what gets shown back to the model.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

/// Schema version once `memory_entities` has been built from existing records.
const ENTITY_EDGES_VERSION: i64 = 1;

/// Key form for an entity name: trimmed and lowercased.
///
/// "Neo4j" and "neo4j" are the same subject, and which one a save happens to
/// use is an accident of how the model wrote that turn. Matching on the raw
/// string would split one entity into several and lose the join.
fn normalize_entity(entity: &str) -> String {
    entity.trim().to_lowercase()
}

/// SQLite-backed store of serialized memory records.
pub struct MemoryRecordStore {
    conn: Mutex<Connection>,
}

impl MemoryRecordStore {
    /// Open (or create) the store at `db_path`. Pass `":memory:"` for a
    /// per-connection ephemeral store.
    pub fn open(db_path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;

        // One store is shared by every session and every account, so writes
        // arrive concurrently. WAL lets readers run through a write, and the
        // busy timeout makes a writer wait its turn instead of failing the save
        // with `database is locked`. Same rationale as `ctx::store`, which had
        // the pragmas from the start; this store was the one that missed them.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS memories (
              id         TEXT PRIMARY KEY,
              user_id    TEXT NOT NULL,
              record     TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_memories_user ON memories(user_id);

            CREATE TABLE IF NOT EXISTS memory_entities (
              memory_id TEXT NOT NULL,
              entity    TEXT NOT NULL,
              PRIMARY KEY (memory_id, entity)
            );
            CREATE INDEX IF NOT EXISTS idx_memory_entities_entity
              ON memory_entities(entity);
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        // Edges first. A memory whose edges outlive it would keep answering
        // entity lookups with an id that `get` cannot resolve.
        tx.execute(
            "DELETE FROM memory_entities WHERE memory_id = ?1",
            params![id],
        )?;
        let removed = tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Replace the entity edges for one memory.
    ///
    /// Delete-then-insert rather than a merge: entity lists are short and are
    /// rewritten wholesale on every save, so reconciling which ones changed
    /// would cost more than replacing them. Wrapped in a transaction so a
    /// reader never sees a memory mid-rewrite with none of its entities.
    pub fn set_entities(&self, memory_id: &str, entities: &[String]) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM memory_entities WHERE memory_id = ?1",
            params![memory_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO memory_entities (memory_id, entity) VALUES (?1, ?2)",
            )?;
            for entity in entities {
                let key = normalize_entity(entity);
                if key.is_empty() {
                    continue;
                }
                stmt.execute(params![memory_id, key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every record in the store, as `(id, json)`.
    ///
    /// Only for the entity backfill, which has to look inside records this
    /// store treats as opaque. The memory subsystem owns that shape, so it
    /// does the reading; this hands over the rows and stays ignorant.
    pub fn all_records(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, record FROM memories")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Whether the entity edges still have to be built from existing records.
    ///
    /// Answered by `user_version`, not by whether the edge table has rows: a
    /// store whose memories genuinely name no entities is indistinguishable
    /// from one that predates the table, and testing for rows would rescan
    /// every record on every open for as long as that stayed true.
    pub fn needs_entity_backfill(&self) -> rusqlite::Result<bool> {
        let version: i64 =
            self.conn()
                .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(version < ENTITY_EDGES_VERSION)
    }

    /// Record that the backfill is done. Call once it has actually run.
    pub fn mark_entity_backfill_done(&self) -> rusqlite::Result<()> {
        self.conn()
            .pragma_update(None, "user_version", ENTITY_EDGES_VERSION)
    }

    /// Arm the backfill again, so the next open rebuilds every edge.
    ///
    /// The repair for edges that have drifted from the records — and how a
    /// test reaches the pre-backfill state without forging a database file.
    pub fn reset_entity_backfill(&self) -> rusqlite::Result<()> {
        self.conn().pragma_update(None, "user_version", 0i64)
    }

    /// Ids of the memories mentioning any of `entities`.
    ///
    /// This is the one-hop expansion: given the entities on the memories a
    /// search already found, it names the other memories that share them.
    /// Order is unspecified — the caller ranks, this only reports adjacency.
    /// An empty `entities` returns nothing rather than everything.
    pub fn memories_for_entities(&self, entities: &[String]) -> rusqlite::Result<Vec<String>> {
        let keys: Vec<String> = entities
            .iter()
            .map(|e| normalize_entity(e))
            .filter(|e| !e.is_empty())
            .collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        // Built rather than bound as one parameter because SQLite has no array
        // type; the placeholders are generated, never the values.
        let placeholders = vec!["?"; keys.len()].join(",");
        let sql = format!(
            "SELECT DISTINCT memory_id FROM memory_entities WHERE entity IN ({placeholders})"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(keys), |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Ids of `user_id`'s records whose content is byte-identical to `content`.
    ///
    /// Saving the same text twice mints a second id, and the index dedups by
    /// source label, so both copies survive and both rank. In the live store
    /// that put three copies of one memory into the top five for `proxy`.
    /// Matching on the JSON field rather than a stored hash keeps this free of
    /// a migration; the table is small and SQLite scans it in microseconds.
    pub fn ids_with_content(
        &self,
        user_id: &str,
        content: &str,
    ) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM memories
             WHERE user_id = ?1 AND json_extract(record, '$.content') = ?2",
        )?;
        let rows = stmt.query_map(params![user_id, content], |r| r.get::<_, String>(0))?;
        rows.collect()
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
