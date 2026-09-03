//! Memory search cost by query size, on a copy of the live index.
//!
//! The proxy's memory stage measured 1,539 ms p50 (2026-09-03, n=2,014) and
//! the FTS cost is linear in query terms: trigram 39 ms at 20 terms, 79 at
//! 50, 276 at 200 in one process, porter a fifth of that. The three sizes
//! here are the ones the term-cap, trigram-fallback and wide-pass changes
//! (plans 1a-1c in `docs/FABLE_IDEAS_SPEED.md`) are judged on. `limit=20` is
//! the narrow pass a `top_k=5` search issues; `limit=2000` is the wide one.
//!
//! Run with:
//!     cargo bench -p headroom-core --bench memory_search
//!
//! Reads `~/.claude-personal/context-mode/memory/memories_index.db`
//! (`HEADROOM_MEMORY_INDEX` overrides the path), copies it under `target/`
//! and opens only the copy. Without the index it prints a note and registers
//! nothing, so the bench still builds and runs elsewhere.

use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion};
use headroom_core::ctx::{CtxStore, SearchOpts};

fn live_index_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HEADROOM_MEMORY_INDEX") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        Path::new(&home)
            .join(".claude-personal")
            .join("context-mode")
            .join("memory")
            .join("memories_index.db"),
    )
}

/// Copy the live database (and its WAL, which holds the newest pages) under
/// `target/`, so the bench never opens the proxy's file.
fn copy_index(src: &Path) -> std::io::Result<PathBuf> {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    let dir = target.join("memory_search_bench");
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join("memories_index.db");
    std::fs::copy(src, &dst)?;
    let dst_wal = dir.join("memories_index.db-wal");
    let _ = std::fs::remove_file(&dst_wal);
    let _ = std::fs::remove_file(dir.join("memories_index.db-shm"));
    let src_wal = src.with_extension("db-wal");
    if src_wal.exists() {
        std::fs::copy(&src_wal, &dst_wal)?;
    }
    Ok(dst)
}

/// `n` words spread evenly through the vocabulary, so a query is not one
/// alphabetical neighbourhood. Sorted, so a run is repeatable.
fn query_of(db: &Path, n: usize) -> rusqlite::Result<String> {
    let conn = rusqlite::Connection::open(db)?;
    let mut stmt =
        conn.prepare("SELECT word FROM vocabulary WHERE length(word) >= 4 ORDER BY word")?;
    let words: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let step = (words.len() / n).max(1);
    Ok(words
        .iter()
        .step_by(step)
        .take(n)
        .cloned()
        .collect::<Vec<_>>()
        .join(" "))
}

fn bench_search(c: &mut Criterion) {
    let Some(live) = live_index_path() else {
        eprintln!("memory_search: no HOME and no HEADROOM_MEMORY_INDEX; skipping");
        return;
    };
    if !live.exists() {
        eprintln!("memory_search: {} not found; skipping", live.display());
        return;
    }
    let db = match copy_index(&live) {
        Ok(db) => db,
        Err(error) => {
            eprintln!("memory_search: could not copy the index: {error}; skipping");
            return;
        }
    };
    let store = match CtxStore::open(&db) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("memory_search: could not open the copy: {error}; skipping");
            return;
        }
    };

    let mut group = c.benchmark_group("memory_search");
    // One search at 150 terms runs for hundreds of milliseconds; criterion's
    // default hundred samples would take minutes per row.
    group.sample_size(10);
    for &terms in &[20usize, 50, 150] {
        let query = match query_of(&db, terms) {
            Ok(query) => query,
            Err(error) => {
                eprintln!("memory_search: could not read the vocabulary: {error}; skipping");
                return;
            }
        };
        let queries = [query];
        for &limit in &[20usize, 2000] {
            let opts = SearchOpts {
                limit,
                ..Default::default()
            };
            group.bench_function(format!("terms={terms}/limit={limit}"), |b| {
                b.iter(|| black_box(store.search(black_box(&queries), &opts).unwrap()));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
