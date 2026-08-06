//! Batch context storage for CCR post-processing.
//!
//! When batches are submitted, the request context (messages, tools, model)
//! is stored so that when results are retrieved, CCR tool calls can be
//! handled and continuation API calls made.
//!
//! Mirrors Python's `headroom.ccr.batch_store`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ─── Types ───────────────────────────────────────────────────────────────

/// Context for a single request within a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequestContext {
    pub custom_id: String,
    pub messages: Vec<serde_json::Value>,
    pub tools: Option<Vec<serde_json::Value>>,
    pub model: String,
    pub system_instruction: Option<String>,
    #[serde(default)]
    pub extras: HashMap<String, serde_json::Value>,
}

/// Context for an entire batch submission.
#[derive(Debug, Clone)]
pub struct BatchContext {
    pub batch_id: String,
    pub provider: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    pub requests: HashMap<String, BatchRequestContext>,
    pub api_key: Option<String>,
    pub api_base_url: Option<String>,
}

impl BatchContext {
    pub fn new(batch_id: String, provider: String, ttl: Duration) -> Self {
        let now = Instant::now();
        Self {
            batch_id,
            provider,
            created_at: now,
            expires_at: now + ttl,
            requests: HashMap::new(),
            api_key: None,
            api_base_url: None,
        }
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }

    pub fn add_request(&mut self, request: BatchRequestContext) {
        self.requests.insert(request.custom_id.clone(), request);
    }

    pub fn get_request(&self, custom_id: &str) -> Option<&BatchRequestContext> {
        self.requests.get(custom_id)
    }
}

/// Statistics about the batch context store.
#[derive(Debug, Clone)]
pub struct BatchStoreStats {
    pub total_contexts: usize,
    pub max_contexts: usize,
    pub ttl: Duration,
    pub providers: HashMap<String, usize>,
}

// ─── BatchContextStore ───────────────────────────────────────────────────

/// Thread-safe store for batch contexts with TTL-based expiration.
pub struct BatchContextStore {
    contexts: Mutex<HashMap<String, BatchContext>>,
    ttl: Duration,
    max_contexts: usize,
}

impl BatchContextStore {
    pub fn new(ttl: Duration, max_contexts: usize) -> Self {
        Self {
            contexts: Mutex::new(HashMap::new()),
            ttl,
            max_contexts,
        }
    }

    /// Store a batch context. Sets expiration and enforces memory limit.
    pub fn store(&self, mut context: BatchContext) {
        let mut map = self.contexts.lock().unwrap_or_else(|e| e.into_inner());

        // Enforce memory limit
        if map.len() >= self.max_contexts {
            self.cleanup_oldest_locked(&mut map);
        }

        // Set expiration
        context.expires_at = Instant::now() + self.ttl;
        map.insert(context.batch_id.clone(), context);
    }

    /// Get a batch context by ID. Returns None if not found or expired.
    pub fn get(&self, batch_id: &str) -> Option<BatchContext> {
        let mut map = self.contexts.lock().unwrap_or_else(|e| e.into_inner());

        let context = match map.get(batch_id) {
            Some(ctx) => ctx.clone(),
            None => return None,
        };

        if context.is_expired() {
            map.remove(batch_id);
            return None;
        }

        Some(context)
    }

    /// Remove a batch context. Returns true if it existed.
    pub fn remove(&self, batch_id: &str) -> bool {
        let mut map = self.contexts.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(batch_id).is_some()
    }

