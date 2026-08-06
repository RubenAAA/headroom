//! Base abstractions for pluggable quota / rate-limit trackers (port of
//! `headroom/subscription/base.py`).
//!
//! Only the surface the Anthropic tracker needs is ported. The Python
//! `start`/`stop` async lifecycle is intentionally omitted from the trait: in
//! the Rust split the background poll loop is wired in `headroom-proxy`, so the
//! core trait carries only identity + availability + stats. The Codex/Copilot
//! trackers are out of scope, so the registry only ever holds the Anthropic
//! tracker in practice.

use serde_json::Value;
use std::sync::Mutex;

/// A single AI-tool quota / rate-limit tracker.
pub trait QuotaTracker: Send + Sync {
    /// Stats key used in `/stats`. Must be unique across registered trackers.
    fn key(&self) -> &str;

    /// Human-readable name for log messages.
    fn label(&self) -> &str;

    /// Whether this tracker should be activated. Default: always.
    fn is_available(&self) -> bool {
        true
    }

    /// Current snapshot as a serialisable value, or `None` for "no data yet"
    /// (which omits the key from `/stats` rather than emitting `null`).
    fn get_stats(&self) -> Option<Value>;
}

/// Process-wide registry of [`QuotaTracker`] instances.
#[derive(Default)]
pub struct QuotaTrackerRegistry {
    trackers: Mutex<Vec<Box<dyn QuotaTracker>>>,
}

impl QuotaTrackerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tracker. Duplicate keys are rejected.
    pub fn register(&self, tracker: Box<dyn QuotaTracker>) -> Result<(), String> {
        let mut trackers = self.trackers.lock().unwrap();
        if trackers.iter().any(|t| t.key() == tracker.key()) {
            return Err(format!(
                "A tracker with key '{}' is already registered. Each tracker must have a unique key.",
                tracker.key()
            ));
        }
        trackers.push(tracker);
        Ok(())
    }

    /// `{key: stats}` for every available tracker that returns data.
    pub fn get_all_stats(&self) -> serde_json::Map<String, Value> {
        let trackers = self.trackers.lock().unwrap();
        let mut result = serde_json::Map::new();
        for tracker in trackers.iter() {
            if !tracker.is_available() {
                continue;
            }
            if let Some(stats) = tracker.get_stats() {
                result.insert(tracker.key().to_string(), stats);
            }
        }
        result
    }

    /// Stats for a single tracker by key, or `None`.
    pub fn get_stats(&self, key: &str) -> Option<Value> {
        let trackers = self.trackers.lock().unwrap();
        trackers
            .iter()
            .find(|t| t.key() == key)
            .and_then(|t| t.get_stats())
    }

    pub fn len(&self) -> usize {
        self.trackers.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Dummy {
        key: String,
        available: bool,
        stats: Option<Value>,
    }

    impl QuotaTracker for Dummy {
        fn key(&self) -> &str {
            &self.key
        }
        fn label(&self) -> &str {
            "Dummy"
        }
        fn is_available(&self) -> bool {
            self.available
        }
        fn get_stats(&self) -> Option<Value> {
            self.stats.clone()
        }
    }

    #[test]
    fn duplicate_keys_rejected() {
        let reg = QuotaTrackerRegistry::new();
        reg.register(Box::new(Dummy {
            key: "a".into(),
            available: true,
            stats: Some(json!({"x": 1})),
        }))
        .unwrap();
        let err = reg
            .register(Box::new(Dummy {
                key: "a".into(),
                available: true,
                stats: None,
            }))
            .unwrap_err();
        assert!(err.contains("already registered"));
    }

    #[test]
    fn all_stats_excludes_unavailable_and_none() {
        let reg = QuotaTrackerRegistry::new();
        reg.register(Box::new(Dummy {
            key: "has".into(),
            available: true,
            stats: Some(json!({"x": 1})),
        }))
        .unwrap();
        reg.register(Box::new(Dummy {
            key: "unavail".into(),
            available: false,
            stats: Some(json!({"y": 2})),
        }))
        .unwrap();
        reg.register(Box::new(Dummy {
            key: "nodata".into(),
            available: true,
            stats: None,
        }))
        .unwrap();
        let stats = reg.get_all_stats();
        assert_eq!(stats.len(), 1);
        assert!(stats.contains_key("has"));
    }
}
