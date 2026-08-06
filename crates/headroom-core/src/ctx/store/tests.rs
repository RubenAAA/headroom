//! Unit tests for the CTX-1 content store. `cargo test -p headroom-core ctx::`.

use super::*;
use rusqlite::{params, Connection};
use tempfile::TempDir;

fn open_tmp() -> (TempDir, CtxStore) {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("content.db");
    let store = CtxStore::open(&db).unwrap();
    (dir, store)
}

// ── Schema round-trip ──

#[test]
fn schema_created_and_reopens() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("c.db");
    {
        let store = CtxStore::open(&db).unwrap();
        store
            .index_content(
                "doc",
                "# Hello\n\nworld body text here",
                &IndexOpts::default(),
            )
            .unwrap();
    }
    // Reopen the same file — schema is IF NOT EXISTS so this must succeed and
    // preserve the data.
    let store = CtxStore::open(&db).unwrap();
    let hits = store
        .search(
            &["world".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!hits.is_empty(), "reopened DB should still be searchable");
}

#[test]
fn index_summary_counts_code_and_prose() {
    let (_d, store) = open_tmp();
    let content = "# Title\n\nsome prose paragraph\n\n## Code\n\n```rust\nfn main() {}\n```\n";
    let summary = store
        .index_content("src", content, &IndexOpts::default())
        .unwrap();
    assert_eq!(summary.label, "src");
    assert!(summary.total_chunks >= 2);
    assert_eq!(summary.code_chunks, 1, "the fenced block chunk is code");
}

#[test]
fn reindex_same_label_dedupes() {
    let (_d, store) = open_tmp();
    store
        .index_content("x", "alpha content here", &IndexOpts::default())
        .unwrap();
    store
        .index_content("x", "beta content here", &IndexOpts::default())
        .unwrap();
    // Old content must be gone.
    let alpha = store
        .search(
            &["alpha".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        alpha.is_empty(),
        "stale content should be deleted on re-index"
    );
    let beta = store
        .search(
            &["beta".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!beta.is_empty());
}

// ── Chunker ──

#[test]
fn chunk_markdown_by_headings() {
    let text = "# H1\n\nintro para\n\n## Sub A\n\nbody a\n\n## Sub B\n\nbody b";
    let chunks = test_chunk(text, MAX_CHUNK_BYTES_TEST);
    // Titles should reflect the heading stack.
    let titles: Vec<String> = chunks.iter().map(|c| c.0.clone()).collect();
    assert!(titles.iter().any(|t| t == "H1"));
    assert!(titles.iter().any(|t| t == "H1 > Sub A"));
    assert!(titles.iter().any(|t| t == "H1 > Sub B"));
}

#[test]
fn chunk_keeps_code_block_intact() {
    let text = "# Doc\n\n```\nline1\nline2\n## not a heading inside fence\nline3\n```\n";
    let chunks = test_chunk(text, MAX_CHUNK_BYTES_TEST);
    let code_chunk = chunks.iter().find(|c| c.2).expect("a code chunk");
    assert!(code_chunk.1.contains("## not a heading inside fence"));
    assert!(code_chunk.1.contains("line1") && code_chunk.1.contains("line3"));
}

#[test]
fn chunk_horizontal_rule_splits() {
    let text = "section one body\n\n---\n\nsection two body";
    let chunks = test_chunk(text, MAX_CHUNK_BYTES_TEST);
    assert!(chunks.len() >= 2, "hr should break into >=2 chunks");
}

#[test]
fn chunk_oversized_subsplits_under_cap() {
    // Build a >4096-byte body of many paragraphs with no heading.
    let para = "lorem ipsum dolor sit amet ".repeat(20); // ~540 bytes
    let mut body = String::new();
    for _ in 0..20 {
        body.push_str(&para);
        body.push_str("\n\n");
    }
    assert!(body.len() > 4096);
    let chunks = test_chunk(&body, 4096);
    assert!(chunks.len() > 1, "oversized content must sub-split");
    for (title, content, _) in &chunks {
        assert!(
            content.len() <= 4096,
            "no chunk may exceed the 4096 cap (title={title}, len={})",
            content.len()
        );
    }
}

#[test]
fn chunk_plain_text_blank_line_sections() {
    // 3+ small blank-line sections → each becomes its own chunk, titled by its
    // first line (MIN_BLANK_LINE_SECTIONS = 3).
    let text = "first section line\ndetail\n\nsecond section\n\nthird section";
    let chunks = test_chunk_plain(text, 40, MAX_CHUNK_BYTES_TEST);
    assert_eq!(chunks.len(), 3, "3 blank-line sections → 3 chunks");
    assert_eq!(chunks[0].0, "first section line");
    assert_eq!(chunks[1].0, "second section");
    assert!(chunks.iter().all(|c| !c.2), "plain text is always prose");
}

#[test]
fn chunk_plain_text_consecutive_blanks_collapse() {
    // A run of blank lines is ONE separator (JS \n\s*\n greedy over \s).
    let text = "aaa\n\n\n\nbbb\n\nccc";
    let chunks = test_chunk_plain(text, 40, MAX_CHUNK_BYTES_TEST);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].1, "aaa");
    assert_eq!(chunks[1].1, "bbb");
}

