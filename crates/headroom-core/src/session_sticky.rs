//! Generic bounded-LRU session-sticky tracker (Rust port of the three
//! near-identical OrderedDict LRUs in `headroom/proxy/helpers.py`:
//! `SessionBetaTracker` (~L1776), `SessionToolTracker` (~L2037), and
//! `SessionCcrTracker`).
//!
//! Each Python tracker keyed session state by `(provider, session_id)` in a
//! `threading.Lock`-guarded `OrderedDict` bounded to `max_sessions` (default
//! 1000), evicting the oldest entry (`popitem(last=False)`) on overflow and
//! calling `move_to_end` on access to keep the LRU order. This one generic
//! `SessionStickyTracker<T>` collapses all three: callers compose the string
//! key (e.g. `format!("{provider}:{session_id}")`) and choose the value type.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

/// Default per-tracker session bound, matching Python's
/// `_*_TRACKER_MAX_SESSIONS_DEFAULT = 1000`.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Thread-safe bounded LRU over `String` keys. Cloning a value out on `get`
/// (rather than handing back a guard) keeps the lock scope tiny, matching the
/// Python trackers which copy state out under the lock.
pub struct SessionStickyTracker<T: Clone> {
    inner: Mutex<LruCache<String, T>>,
}

impl<T: Clone> SessionStickyTracker<T> {
    /// Build a tracker bounded to `capacity` sessions. A `capacity` of 0 is
    /// treated as 1 (an LRU must hold at least one entry).
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Number of live sessions (LRU touch-free), mirroring `active_sessions`.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clone the value for `key`, touching it as most-recently-used
    /// (`OrderedDict.move_to_end` on hit).
    pub fn get(&self, key: &str) -> Option<T> {
        self.inner.lock().unwrap().get(key).cloned()
    }

    /// Insert/overwrite `key`, touching it as most-recently-used and evicting
    /// the oldest entry past capacity.
    pub fn insert(&self, key: String, value: T) {
        self.inner.lock().unwrap().put(key, value);
    }

    /// First-write-wins: return the existing value for `key`, or insert the
    /// value produced by `f` and return a clone of it. Ports the tool-tracker
    /// semantics where the first golden definition seen for a session sticks.
    pub fn get_or_insert_with<F: FnOnce() -> T>(&self, key: &str, f: F) -> T {
        let mut guard = self.inner.lock().unwrap();
        if let Some(v) = guard.get(key) {
            return v.clone();
        }
        let v = f();
        guard.put(key.to_string(), v.clone());
        v
    }

    /// Read-modify-write under one lock. `f` receives the current value (`None`
    /// for a fresh key) and returns the value to store; the stored clone is
    /// returned. Ports the beta-tracker's union-and-persist merge step.
    pub fn update<F: FnOnce(Option<T>) -> T>(&self, key: &str, f: F) -> T {
        let mut guard = self.inner.lock().unwrap();
        let current = guard.get(key).cloned();
        let next = f(current);
        guard.put(key.to_string(), next.clone());
        next
    }

    /// Clear all session state (test/reset helper, matching Python `reset`).
    pub fn reset(&self) {
        self.inner.lock().unwrap().clear();
    }
}

impl<T: Clone> Default for SessionStickyTracker<T> {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_missing_is_none() {
        let t: SessionStickyTracker<u32> = SessionStickyTracker::new(4);
        assert!(t.get("a").is_none());
        assert!(t.is_empty());
    }

    #[test]
    fn insert_and_get() {
        let t = SessionStickyTracker::new(4);
        t.insert("a".into(), 1);
        assert_eq!(t.get("a"), Some(1));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn eviction_at_capacity() {
        let t = SessionStickyTracker::new(2);
        t.insert("a".into(), 1);
        t.insert("b".into(), 2);
        t.insert("c".into(), 3); // evicts "a" (oldest)
        assert_eq!(t.len(), 2);
        assert!(t.get("a").is_none());
        assert_eq!(t.get("b"), Some(2));
        assert_eq!(t.get("c"), Some(3));
    }

    #[test]
    fn get_touches_lru_order() {
        let t = SessionStickyTracker::new(2);
        t.insert("a".into(), 1);
        t.insert("b".into(), 2);
        // Touch "a" so "b" becomes the eviction victim.
        assert_eq!(t.get("a"), Some(1));
        t.insert("c".into(), 3);
        assert_eq!(t.get("a"), Some(1));
        assert!(t.get("b").is_none());
    }

    #[test]
    fn get_or_insert_first_write_wins() {
        let t = SessionStickyTracker::new(4);
        assert_eq!(t.get_or_insert_with("k", || 10), 10);
        // Second call must NOT overwrite.
        assert_eq!(t.get_or_insert_with("k", || 99), 10);
        assert_eq!(t.get("k"), Some(10));
    }

    #[test]
    fn update_merges_existing() {
        let t: SessionStickyTracker<Vec<String>> = SessionStickyTracker::new(4);
        let merged = t.update("s", |cur| {
            let mut v = cur.unwrap_or_default();
            v.push("beta-a".into());
            v
        });
        assert_eq!(merged, vec!["beta-a".to_string()]);
        let merged = t.update("s", |cur| {
            let mut v = cur.unwrap_or_default();
            v.push("beta-b".into());
            v
        });
        assert_eq!(merged, vec!["beta-a".to_string(), "beta-b".to_string()]);
    }

    #[test]
    fn zero_capacity_holds_one() {
        let t = SessionStickyTracker::new(0);
        t.insert("a".into(), 1);
        assert_eq!(t.get("a"), Some(1));
    }

    #[test]
    fn reset_clears() {
        let t = SessionStickyTracker::new(4);
        t.insert("a".into(), 1);
        t.reset();
        assert!(t.is_empty());
    }
}
