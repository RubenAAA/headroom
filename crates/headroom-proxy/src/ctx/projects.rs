//! CTX-2b — per-project store registry.
//!
//! The ctx storage layer isolates projects by **file path**: one sessions DB
//! and one content DB per project, named by `hash_project_dir_canonical`
//! (`headroom_core::ctx`). There is no project column inside either DB, and
//! [`headroom_core::ctx::SearchOpts`] has no project field, so opening the
//! right file is the *only* thing that keeps one project's context out of
//! another's.
//!
//! Every proxy call site used to pass `project_dir = ""`, which collapsed all
//! of them onto one shared file. Recall then ran an unscoped BM25 query across
//! it and could inject one project's offloaded tool output into a session for
//! an unrelated project — observed live.
//!
//! The stores cannot be opened once at boot, because the project is a property
//! of a request and the proxy serves many. They are opened on first sight of a
//! project and cached here.
//!
//! # Bounded
//!
//! Handles are held in an LRU so a long-lived proxy that has seen hundreds of
//! projects does not hold hundreds of open sqlite connections. Eviction closes
//! the handle; the next request for that project reopens it. Nothing is lost —
//! the durable state is the file.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use headroom_core::ctx::{content_db_path, session_db_path, CtxStore, SessionsStore};
use lru::LruCache;

/// Open sqlite handles kept per store kind. Sixteen matches the memory
/// router's `max_open_backends` — enough that switching between the handful of
/// projects anyone works on in a session never reopens, small enough to bound
/// the file descriptors.
const MAX_OPEN_PER_KIND: usize = 16;

/// Wall-clock ceiling for one cold-tier sweep.
///
/// The sweep runs only after the CCR store has already missed, and the
/// alternative to spending this is a wasted continuation round plus the model
/// re-reading the source, which costs far more. It still needs a ceiling: the
/// number of project DBs grows without bound, and a retrieval must not stall a
/// turn while every one of them is opened. Exhausting the budget is logged and
/// degrades to the old behaviour, a plain miss.
const COLD_TIER_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// Outcome of one cold-tier sweep, including what it cost.
///
/// The cost travels with the result so the caller can put it in the same event
/// as the hit or miss. A fallback nobody can see the latency of is one nobody
/// can tell has gone slow.
pub struct ColdTierLookup {
    /// Project key and content, when a store held the hash.
    pub found: Option<(String, String)>,
    pub elapsed: std::time::Duration,
    /// Project DBs actually opened and queried.
    pub scanned: usize,
    /// Whether the budget ran out before every project was swept.
    pub gave_up: bool,
}

impl ColdTierLookup {
    fn miss(started: std::time::Instant, scanned: usize, gave_up: bool) -> Self {
        Self {
            found: None,
            elapsed: started.elapsed(),
            scanned,
            gave_up,
        }
    }
}

/// The bucket used when a request names no project.
///
/// Empty string, which is what every call site passed unconditionally before
/// sharding worked — so requests the resolver cannot place still land where
/// their history already is, rather than in a new and empty bucket.
pub const UNRESOLVED_PROJECT: &str = "";

/// Lazily-opened, per-project ctx stores. Shared behind an `Arc` by the
/// capture observer, the offload sink, and the injection engine, so all three
/// read and write the same file for a given project.
pub struct ProjectStores {
    base: PathBuf,
    sessions: Mutex<LruCache<String, Arc<SessionsStore>>>,
    content: Mutex<LruCache<String, Arc<CtxStore>>>,
}

impl ProjectStores {
    pub fn new(base: PathBuf) -> Self {
        let cap = NonZeroUsize::new(MAX_OPEN_PER_KIND).expect("nonzero");
        Self {
            base,
            sessions: Mutex::new(LruCache::new(cap)),
            content: Mutex::new(LruCache::new(cap)),
        }
    }

    /// The sessions DB for `project_dir`, opening it if this is the first
    /// request for that project. `None` when the file cannot be opened —
    /// logged once per attempt; capture and recall both treat it as "no
    /// history", which is the safe direction.
    pub fn sessions(&self, project_dir: &str) -> Option<Arc<SessionsStore>> {
        let path = session_db_path(&self.base, project_dir);
        self.get_or_open(&self.sessions, project_dir, &path, |p| {
            SessionsStore::open(p)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        })
    }

    /// The FTS content DB for `project_dir`, opened on first sight.
    pub fn content(&self, project_dir: &str) -> Option<Arc<CtxStore>> {
        let path = content_db_path(&self.base, project_dir);
        self.get_or_open(&self.content, project_dir, &path, |p| {
            CtxStore::open(p).map(Arc::new).map_err(|e| e.to_string())
        })
    }

