//! Per-request memoization over any [`Tokenizer`].
//!
//! The pipeline counts the same spans several times per request (live-zone
//! gates count a block, then message counting counts it again). Counts are
//! deterministic per (tokenizer, text), so caching them changes no values —
//! it only skips re-entering the encoder. The wrapper borrows the real
//! counter and dies with the request, so no state leaks across requests.
//!
//! Keys are the exact texts (lookups borrow, only inserts clone), so unlike
//! a hash key there is no collision that could return a wrong count.
//! Eviction is true LRU: hot spans survive, cold ones fall off.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

use super::{Backend, Tokenizer};

/// Cap per request; the working set is one request's spans, not a stream.
const MAX_ENTRIES: usize = 512;

/// Memoizes `count_text` for a single request. Wrap the request's tokenizer
/// at the entry point and hand `&wrapper` everywhere `&dyn Tokenizer` goes;
/// nested counting (e.g. `count_messages`) funnels through the same cache.
#[derive(Debug)]
pub struct MemoizingTokenizer<'a> {
    inner: &'a dyn Tokenizer,
    cache: Mutex<LruCache<String, usize>>,
}

impl<'a> MemoizingTokenizer<'a> {
    /// Borrow `inner` for one request's counting.
    pub fn new(inner: &'a dyn Tokenizer) -> Self {
        Self {
            inner,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_ENTRIES).expect("request memo capacity is nonzero"),
            )),
        }
    }

    /// Entries currently cached. Test-only, so tests can observe hits.
    #[cfg(test)]
    fn cached(&self) -> usize {
        self.cache.lock().expect("memo cache poisoned").len()
    }
}

impl Tokenizer for MemoizingTokenizer<'_> {
    fn count_text(&self, text: &str) -> usize {
        let mut cache = self.cache.lock().expect("memo cache poisoned");
        if let Some(&hit) = cache.get(text) {
            return hit;
        }
        let counted = self.inner.count_text(text);
        cache.push(text.to_owned(), counted);
        counted
    }

    fn backend(&self) -> Backend {
        self.inner.backend()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::EstimatingCounter;

    #[test]
    fn repeat_counts_hit_cache() {
        let inner = EstimatingCounter::default();
        let memo = MemoizingTokenizer::new(&inner);
        let text = "the quick brown fox jumps over the lazy dog ".repeat(20);
        let first = memo.count_text(&text);
        assert_eq!(first, inner.count_text(&text));
        assert_eq!(memo.cached(), 1);
        assert_eq!(memo.count_text(&text), first);
        assert_eq!(memo.cached(), 1);
        let other = "entirely different content here ".repeat(20);
        assert_eq!(memo.count_text(&other), inner.count_text(&other));
        assert_eq!(memo.cached(), 2);
    }

    #[test]
    fn delegates_backend() {
        let inner = EstimatingCounter::default();
        let memo = MemoizingTokenizer::new(&inner);
        assert_eq!(memo.backend(), inner.backend());
    }

    #[test]
    fn eviction_recounts_correctly() {
        let inner = EstimatingCounter::default();
        let memo = MemoizingTokenizer::new(&inner);
        let first = "eviction probe alpha ".repeat(4);
        assert_eq!(memo.count_text(&first), inner.count_text(&first));
        for i in 0..MAX_ENTRIES {
            let text = format!("filler span {i} ");
            assert_eq!(memo.count_text(&text), inner.count_text(&text));
        }
        // Bound holds and the long-evicted first span recomputes exactly.
        assert_eq!(memo.cached(), MAX_ENTRIES);
        assert_eq!(memo.count_text(&first), inner.count_text(&first));
    }
}