#[test]
fn chunk_plain_text_long_line_subsplits_under_cap() {
    // One huge single line (no blanks) must sub-split under the byte cap.
    let big = "x".repeat(5000);
    let chunks = test_chunk_plain(&big, 40, 4096);
    assert!(chunks.len() > 1);
    for (_, content, _) in &chunks {
        assert!(content.len() <= 4096);
    }
}

// ── Query sanitization / hostile inputs ──

#[test]
fn sanitize_escapes_hostile_fts_operators() {
    // Quotes, parens, stars, colons — must not crash MATCH; they become spaces.
    let q = sanitize_query("foo\" OR bar) AND (baz*", true);
    // AND/OR removed as operators, tokens quoted, joined by OR.
    assert!(q.contains("\"foo\""));
    assert!(q.contains("\"bar\""));
    assert!(q.contains("\"baz\""));
    assert!(!q.to_uppercase().contains(" AND "), "AND operator stripped");
}

#[test]
fn sanitize_empty_query_is_safe() {
    assert_eq!(sanitize_query("(){}[]", true), "\"\"");
    assert_eq!(sanitize_query("   ", true), "\"\"");
}

#[test]
fn sanitize_all_stopwords_falls_back() {
    // "the and for" — `and` is stripped as an FTS operator first (AND), leaving
    // ["the","for"], both stopwords → fall back to those unfiltered words.
    let q = sanitize_query("the and for", false);
    assert!(q.contains("\"the\"") && q.contains("\"for\""));
    assert!(
        !q.contains("\"and\""),
        "`and` is removed as an FTS operator"
    );
}

#[test]
fn trigram_sanitizer_drops_short() {
    assert_eq!(sanitize_trigram_query("ab", true), "");
    let q = sanitize_trigram_query("error handling", true);
    assert!(q.contains("\"error\"") && q.contains("\"handling\""));
}