    /// Search every project's content DB for a block by its `content_hash`,
    /// skipping `already_checked`.
    ///
    /// The CCR store is one global file and is not sharded by project, so this
    /// is not a fix for cross-project isolation — it is the cold tier. `ccr.db`
    /// drops a block after a week idle; the content index keeps it. A hash the
    /// model quotes from an older stretch of its own transcript therefore
    /// misses in `ccr.db` and is still on disk here, under whichever project
    /// was current when the block was offloaded. That project is often not the
    /// one making the request, because subagents, teammates and held working
    /// directories all move the resolved project between turns.
    ///
    /// Returns the project key of the DB that answered, which is the
    /// `hash_project_dir_canonical` stem of the file. The original path is not
    /// recoverable from it — the hash is one-way — so it identifies the store
    /// for correlation, not for display.
    ///
    /// Handles are opened one-shot and dropped rather than kept, so a sweep of
    /// every project on disk cannot evict the working set from the LRU. The
    /// sweep runs only after a miss, which is rare.
    pub fn find_content_any_project(
        &self,
        content_hash: &str,
        already_checked: &str,
    ) -> ColdTierLookup {
        let started = std::time::Instant::now();
        let mut scanned = 0usize;
        let skip = headroom_core::ctx::hash_project_dir_canonical(already_checked);
        let Some(dir) = content_db_path(&self.base, already_checked)
            .parent()
            .map(Path::to_path_buf)
        else {
            return ColdTierLookup::miss(started, scanned, false);
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    event = "ctx_content_scan_failed",
                    path = %dir.display(),
                    error = %e,
                );
                return ColdTierLookup::miss(started, scanned, false);
            }
        };
        for entry in entries.flatten() {
            // Budget checked per file rather than per row: one file is the
            // smallest unit of work here, and an indexed point lookup on a
            // few hundred `sources` rows is far quicker than the check.
            if started.elapsed() >= COLD_TIER_BUDGET {
                tracing::warn!(
                    event = "ctx_cold_tier_budget_exhausted",
                    hash = %content_hash,
                    scanned = scanned,
                    budget_ms = COLD_TIER_BUDGET.as_millis() as u64,
                    "cold-tier lookup gave up before sweeping every project"
                );
                return ColdTierLookup::miss(started, scanned, true);
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }
            let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if key == skip {
                continue;
            }
            // Read-only: the sweep must not create a DB for a project that has
            // none, nor take a write lock on one another proxy is writing.
            let Ok(store) = CtxStore::open_read_only(&path) else {
                continue;
            };
            scanned += 1;
            match store.content_by_hash(content_hash) {
                Ok(Some(content)) => {
                    return ColdTierLookup {
                        found: Some((key.to_string(), content)),
                        elapsed: started.elapsed(),
                        scanned,
                        gave_up: false,
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    event = "ctx_content_hash_lookup_failed",
                    path = %path.display(),
                    error = %e,
                ),
            }
        }
        ColdTierLookup::miss(started, scanned, false)
    }

    fn get_or_open<T>(
        &self,
        cache: &Mutex<LruCache<String, Arc<T>>>,
        project_dir: &str,
        path: &Path,
        open: impl Fn(&Path) -> Result<Arc<T>, String>,
    ) -> Option<Arc<T>> {
        let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(store) = guard.get(project_dir) {
            return Some(Arc::clone(store));
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    event = "ctx_project_store_dir_failed",
                    path = %parent.display(),
                    error = %e,
                );
                return None;
            }
        }
        match open(path) {
            Ok(store) => {
                guard.put(project_dir.to_string(), Arc::clone(&store));
                tracing::debug!(
                    event = "ctx_project_store_opened",
                    project_dir = %project_dir,
                    path = %path.display(),
                );
                Some(store)
            }
            Err(e) => {
                tracing::warn!(
                    event = "ctx_project_store_open_failed",
                    project_dir = %project_dir,
                    path = %path.display(),
                    error = %e,
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::ctx::{IndexOpts, SearchOpts};
    use tempfile::TempDir;

    /// The whole point: content indexed under one project must not be findable
    /// from another. This is the live-observed leak, pinned.
    #[test]
    fn a_projects_content_is_invisible_to_another_project() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());

        let a = stores.content("/home/dev/alpha").expect("open alpha");
        a.index_content(
            "notes",
            "the alpha deploy key rotates on fridays",
            &IndexOpts::default(),
        )
        .unwrap();

        let opts = SearchOpts {
            limit: 5,
            ..Default::default()
        };
        assert!(
            !a.search(&["deploy key".to_string()], &opts)
                .unwrap()
                .is_empty(),
            "the owning project must find its own content"
        );

        let b = stores.content("/home/dev/beta").expect("open beta");
        assert!(
            b.search(&["deploy key".to_string()], &opts)
                .unwrap()
                .is_empty(),
            "another project must not see it"
        );
    }

    #[test]
    fn the_same_project_gets_the_same_handle() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        let a = stores.content("/home/dev/alpha").unwrap();
        let b = stores.content("/home/dev/alpha").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "one handle per project, not per call");
    }

    /// Eviction must not lose data — the file is the durable state, the handle
    /// is only a cache.
    #[test]
    fn content_survives_eviction_of_its_handle() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        stores
            .content("/home/dev/alpha")
            .unwrap()
            .index_content("notes", "alpha remembers this", &IndexOpts::default())
            .unwrap();

        // Push the alpha handle out of a 16-slot LRU.
        for i in 0..MAX_OPEN_PER_KIND + 1 {
            stores.content(&format!("/home/dev/filler{i}")).unwrap();
        }

        let opts = SearchOpts {
            limit: 5,
            ..Default::default()
        };
        let reopened = stores.content("/home/dev/alpha").unwrap();
        assert!(!reopened
            .search(&["remembers".to_string()], &opts)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sessions_shard_by_project_too() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        let a = stores.sessions("/home/dev/alpha").unwrap();
        let b = stores.sessions("/home/dev/beta").unwrap();
        a.record_prefix("conv-1", 2, "hash-1").unwrap();
        assert!(a.last_prefix("conv-1").unwrap().is_some());
        assert!(
            b.last_prefix("conv-1").unwrap().is_none(),
            "a conversation must not surface in another project's sessions DB"
        );
    }

    /// The bug this fixes: a block offloaded while one project was current,
    /// asked for back while another is. The CCR store answers most of those on
    /// its own, being global; this covers the case where it has expired the
    /// block and only the per-project index still holds it.
    #[test]
    fn find_content_any_project_reaches_across_projects() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        stores
            .content("/home/dev/alpha")
            .unwrap()
            .index_content(
                "cargo test output",
                "alpha stored this block",
                &IndexOpts {
                    content_hash: Some("0123456789abcdef01234567".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        // Beta exists and does not hold the hash, so the sweep has to keep
        // looking rather than stop at the first store it opens.
        stores
            .content("/home/dev/beta")
            .unwrap()
            .index_content(
                "unrelated",
                "beta stored something else",
                &IndexOpts::default(),
            )
            .unwrap();

        assert!(
            stores
                .content("/home/dev/beta")
                .unwrap()
                .content_by_hash("0123456789abcdef01234567")
                .unwrap()
                .is_none(),
            "the requesting project's own store must miss, or the test proves nothing"
        );

        let hit = stores.find_content_any_project("0123456789abcdef01234567", "/home/dev/beta");
        assert!(!hit.gave_up, "a two-project sweep must fit the budget");
        assert_eq!(hit.scanned, 1, "beta is skipped, so only alpha is opened");
        let (project, content) = hit.found.expect("the block is on disk under alpha");
        assert_eq!(content, "alpha stored this block");
        assert_eq!(
            project,
            headroom_core::ctx::hash_project_dir_canonical("/home/dev/alpha")
        );
    }

    #[test]
    fn find_content_any_project_skips_the_store_already_checked() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        stores
            .content("/home/dev/alpha")
            .unwrap()
            .index_content(
                "only copy",
                "alpha stored this block",
                &IndexOpts {
                    content_hash: Some("0123456789abcdef01234567".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert!(
            stores
                .find_content_any_project("0123456789abcdef01234567", "/home/dev/alpha")
                .found
                .is_none(),
            "alpha was already checked by the caller; sweeping it again would \
             report a cross-project hit for a plain local one"
        );
    }

    #[test]
    fn find_content_any_project_misses_when_no_store_holds_the_hash() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        stores
            .content("/home/dev/alpha")
            .unwrap()
            .index_content("notes", "alpha stored this", &IndexOpts::default())
            .unwrap();
        assert!(stores
            .find_content_any_project("ffffffffffffffffffffffff", "/home/dev/beta")
            .found
            .is_none());
    }

    /// The sweep is bounded and reports what it cost, so a fallback that goes
    /// slow is visible in the same event as the hit.
    #[test]
    fn find_content_any_project_reports_its_cost_and_stays_in_budget() {
        let dir = TempDir::new().unwrap();
        let stores = ProjectStores::new(dir.path().to_path_buf());
        for i in 0..20 {
            stores
                .content(&format!("/home/dev/p{i}"))
                .unwrap()
                .index_content("notes", "nothing to find here", &IndexOpts::default())
                .unwrap();
        }
        let miss = stores.find_content_any_project("ffffffffffffffffffffffff", "/home/dev/p0");
        assert!(miss.found.is_none());
        assert_eq!(
            miss.scanned, 19,
            "every project but the one already checked"
        );
        assert!(
            !miss.gave_up && miss.elapsed < COLD_TIER_BUDGET,
            "20 small stores must fit the budget, took {:?}",
            miss.elapsed
        );
    }
}
