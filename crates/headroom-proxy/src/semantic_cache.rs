//! Semantic cache for the Headroom proxy.
//!
//! Simple semantic cache based on message content hash with LRU eviction.
//! Uses `tokio::sync::RwLock` for async-safe access.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Recursively drop `cache_control` annotations before hashing.
fn strip_cache_control(obj: &Value) -> Value {
    match obj {
        Value::Object(map) => {
            let filtered: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, v)| (k.clone(), strip_cache_control(v)))
                .collect();
            Value::Object(filtered)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(strip_cache_control).collect()),
        _ => obj.clone(),
    }
}

/// A cached response entry.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub response_body: Vec<u8>,
    pub response_headers: HashMap<String, String>,
    pub created_at: Instant,
    pub ttl: Duration,
    pub hit_count: u64,
}

/// Simple semantic cache based on message content hash.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
pub struct SemanticCache {
    max_entries: usize,
    ttl: Duration,
    entries: std::sync::RwLock<LruCache>,
}

struct LruCache {
    order: Vec<String>, // insertion order for LRU eviction
    map: HashMap<String, CacheEntry>,
}

impl LruCache {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            map: HashMap::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<&CacheEntry> {
        if self.map.contains_key(key) {
            // Move to end for LRU
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
            self.order.push(key.to_string());
            self.map.get(key)
        } else {
            None
        }
    }

    fn insert(&mut self, key: String, entry: CacheEntry, max_entries: usize) {
        // Remove existing if present
        if self.map.contains_key(&key) {
            if let Some(pos) = self.order.iter().position(|k| *k == key) {
                self.order.remove(pos);
            }
        }

        // Evict oldest if at capacity
        while self.map.len() >= max_entries {
            if let Some(oldest) = self.order.first().cloned() {
                self.order.remove(0);
                self.map.remove(&oldest);
            } else {
                break;
            }
        }

        self.order.push(key.clone());
        self.map.insert(key, entry);
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn total_hits(&self) -> u64 {
        self.map.values().map(|e| e.hit_count).sum()
    }
}

impl SemanticCache {
    pub fn new(max_entries: usize, ttl_seconds: u64) -> Self {
        Self {
            max_entries,
            ttl: Duration::from_secs(ttl_seconds),
            entries: std::sync::RwLock::new(LruCache::new()),
        }
    }

