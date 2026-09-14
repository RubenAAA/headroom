//! Lossless fetch page store + template/content extract orchestration.
//!
//! Port of context-mode's `src/fetch/page-store.ts` and `src/fetch/extract.ts`
//! (upstream `next`, `5b9c00c`). Holds the complete converted document for
//! every page ever fetched plus every block with its content/template label.
//! The FTS index receives only content blocks; this store is what makes that
//! safe, because nothing is thrown away — the whole document stays here,
//! verbatim and retrievable.
//!
//! It is also the comparison set: classification asks "was this exact block
//! already seen on a different page of this host", answered by `page_blocks`
//! joined to `pages`. Nothing here truncates: full documents are stored
//! whole, no size cap, no prefix, no summary.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::fetch_blocks::{
    classify_blocks, content_text, split_blocks, template_text, BlockKind, ClassifiedBlock,
};

/// How the document reached us. `Markdown` = the site served its own
/// machine-readable version (authored for machines, no chrome to classify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchRoute {
    Markdown,
    Html,
    Json,
    Text,
}

impl FetchRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            FetchRoute::Markdown => "markdown",
            FetchRoute::Html => "html",
            FetchRoute::Json => "json",
            FetchRoute::Text => "text",
        }
    }

    pub fn from_name(s: &str) -> Self {
        match s {
            "markdown" => FetchRoute::Markdown,
            "json" => FetchRoute::Json,
            "text" => FetchRoute::Text,
            _ => FetchRoute::Html,
        }
    }
}

/// JSON and plain-text responses are not web pages and have no chrome; they
/// pass through untouched so the existing JSON/text indexing strategies keep
/// their exact behaviour.
pub fn route_skips_extraction(route: FetchRoute) -> bool {
    matches!(route, FetchRoute::Json | FetchRoute::Text)
}

/// Canonical identity of a fetched page: the URL minus its fragment. Two
/// fetches of the same page must be the same row, or a page compared against
/// an older copy of itself marks its own article as template.
pub fn page_key_for(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// Host of a URL, lower-cased, with an explicit port kept — mirroring
/// upstream's `URL.host`, which serializes as `host[:port]` (IPv6 bracketed).
/// Empty string when the URL will not parse.
pub fn host_for(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => {
                // `host_str` strips IPv6 brackets; restore them so the value
                // reads exactly like upstream's `URL.host`.
                let host = host.to_lowercase();
                let host = if host.contains(':') && !host.starts_with('[') {
                    format!("[{host}]")
                } else {
                    host
                };
                match parsed.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host,
                }
            }
            None => String::new(),
        },
        Err(_) => String::new(),
    }
}

/// One stored page: the complete converted document plus its metadata.
#[derive(Debug, Clone)]
pub struct StoredPage {
    pub page_key: String,
    pub host: String,
    pub url: String,
    pub source_label: String,
    pub route: String,
    pub provisional: bool,
    pub full_text: String,
}

/// One stored block with its label.
#[derive(Debug, Clone)]
pub struct StoredBlock {
    pub ordinal: usize,
    pub hash: String,
    pub kind: BlockKind,
    pub raw: String,
    pub text: String,
}

impl StoredBlock {
    /// The block without its label, for re-classification and text rendering.
    fn as_block(&self) -> super::fetch_blocks::Block {
        super::fetch_blocks::Block {
            ordinal: self.ordinal,
            raw: self.raw.clone(),
            text: self.text.clone(),
            hash: self.hash.clone(),
        }
    }
}

/// A cold-start page resolved by a later page of the same host: text to
/// re-index under the same label (the FTS `index_content` replaces rows
/// sharing a label, so this swaps provisional content for classified).
#[derive(Debug, Clone)]
pub struct Relabelled {
    pub source_label: String,
    pub url: String,
    pub index_text: String,
    pub template_bytes: usize,
}

