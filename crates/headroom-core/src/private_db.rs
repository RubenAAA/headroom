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
    use std::os::unix::fs::OpenOptionsExt;

    let s = path.as_os_str().to_string_lossy();
    if s.is_empty() || s == ":memory:" || s.starts_with("file:") {
        return Ok(());
    }
    // A new database is born 0600 (the umask can only clear bits, never add
    // them). `create_new` fails on any existing name, symlinks included, so
    // this never opens a file someone else may hold.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow())
        .open(path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => narrow_existing(path, false)?,
        Err(e) => return Err(e),
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(suffix);
        narrow_existing(Path::new(&sibling), true)?;
    }
    Ok(())
}

#[cfg(unix)]
fn nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

/// Make an existing file 0600 without ever closing a descriptor on one that
/// already is.
///
/// Closing any descriptor on a SQLite file drops every POSIX lock this process
/// holds on it (sqlite.org/howtocorrupt.html, 2.2), and the ctx stores re-open
/// a database another connection may still be writing. So a regular 0600 file
/// is only `lstat`ed. Ownership is not checked: a 0600 file we do not own
/// cannot be opened by SQLite either, unless we are root, and root skips it
/// upstream too. Anything else is opened `O_NOFOLLOW` and narrowed through
/// that descriptor, not by path, so a swapped-in symlink cannot redirect the
/// chmod. Symlinks and other non-regular files are refused.
#[cfg(unix)]
fn narrow_existing(path: &Path, missing_ok: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let not_regular = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing {}: not a regular file", path.display()),
        )
    };
    let meta = match std::fs::symlink_metadata(path) {
        Err(e) if missing_ok && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    if meta.permissions().mode() & 0o7777 == 0o600 {
        return Ok(());
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(path)
    {
        // SQLite deleted the sidecar between the lstat and the open.
        Err(e) if missing_ok && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(not_regular());
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
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

    const PROBE_ENV: &str = "HEADROOM_PRIVATE_DB_LOCK_PROBE";

    /// Child half of `reopening_keeps_another_connections_lock`: exits 42 when
    /// the database is write-locked by another process, 43 when it is not
    /// (distinct from libtest's own codes, so a filter typo cannot pass).
    /// A no-op in a normal test run.
    #[test]
    fn lock_probe_child() {
        let Some(db) = std::env::var_os(PROBE_ENV) else {
            return;
        };
        let conn = Connection::open(db).unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let locked = conn.execute_batch("BEGIN IMMEDIATE").is_err();
        std::process::exit(if locked { 42 } else { 43 });
    }

    #[test]
    fn reopening_keeps_another_connections_lock() {
        // Closing any descriptor on a database drops every POSIX lock the
        // process holds on it (sqlite.org/howtocorrupt.html 2.2), so a second
        // `open` of a store still in use must not open and close the file.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("held.db");
        let writer = open(&db).unwrap();
        writer
            .execute_batch("CREATE TABLE t(x); BEGIN IMMEDIATE; INSERT INTO t VALUES (1);")
            .unwrap();
        let _reopened = open(&db).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "private_db::tests::lock_probe_child"])
            .env(PROBE_ENV, &db)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(42),
            "another process took the write lock this one holds"
        );
    }

    #[test]
    fn symlinked_database_is_refused_and_its_target_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.db");
        std::fs::write(&target, b"").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("store.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open(&link).is_err());
        assert_eq!(mode(&target), 0o644);
    }

    #[test]
    fn non_owner_only_modes_are_narrowed() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("odd.db");
        for m in [0o400, 0o700, 0o4600] {
            std::fs::write(&db, b"").unwrap();
            std::fs::set_permissions(&db, std::fs::Permissions::from_mode(m)).unwrap();
            make_private(&db).unwrap();
            assert_eq!(
                std::fs::metadata(&db).unwrap().permissions().mode() & 0o7777,
                0o600
            );
            std::fs::remove_file(&db).unwrap();
        }
    }
}