    /// Compute cache key from messages, model, and extra fields.
    fn compute_key(
        messages: &[Value],
        model: &str,
        extra: &serde_json::Map<String, Value>,
    ) -> String {
        let mut key_parts = serde_json::Map::new();
        key_parts.insert("model".to_string(), serde_json::json!(model));
        key_parts.insert("messages".to_string(), Value::Array(messages.to_vec()));
        for (k, v) in extra {
            key_parts.insert(k.clone(), strip_cache_control(v));
        }

        let normalized = serde_json::to_string(&key_parts).unwrap_or_default();
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        normalized.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// Get cached response if exists and not expired.
    pub fn get(
        &self,
        messages: &[Value],
        model: &str,
        extra: &serde_json::Map<String, Value>,
    ) -> Option<CacheEntry> {
        let key = Self::compute_key(messages, model, extra);
        let mut cache = self.entries.write().unwrap();

        let entry = match cache.get(&key) {
            Some(e) => e.clone(),
            None => return None,
        };

        // Check expiration
        if entry.created_at.elapsed() > entry.ttl {
            cache.map.remove(&key);
            if let Some(pos) = cache.order.iter().position(|k| *k == key) {
                cache.order.remove(pos);
            }
            return None;
        }

        // Increment hit count
        if let Some(e) = cache.map.get_mut(&key) {
            e.hit_count += 1;
        }

        Some(entry)
    }

    /// Cache a response.
    pub fn set(
        &self,
        messages: &[Value],
        model: &str,
        response_body: Vec<u8>,
        response_headers: HashMap<String, String>,
        tokens_saved: u64,
        extra: &serde_json::Map<String, Value>,
    ) {
        let key = Self::compute_key(messages, model, extra);
        let entry = CacheEntry {
            response_body,
            response_headers,
            created_at: Instant::now(),
            ttl: self.ttl,
            hit_count: 0,
        };
        let mut cache = self.entries.write().unwrap();
        cache.insert(key, entry, self.max_entries);
    }

    /// Clear all cache entries.
    pub fn clear(&self) {
        let mut cache = self.entries.write().unwrap();
        cache.order.clear();
        cache.map.clear();
    }

    /// Get cache statistics.
    pub fn stats(&self) -> HashMap<String, Value> {
        let cache = self.entries.read().unwrap();
        let mut stats = HashMap::new();
        stats.insert("entries".to_string(), serde_json::json!(cache.len()));
        stats.insert(
            "max_entries".to_string(),
            serde_json::json!(self.max_entries),
        );
        stats.insert(
            "total_hits".to_string(),
            serde_json::json!(cache.total_hits()),
        );
        stats.insert(
            "ttl_seconds".to_string(),
            serde_json::json!(self.ttl.as_secs()),
        );
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Value> {
        vec![serde_json::json!({"role": "user", "content": "hello"})]
    }

    #[test]
    fn strip_cache_control_removes_key() {
        let val = serde_json::json!({"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}});
        let stripped = strip_cache_control(&val);
        assert!(stripped.get("cache_control").is_none());
        assert_eq!(stripped["text"], "hi");
    }

    #[test]
    fn strip_cache_control_preserves_other_keys() {
        let val = serde_json::json!({"type": "text", "text": "hi"});
        let stripped = strip_cache_control(&val);
        assert_eq!(stripped, val);
    }

    #[test]
    fn compute_key_same_for_same_input() {
        let extra = serde_json::Map::new();
        let k1 = SemanticCache::compute_key(&msgs(), "claude-3", &extra);
        let k2 = SemanticCache::compute_key(&msgs(), "claude-3", &extra);
        assert_eq!(k1, k2);
    }

    #[test]
    fn compute_key_differs_for_different_model() {
        let extra = serde_json::Map::new();
        let k1 = SemanticCache::compute_key(&msgs(), "claude-3", &extra);
        let k2 = SemanticCache::compute_key(&msgs(), "claude-4", &extra);
        assert_ne!(k1, k2);
    }

    #[test]
    fn get_miss_on_empty_cache() {
        let cache = SemanticCache::new(100, 3600);
        let extra = serde_json::Map::new();
        assert!(cache.get(&msgs(), "model", &extra).is_none());
    }

    #[test]
    fn set_and_get_hit() {
        let cache = SemanticCache::new(100, 3600);
        let extra = serde_json::Map::new();
        cache.set(
            &msgs(),
            "model",
            b"response".to_vec(),
            HashMap::new(),
            10,
            &extra,
        );
        let entry = cache.get(&msgs(), "model", &extra);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().response_body, b"response");
    }

    #[test]
    fn lru_eviction() {
        let cache = SemanticCache::new(2, 3600);
        let extra = serde_json::Map::new();
        let m1 = vec![serde_json::json!({"role": "user", "content": "a"})];
        let m2 = vec![serde_json::json!({"role": "user", "content": "b"})];
        let m3 = vec![serde_json::json!({"role": "user", "content": "c"})];

        cache.set(&m1, "m", b"1".to_vec(), HashMap::new(), 0, &extra);
        cache.set(&m2, "m", b"2".to_vec(), HashMap::new(), 0, &extra);
        cache.set(&m3, "m", b"3".to_vec(), HashMap::new(), 0, &extra);

        // m1 should be evicted
        assert!(cache.get(&m1, "m", &extra).is_none());
        assert!(cache.get(&m2, "m", &extra).is_some());
        assert!(cache.get(&m3, "m", &extra).is_some());
    }

    #[test]
    fn stats_returns_correct_counts() {
        let cache = SemanticCache::new(100, 3600);
        let extra = serde_json::Map::new();
        cache.set(&msgs(), "m", b"data".to_vec(), HashMap::new(), 5, &extra);
        let stats = cache.stats();
        assert_eq!(stats["entries"], serde_json::json!(1));
        assert_eq!(stats["max_entries"], serde_json::json!(100));
    }

    #[test]
    fn clear_empties_cache() {
        let cache = SemanticCache::new(100, 3600);
        let extra = serde_json::Map::new();
        cache.set(&msgs(), "m", b"data".to_vec(), HashMap::new(), 0, &extra);
        cache.clear();
        assert!(cache.get(&msgs(), "m", &extra).is_none());
        assert_eq!(cache.stats()["entries"], serde_json::json!(0));
    }
}
