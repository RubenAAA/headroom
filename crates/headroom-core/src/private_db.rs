//! Owner-only SQLite files.
//!
//! The CCR store, the ctx content stores and the sessions DB hold raw tool
//! output: file contents, command output, pasted secrets. `Connection::open`
//! creates the file at the process umask, usually 0644, so on a shared host
//! any local user could read them. Upstream `9b8cae84` fixed the Python side
//! the same way: create the file private from birth instead of narrowing
//! after writes have landed.
//!
//! SQLite gives the `-wal`, `-shm` and `-journal` files the mode of the main
//! database, so a private main file keeps its siblings private too. Files
//! created by an older build are narrowed on the next open, siblings
//! included.

use std::path::Path;

use rusqlite::Connection;

/// `Connection::open`, but the file (and any sibling left by an older build)
/// is owner-only before SQLite writes a byte. In-memory and URI paths are
/// passed through untouched.
pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let path = path.as_ref();
    make_private(path).map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!("making {} owner-only: {e}", path.display())),
        )
    })?;
    Connection::open(path)
}

#[cfg(unix)]
fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let s = path.as_os_str().to_string_lossy();
    if s.is_empty() || s == ":memory:" || s.starts_with("file:") {
        return Ok(());
    }
    // `create` without `truncate`: an existing database keeps its bytes, and
    // a new one is born 0600 (the umask can only clear bits, never add them).
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(suffix);
        match std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o600)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => other?,
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_database_and_its_wal_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ccr.db");
        let conn = open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        assert_eq!(mode(&db), 0o600);
        assert_eq!(mode(&dir.path().join("ccr.db-wal")), 0o600);
    }

    #[test]
    fn existing_world_readable_files_are_narrowed_and_keep_their_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("old.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (7);")
                .unwrap();
            // Keep the WAL on disk the way a crashed older build leaves it.
            std::mem::forget(conn);
        }
        let wal = dir.path().join("old.db-wal");
        for p in [&db, &wal] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let conn = open(&db).unwrap();
        assert_eq!(mode(&db), 0o600);
        assert_eq!(mode(&wal), 0o600);
        let x: i64 = conn.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 7);
    }

    #[test]
    fn in_memory_paths_pass_through() {
        open(":memory:").unwrap();
    }
}