    /// Remove all expired entries. Returns the count removed.
    pub fn cleanup_expired(&self) -> usize {
        let mut map = self.contexts.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|_, ctx| !ctx.is_expired());
        before - map.len()
    }

    /// Get store statistics.
    pub fn stats(&self) -> BatchStoreStats {
        let map = self.contexts.lock().unwrap_or_else(|e| e.into_inner());
        let mut providers: HashMap<String, usize> = HashMap::new();
        for ctx in map.values() {
            *providers.entry(ctx.provider.clone()).or_insert(0) += 1;
        }
        BatchStoreStats {
            total_contexts: map.len(),
            max_contexts: self.max_contexts,
            ttl: self.ttl,
            providers,
        }
    }

    fn cleanup_oldest_locked(&self, map: &mut HashMap<String, BatchContext>) {
        if map.is_empty() {
            return;
        }
        // Sort by creation time, remove oldest 10%
        let mut entries: Vec<_> = map.iter().map(|(k, v)| (k.clone(), v.created_at)).collect();
        entries.sort_by_key(|(_, t)| *t);
        let to_remove = std::cmp::max(1, entries.len() / 10);
        for (batch_id, _) in entries.iter().take(to_remove) {
            map.remove(batch_id);
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_request(custom_id: &str) -> BatchRequestContext {
        BatchRequestContext {
            custom_id: custom_id.to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            tools: None,
            model: "claude-sonnet-4-5-20250929".to_string(),
            system_instruction: None,
            extras: HashMap::new(),
        }
    }

    #[test]
    fn store_and_retrieve() {
        let store = BatchContextStore::new(Duration::from_secs(60), 100);
        let mut ctx = BatchContext::new(
            "batch_1".to_string(),
            "anthropic".to_string(),
            Duration::from_secs(60),
        );
        ctx.add_request(make_request("req_1"));
        ctx.api_key = Some("test-key".to_string());

        store.store(ctx);

        let retrieved = store.get("batch_1");
        assert!(retrieved.is_some());
        let r = retrieved.unwrap();
        assert_eq!(r.batch_id, "batch_1");
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.requests.len(), 1);
        assert_eq!(r.api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let store = BatchContextStore::new(Duration::from_secs(60), 100);
        assert!(store.get("missing").is_none());
    }

    #[test]
    fn ttl_expiration() {
        let store = BatchContextStore::new(Duration::from_millis(50), 100);
        let ctx = BatchContext::new(
            "batch_1".to_string(),
            "anthropic".to_string(),
            Duration::from_millis(50),
        );
        store.store(ctx);

        assert!(store.get("batch_1").is_some());

        thread::sleep(Duration::from_millis(100));

        assert!(store.get("batch_1").is_none());
    }

    #[test]
    fn remove() {
        let store = BatchContextStore::new(Duration::from_secs(60), 100);
        let ctx = BatchContext::new(
            "batch_1".to_string(),
            "anthropic".to_string(),
            Duration::from_secs(60),
        );
        store.store(ctx);

        assert!(store.remove("batch_1"));
        assert!(!store.remove("batch_1"));
        assert!(store.get("batch_1").is_none());
    }

    #[test]
    fn cleanup_expired() {
        let store = BatchContextStore::new(Duration::from_millis(50), 100);
        store.store(BatchContext::new(
            "a".to_string(),
            "anthropic".to_string(),
            Duration::from_millis(50),
        ));
        store.store(BatchContext::new(
            "b".to_string(),
            "openai".to_string(),
            Duration::from_millis(50),
        ));

        thread::sleep(Duration::from_millis(100));

        let removed = store.cleanup_expired();
        assert_eq!(removed, 2);
        assert!(store.get("a").is_none());
        assert!(store.get("b").is_none());
    }

    #[test]
    fn max_contexts_eviction() {
        let store = BatchContextStore::new(Duration::from_secs(60), 5);
        for i in 0..10 {
            store.store(BatchContext::new(
                format!("batch_{}", i),
                "anthropic".to_string(),
                Duration::from_secs(60),
            ));
        }
        let stats = store.stats();
        assert!(stats.total_contexts <= 5);
    }

    #[test]
    fn stats_count_by_provider() {
        let store = BatchContextStore::new(Duration::from_secs(60), 100);
        store.store(BatchContext::new(
            "a".to_string(),
            "anthropic".to_string(),
            Duration::from_secs(60),
        ));
        store.store(BatchContext::new(
            "b".to_string(),
            "anthropic".to_string(),
            Duration::from_secs(60),
        ));
        store.store(BatchContext::new(
            "c".to_string(),
            "openai".to_string(),
            Duration::from_secs(60),
        ));

        let stats = store.stats();
        assert_eq!(stats.total_contexts, 3);
        assert_eq!(*stats.providers.get("anthropic").unwrap(), 2);
        assert_eq!(*stats.providers.get("openai").unwrap(), 1);
    }

    #[test]
    fn batch_context_add_and_get_request() {
        let mut ctx = BatchContext::new(
            "b1".to_string(),
            "anthropic".to_string(),
            Duration::from_secs(60),
        );
        ctx.add_request(make_request("req_1"));
        ctx.add_request(make_request("req_2"));

        assert!(ctx.get_request("req_1").is_some());
        assert!(ctx.get_request("req_2").is_some());
        assert!(ctx.get_request("req_3").is_none());
    }

    #[test]
    fn batch_context_is_expired() {
        let mut ctx = BatchContext::new(
            "b1".to_string(),
            "anthropic".to_string(),
            Duration::from_millis(1),
        );
        assert!(!ctx.is_expired());
        thread::sleep(Duration::from_millis(5));
        assert!(ctx.is_expired());
    }

    #[test]
    fn store_overwrites_existing() {
        let store = BatchContextStore::new(Duration::from_secs(60), 100);
        let ctx1 = BatchContext::new(
            "batch_1".to_string(),
            "anthropic".to_string(),
            Duration::from_secs(60),
        );
        store.store(ctx1);

        let mut ctx2 = BatchContext::new(
            "batch_1".to_string(),
            "openai".to_string(),
            Duration::from_secs(60),
        );
        ctx2.add_request(make_request("new_req"));
        store.store(ctx2);

        let r = store.get("batch_1").unwrap();
        assert_eq!(r.provider, "openai");
        assert_eq!(r.requests.len(), 1);
    }
}