/// Outcome of [`extract_and_store`].
#[derive(Debug)]
pub enum ExtractOutcome {
    Index {
        /// Text for the FTS index — content blocks only.
        index_text: String,
        /// Complete document as stored (byte accounting).
        stored_bytes: usize,
        content_bytes: usize,
        template_bytes: usize,
        template_blocks: usize,
        total_blocks: usize,
        provisional: bool,
        route: FetchRoute,
        /// Earlier provisional pages of this host now resolved.
        relabelled: Vec<Relabelled>,
    },
    Refuse {
        reason: String,
        stored_bytes: usize,
        route: FetchRoute,
    },
}

/// SQLite sidecar beside the FTS content DB. Synchronous by contract (like
/// `CtxStore`); callers must not hold it across `.await` points.
pub struct PageStore {
    conn: Connection,
}

impl PageStore {
    fn init(conn: &Connection) -> rusqlite::Result<()> {
        // Same rationale as CtxStore::open: WAL so readers never block the
        // single writer; NORMAL sync so a torn row costs a search miss, not
        // integrity. Busy timeout because concurrent fetches share the file.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pages (
               page_key TEXT PRIMARY KEY,
               host TEXT NOT NULL,
               url TEXT NOT NULL,
               source_label TEXT NOT NULL,
               route TEXT NOT NULL,
               provisional INTEGER NOT NULL DEFAULT 0,
               fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
               full_text TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS page_blocks (
               page_key TEXT NOT NULL,
               ordinal INTEGER NOT NULL,
               hash TEXT NOT NULL,
               kind TEXT NOT NULL,
               raw TEXT NOT NULL,
               text TEXT NOT NULL,
               PRIMARY KEY (page_key, ordinal)
             );
             CREATE INDEX IF NOT EXISTS idx_pages_host ON pages(host);
             CREATE INDEX IF NOT EXISTS idx_page_blocks_hash ON page_blocks(hash);",
        )?;
        Ok(())
    }

    /// Open (creating) the sidecar at `db_path`.
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    /// In-memory sidecar. Used when the content DB itself is in-memory
    /// (tests) and as the graceful fallback when the file cannot be opened —
    /// extraction still classifies within the process instead of dying.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    /// Distinct pages already recorded for a host, excluding `except_page_key`.
    pub fn host_page_count(&self, host: &str, except_page_key: &str) -> rusqlite::Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pages WHERE host = ?1 AND page_key <> ?2",
            params![host, except_page_key],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    /// How many distinct *other* pages of this host carry each hash. The
    /// `page_key <> ?` clause keeps a page from classifying itself.
    pub fn other_page_counts(
        &self,
        host: &str,
        except_page_key: &str,
        hashes: &[String],
    ) -> rusqlite::Result<HashMap<String, usize>> {
        let unique: HashSet<&str> = hashes.iter().map(String::as_str).collect();
        let mut counts = HashMap::with_capacity(unique.len());
        for h in unique {
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(DISTINCT b.page_key) FROM page_blocks b
                 JOIN pages p ON p.page_key = b.page_key
                 WHERE b.hash = ?1 AND p.host = ?2 AND b.page_key <> ?3",
                params![h, host, except_page_key],
                |row| row.get(0),
            )?;
            counts.insert(h.to_string(), n.max(0) as usize);
        }
        Ok(counts)
    }

    /// Store one page whole: the complete document plus every labelled block,
    /// replacing any prior fetch of the same page key.
    pub fn record_page(
        &self,
        page: &StoredPage,
        blocks: &[ClassifiedBlock],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO pages (page_key, host, url, source_label, route, provisional, fetched_at, full_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), ?7)
             ON CONFLICT(page_key) DO UPDATE SET
               host = excluded.host, url = excluded.url, source_label = excluded.source_label,
               route = excluded.route, provisional = excluded.provisional,
               fetched_at = excluded.fetched_at, full_text = excluded.full_text",
            params![
                page.page_key,
                page.host,
                page.url,
                page.source_label,
                page.route,
                i64::from(page.provisional),
                page.full_text,
            ],
        )?;
        tx.execute(
            "DELETE FROM page_blocks WHERE page_key = ?1",
            params![page.page_key],
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO page_blocks (page_key, ordinal, hash, kind, raw, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for b in blocks {
                ins.execute(params![
                    page.page_key,
                    b.ordinal as i64,
                    b.hash,
                    b.kind.as_str(),
                    b.raw,
                    b.text,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Update stored labels after a re-run, and clear the provisional flag.
    pub fn relabel_page(
        &self,
        page_key: &str,
        blocks: &[ClassifiedBlock],
        provisional: bool,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut upd = tx
                .prepare("UPDATE page_blocks SET kind = ?1 WHERE page_key = ?2 AND ordinal = ?3")?;
            for b in blocks {
                upd.execute(params![b.kind.as_str(), page_key, b.ordinal as i64])?;
            }
        }
        tx.execute(
            "UPDATE pages SET provisional = ?1 WHERE page_key = ?2",
            params![i64::from(provisional), page_key],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every page of a host still carrying a cold-start (provisional)
    /// labelling, excluding the page that just landed.
    pub fn provisional_pages(
        &self,
        host: &str,
        except_page_key: &str,
    ) -> rusqlite::Result<Vec<StoredPage>> {
        let mut stmt = self.conn.prepare(
            "SELECT page_key, host, url, source_label, route, provisional, full_text
             FROM pages WHERE host = ?1 AND provisional = 1 AND page_key <> ?2",
        )?;
        let rows = stmt.query_map(params![host, except_page_key], |row| {
            Ok(StoredPage {
                page_key: row.get(0)?,
                host: row.get(1)?,
                url: row.get(2)?,
                source_label: row.get(3)?,
                route: row.get(4)?,
                provisional: row.get::<_, i64>(5)? == 1,
                full_text: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// Every stored block of a page in document order — content and template.
    pub fn blocks_of(&self, page_key: &str) -> rusqlite::Result<Vec<StoredBlock>> {
        let mut stmt = self.conn.prepare(
            "SELECT ordinal, hash, kind, raw, text FROM page_blocks
             WHERE page_key = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![page_key], |row| {
            Ok(StoredBlock {
                ordinal: row.get::<_, i64>(0)? as usize,
                hash: row.get(1)?,
                kind: BlockKind::from_label(&row.get::<_, String>(2)?),
                raw: row.get(3)?,
                text: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// The complete converted document as it was stored, or `None` when the
    /// page was never fetched. (Upstream `fullTextOf`; used by its
    /// measurement scripts — kept here so the stored side stays retrievable.)
    pub fn full_text_of(&self, page_key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT full_text FROM pages WHERE page_key = ?1",
                params![page_key],
                |row| row.get(0),
            )
            .optional()
    }
}

/// The chrome that was labelled out of a stored page, for retrieval.
/// (Upstream `storedTemplateText`.)
pub fn stored_template_text(store: &PageStore, url: &str) -> rusqlite::Result<String> {
    let blocks = store.blocks_of(&page_key_for(url))?;
    let classified: Vec<super::fetch_blocks::ClassifiedBlock> = blocks
        .iter()
        .map(|b| super::fetch_blocks::ClassifiedBlock {
            ordinal: b.ordinal,
            raw: b.raw.clone(),
            text: b.text.clone(),
            hash: b.hash.clone(),
            kind: b.kind,
        })
        .collect();
    Ok(template_text(&classified))
}

/// Run the lossless template/content pass for one fetch: cheapest correct
/// answer first (site-authored markdown skips classification), store the page
/// whole either way, refuse only when the page carries no page-specific
/// content at all. Never truncates; labels, never drops.
pub fn extract_and_store(
    url: &str,
    source_label: &str,
    document: &str,
    route: FetchRoute,
    store: &PageStore,
) -> rusqlite::Result<ExtractOutcome> {
    let page_key = page_key_for(url);
    let host = host_for(url);
    let stored_bytes = document.len();

    let blocks = split_blocks(document);
    let authored = route == FetchRoute::Markdown;
    let host_page_count = store.host_page_count(&host, &page_key)?;
    let counts = if authored {
        HashMap::new()
    } else {
        let hashes: Vec<String> = blocks.iter().map(|b| b.hash.clone()).collect();
        store.other_page_counts(&host, &page_key, &hashes)?
    };

    let result = classify_blocks(blocks, &counts, host_page_count, authored);

    // Store the page WHOLE regardless of the verdict — including when about
    // to refuse. Refusing to index is not a licence to discard bytes.
    store.record_page(
        &StoredPage {
            page_key: page_key.clone(),
            host: host.clone(),
            url: url.to_string(),
            source_label: source_label.to_string(),
            route: route.as_str().to_string(),
            provisional: result.provisional,
            full_text: document.to_string(),
        },
        &result.blocks,
    )?;

    if result.all_template {
        return Ok(ExtractOutcome::Refuse {
            reason: format!(
                "every block of this page is byte-identical to blocks already seen on other pages of {host}, \
                 so the response carried the site shell rather than this page — its content is rendered \
                 client-side by JavaScript and an HTTP fetch cannot see it. Nothing was indexed (the response \
                 is stored whole and unaltered). Retrying this URL returns the same shell; look for this site's \
                 llms.txt, a raw .md source, an OpenAPI spec, or a repository README instead."
            ),
            stored_bytes,
            route,
        });
    }

    // A second page of a host resolves every cold-start page that came
    // before it. A first page wrongly labelled and never revisited is
    // exactly the silent loss this design forbids.
    let mut relabelled = Vec::new();
    if !result.provisional {
        for prev in store.provisional_pages(&host, &page_key)? {
            let prev_blocks = store.blocks_of(&prev.page_key)?;
            let prev_hashes: Vec<String> = prev_blocks.iter().map(|b| b.hash.clone()).collect();
            let prev_counts = store.other_page_counts(&host, &prev.page_key, &prev_hashes)?;
            let prev_host_count = store.host_page_count(&prev.host, &prev.page_key)?;
            let rerun = classify_blocks(
                prev_blocks.iter().map(StoredBlock::as_block).collect(),
                &prev_counts,
                prev_host_count,
                FetchRoute::from_name(&prev.route) == FetchRoute::Markdown,
            );
            // An all-template re-run means the earlier page was a shell too.
            // Leave its stored bytes and index rows alone rather than
            // emptying a source already being searched.
            if rerun.all_template {
                continue;
            }
            store.relabel_page(&prev.page_key, &rerun.blocks, rerun.provisional)?;
            if rerun.template_bytes > 0 {
                relabelled.push(Relabelled {
                    source_label: prev.source_label.clone(),
                    url: prev.url.clone(),
                    index_text: content_text(&rerun.blocks),
                    template_bytes: rerun.template_bytes,
                });
            }
        }
    }

    let template_blocks = result
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Template)
        .count();
    Ok(ExtractOutcome::Index {
        index_text: content_text(&result.blocks),
        stored_bytes,
        content_bytes: result.content_bytes,
        template_bytes: result.template_bytes,
        template_blocks,
        total_blocks: result.blocks.len(),
        provisional: result.provisional,
        route,
        relabelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_key_strips_fragment_only() {
        assert_eq!(
            page_key_for("https://x.dev/a#b"),
            "https://x.dev/a".to_string()
        );
        assert_eq!(page_key_for("not a url"), "not a url".to_string());
    }

    #[test]
    fn host_is_lowercase_with_port() {
        assert_eq!(host_for("https://X.DEV:8443/a"), "x.dev:8443".to_string());
        assert_eq!(host_for("https://x.dev/a"), "x.dev".to_string());
        // Default ports vanish (WHATWG), explicit ones stay — as upstream's
        // `URL.host`. IPv6 keeps its brackets.
        assert_eq!(host_for("https://x.dev:443/a"), "x.dev".to_string());
        assert_eq!(host_for("http://[::1]:8080/a"), "[::1]:8080".to_string());
        assert_eq!(host_for(":::"), String::new());
    }

    #[test]
    fn second_page_labels_first_page_chrome() {
        let store = PageStore::open_in_memory().expect("in-memory page store");
        let host = "docs.example.com";
        let a = "https://docs.example.com/a";
        let b = "https://docs.example.com/b";
        let nav = "# Docs Nav";
        let doc_a = format!("{nav}\n\nArticle A\n");
        let doc_b = format!("{nav}\n\nArticle B\n");

        // First page: provisional, everything admitted as content.
        let out_a =
            extract_and_store(a, "lbl-a", &doc_a, FetchRoute::Html, &store).expect("extract a");
        let provisional = match out_a {
            ExtractOutcome::Index { provisional, .. } => provisional,
            ExtractOutcome::Refuse { .. } => panic!("first page must not refuse"),
        };
        assert!(provisional);
        assert_eq!(store.host_page_count(host, "").expect("count"), 1);

        // Second page: resolves the first; nav is template in the new index.
        let out_b =
            extract_and_store(b, "lbl-b", &doc_b, FetchRoute::Html, &store).expect("extract b");
        match out_b {
            ExtractOutcome::Index {
                index_text,
                relabelled,
                provisional,
                ..
            } => {
                assert!(!provisional);
                assert!(
                    !index_text.contains("Docs Nav"),
                    "shared nav must leave the index: {index_text:?}"
                );
                assert!(index_text.contains("Article B"));
                assert_eq!(relabelled.len(), 1, "cold-start page must be re-run");
                assert_eq!(relabelled[0].source_label, "lbl-a");
                assert!(relabelled[0].index_text.contains("Article A"));
            }
            ExtractOutcome::Refuse { .. } => panic!("second page must not refuse"),
        }

        // The first page's stored bytes are untouched (lossless side kept).
        let key_a = page_key_for(a);
        let stored: Vec<StoredBlock> = store.blocks_of(&key_a).expect("blocks of a");
        assert_eq!(stored.len(), 2);
    }

    #[test]
    fn shell_page_refuses_but_stores() {
        let store = PageStore::open_in_memory().expect("in-memory page store");
        let host = "spa.example.com";
        let a = format!("https://{host}/a");
        let b = format!("https://{host}/b");
        // Two pages with identical converted text: the second carries no
        // page-specific content.
        extract_and_store(&a, "lbl-a", "# Shell\n\nSame\n", FetchRoute::Html, &store)
            .expect("extract a");
        let out = extract_and_store(&b, "lbl-b", "# Shell\n\nSame\n", FetchRoute::Html, &store)
            .expect("extract b");
        match out {
            ExtractOutcome::Refuse { reason, .. } => {
                assert!(reason.contains(host), "refusal names the host");
            }
            ExtractOutcome::Index { .. } => panic!("identical page must refuse"),
        }
        // Stored anyway.
        assert_eq!(store.blocks_of(&page_key_for(&b)).expect("blocks").len(), 2);
    }

    #[test]
    fn authored_markdown_never_provisional() {
        let store = PageStore::open_in_memory().expect("in-memory page store");
        let out = extract_and_store(
            "https://md.example.com/a",
            "lbl",
            "# T\n\nBody\n",
            FetchRoute::Markdown,
            &store,
        )
        .expect("extract");
        match out {
            ExtractOutcome::Index {
                provisional,
                index_text,
                ..
            } => {
                assert!(!provisional);
                assert!(index_text.contains("Body"));
            }
            ExtractOutcome::Refuse { .. } => panic!("authored page must not refuse"),
        }
    }

    #[test]
    fn stored_side_stays_retrievable() {
        let store = PageStore::open_in_memory().expect("in-memory page store");
        let url_a = "https://docs.example.com/a";
        let url_b = "https://docs.example.com/b";
        let doc_a = "# Docs Nav\n\nArticle A\n";
        extract_and_store(url_a, "lbl-a", doc_a, FetchRoute::Html, &store).expect("extract a");
        extract_and_store(
            url_b,
            "lbl-b",
            "# Docs Nav\n\nArticle B\n",
            FetchRoute::Html,
            &store,
        )
        .expect("extract b");

        // Whole document retrievable verbatim.
        assert_eq!(
            store
                .full_text_of(&page_key_for(url_a))
                .expect("full text query"),
            Some(doc_a.to_string())
        );
        assert_eq!(
            store
                .full_text_of(&page_key_for("https://docs.example.com/never-fetched"))
                .expect("missing page query"),
            None
        );
        // Template stream retrievable after the second page labelled the nav.
        assert_eq!(
            stored_template_text(&store, url_a).expect("template text"),
            "# Docs Nav".to_string()
        );
    }
}