#[test]
fn hostile_query_does_not_crash_search() {
    let (_d, store) = open_tmp();
    store
        .index_content("d", "the quick brown fox jumps", &IndexOpts::default())
        .unwrap();
    // A pile of FTS metacharacters must return cleanly, not error.
    let hits = store
        .search(
            &["\"*() AND OR NEAR: ^~[]".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    // No panic / no Err is the assertion; result set may be empty.
    let _ = hits;
}

// ── Search behavior ──

#[test]
fn search_finds_indexed_content() {
    let (_d, store) = open_tmp();
    store
        .index_content(
            "guide",
            "# Async\n\nspawn_blocking runs on a thread pool",
            &IndexOpts::default(),
        )
        .unwrap();
    let hits = store
        .search(
            &["spawn_blocking".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].source, "guide");
    assert_eq!(hits[0].match_layer, "rrf");
    assert!(hits[0].rank < 0.0, "RRF rank is a negative score");
}

#[test]
fn content_type_filter() {
    let (_d, store) = open_tmp();
    store
        .index_content(
            "mix",
            "# Prose\n\ndatabase connection pooling notes\n\n## Code\n\n```\ndatabase_pool.connect()\n```",
            &IndexOpts::default(),
        )
        .unwrap();
    let code = store
        .search(
            &["database".to_string()],
            &SearchOpts {
                limit: 5,
                content_type: Some(ContentType::Code),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(code.iter().all(|h| h.content_type == "code"));
    assert!(!code.is_empty(), "should find the code chunk");

    let prose = store
        .search(
            &["database".to_string()],
            &SearchOpts {
                limit: 5,
                content_type: Some(ContentType::Prose),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(prose.iter().all(|h| h.content_type == "prose"));
}

#[test]
fn source_filter_literal_substring() {
    let (_d, store) = open_tmp();
    store
        .index_content(
            "api_docs",
            "widget endpoint returns json",
            &IndexOpts::default(),
        )
        .unwrap();
    store
        .index_content("changelog", "widget bug fixed today", &IndexOpts::default())
        .unwrap();
    let hits = store
        .search(
            &["widget".to_string()],
            &SearchOpts {
                limit: 5,
                source: Some("api".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h.source == "api_docs"));
}

#[test]
fn timeline_vs_relevance_sort() {
    let (_d, store) = open_tmp();
    // Two sources indexed at (effectively) the same second; timeline sort must
    // not error and must return chronological (ascending timestamp) order.
    store
        .index_content("early", "recurring keyword alpha", &IndexOpts::default())
        .unwrap();
    store
        .index_content("late", "recurring keyword beta", &IndexOpts::default())
        .unwrap();

    let timeline = store
        .search(
            &["recurring".to_string()],
            &SearchOpts {
                limit: 10,
                sort: SortMode::Timeline,
                ..Default::default()
            },
        )
        .unwrap();
    // Ascending by timestamp.
    for w in timeline.windows(2) {
        assert!(w[0].timestamp <= w[1].timestamp);
    }

    let relevance = store
        .search(
            &["recurring".to_string()],
            &SearchOpts {
                limit: 10,
                sort: SortMode::Relevance,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!relevance.is_empty());
}

#[test]
fn fuzzy_correction_recovers_typo() {
    let (_d, store) = open_tmp();
    store
        .index_content(
            "logs",
            "authentication middleware initialized successfully",
            &IndexOpts::default(),
        )
        .unwrap();
    // "authentcation" (missing 'i') should fuzzy-correct to "authentication".
    let hits = store
        .search(
            &["authentcation".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!hits.is_empty(), "typo should be corrected via vocabulary");
    assert_eq!(hits[0].match_layer, "rrf-fuzzy");
}

#[test]
fn multi_query_merge_dedupes() {
    let (_d, store) = open_tmp();
    store
        .index_content(
            "doc",
            "# Cache\n\nprefix cache stability invariants",
            &IndexOpts::default(),
        )
        .unwrap();
    // Two queries both hitting the same chunk — merged, not duplicated.
    let hits = store
        .search(
            &["cache".to_string(), "stability".to_string()],
            &SearchOpts {
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    let mut keys: Vec<String> = hits
        .iter()
        .map(|h| format!("{}::{}", h.source, h.title))
        .collect();
    keys.sort();
    let before = keys.len();
    keys.dedup();
    assert_eq!(before, keys.len(), "no duplicate (source,title) rows");
}

// ── RRF merge order (unit) ──

#[test]
fn rrf_prefers_document_in_both_layers() {
    let (_d, store) = open_tmp();
    // "config" appears in both porter and trigram; "cfg" (short) mostly porter.
    store
        .index_content(
            "both",
            "config configuration settings config",
            &IndexOpts::default(),
        )
        .unwrap();
    store
        .index_content("onlyone", "settings values here", &IndexOpts::default())
        .unwrap();
    let hits = store
        .search(
            &["config".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        hits[0].source, "both",
        "doc matched by both FTS layers ranks first"
    );
}

// ── Path sharding ──

#[test]
fn content_db_path_is_hash_sharded() {
    let base = Path::new("/tmp/base");
    let p1 = content_db_path(base, "/home/user/projA");
    let p2 = content_db_path(base, "/home/user/projB");
    assert_ne!(p1, p2);
    assert!(p1.starts_with("/tmp/base/content"));
    assert!(p1.to_string_lossy().ends_with(".db"));
    // 16-hex-char stem.
    let stem = p1.file_stem().unwrap().to_string_lossy().to_string();
    assert_eq!(stem.len(), 16);
    assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
    // Trailing slash normalizes to the same shard.
    assert_eq!(
        content_db_path(base, "/home/user/projA/"),
        content_db_path(base, "/home/user/projA"),
    );
}

#[test]
fn hash_matches_known_sha256_prefix() {
    // sha256("/x") = 049da0.... first 16 hex chars are deterministic; assert
    // the function is a stable truncated-sha256 over the normalized path.
    let h = hash_project_dir_canonical("/x");
    assert_eq!(h.len(), 16);
    // Recompute independently.
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(b"/x");
    let expect: String = d.iter().take(8).map(|b| format!("{b:02x}")).collect();
    assert_eq!(h, expect);
}

// ── TS compatibility: open a DB created with the literal TS CREATE statements ──

#[test]
fn opens_ts_created_db() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("ts.db");
    {
        // Create the DB using the *literal* TS CREATE statements (copied
        // verbatim from context-mode/src/store.ts:463) plus a hand-inserted row.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sources (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              label TEXT NOT NULL,
              chunk_count INTEGER NOT NULL DEFAULT 0,
              code_chunk_count INTEGER NOT NULL DEFAULT 0,
              indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
              file_path TEXT,
              content_hash TEXT
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
              title, content, source_id UNINDEXED, content_type UNINDEXED,
              source_category UNINDEXED, session_id UNINDEXED, event_id UNINDEXED,
              timestamp UNINDEXED, tokenize='porter unicode61'
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_trigram USING fts5(
              title, content, source_id UNINDEXED, content_type UNINDEXED,
              source_category UNINDEXED, session_id UNINDEXED, event_id UNINDEXED,
              timestamp UNINDEXED, tokenize='trigram'
            );
            CREATE TABLE IF NOT EXISTS vocabulary (word TEXT PRIMARY KEY);
            CREATE INDEX IF NOT EXISTS idx_sources_label ON sources(label);
            "#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sources (label, chunk_count, code_chunk_count, indexed_at) VALUES ('tsdoc', 1, 0, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        for tbl in ["chunks", "chunks_trigram"] {
            conn.execute(
                &format!("INSERT INTO {tbl} (title, content, source_id, content_type, source_category, session_id, event_id, timestamp) VALUES ('Intro', 'reciprocal rank fusion pipeline', ?1, 'prose', NULL, '', '', '2026-01-01T00:00:00Z')"),
                params![sid],
            )
            .unwrap();
        }
    }

    // Now open with CtxStore — schema is IF NOT EXISTS so no clobber — and search.
    let store = CtxStore::open(&db).unwrap();
    let hits = store
        .search(
            &["reciprocal fusion".to_string()],
            &SearchOpts {
                limit: 5,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!hits.is_empty(), "must read TS-created rows");
    assert_eq!(hits[0].source, "tsdoc");
    assert_eq!(hits[0].content_type, "prose");
    assert_eq!(hits[0].timestamp.as_deref(), Some("2026-01-01T00:00:00Z"));
}

// Re-export a small chunk-testing hook and the cap constant for the tests that
// exercise the private chunker.
const MAX_CHUNK_BYTES_TEST: usize = 4096;
