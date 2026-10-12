# Learning: copy a live SQLite DB with the backup API

- **Source:** ctx retention testing, 2026-10-12.
- **Evidence:** a `cp` of a live sessions DB, taken without its `-wal`
  file, opened as "database disk image is malformed". Copies made with
  Python's `sqlite3.Connection.backup` opened and pruned cleanly.
- **Cause:** in WAL mode recent commits live in the `-wal` file until a
  checkpoint; the main file alone is not a consistent snapshot.
- **Rule:** to test against real ctx data, copy with the backup API (or
  `sqlite3 src ".backup dst"`), never by copying files. Put the copies
  on disk, not `/tmp`: that is an 8G tmpfs here.
