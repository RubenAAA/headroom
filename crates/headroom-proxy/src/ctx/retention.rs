//! Daily sweep that keeps the ctx store bounded: every `sessions/*.db` and
//! `content/*.db` under the base, not only the projects this process has
//! opened, since a project nobody works on any more is never reopened and
//! would otherwise keep its data for good. The passes themselves live in
//! [`headroom_core::ctx::retention`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use headroom_core::ctx::retention::{Pruned, prune_content_db, prune_sessions_db};

/// Rows older than this go.
const MAX_AGE_DAYS: u32 = 30;
/// Live data kept per file, oldest rows out first past it.
const MAX_BYTES: u64 = 1 << 30;
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

type Pass = fn(&Path, u32, u64) -> rusqlite::Result<Pruned>;

/// Start the sweep on its own thread: sqlite calls block, and a `VACUUM` of
/// a large file takes minutes. Runs once at start, then every day.
pub fn spawn(base: PathBuf) {
    let started = std::thread::Builder::new()
        .name("ctx-retention".into())
        .spawn(move || {
            loop {
                sweep(&base);
                std::thread::sleep(INTERVAL);
            }
        });
    if let Err(error) = started {
        tracing::warn!(event = "ctx_retention_spawn_failed", error = %error, "ctx retention sweep not started");
    }
}

fn sweep(base: &Path) {
    let passes: [(&str, Pass); 2] = [
        ("sessions", prune_sessions_db),
        ("content", prune_content_db),
    ];
    for (kind, pass) in passes {
        let Ok(entries) = std::fs::read_dir(base.join(kind)) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().is_none_or(|ext| ext != "db") {
                continue;
            }
            match pass(&path, MAX_AGE_DAYS, MAX_BYTES) {
                Ok(Pruned {
                    rows: 0,
                    vacuumed: false,
                }) => {}
                Ok(pruned) => tracing::info!(
                    event = "ctx_retention_pruned",
                    kind = kind,
                    path = %path.display(),
                    rows = pruned.rows,
                    vacuumed = pruned.vacuumed,
                    "ctx store pruned to its age and size limits"
                ),
                Err(error) => tracing::warn!(
                    event = "ctx_retention_failed",
                    kind = kind,
                    path = %path.display(),
                    error = %error,
                    "ctx store prune failed; retried at the next sweep"
                ),
            }
        }
    }
}
