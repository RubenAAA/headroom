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
}
