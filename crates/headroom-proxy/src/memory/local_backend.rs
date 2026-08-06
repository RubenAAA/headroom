//! In-memory memory backend with text-based search.
//!
//! Implements `MemoryBackend` using a `HashMap`-based store with
//! simple text-matching search (no vector embeddings needed). This
//! makes the memory system functional in the Rust proxy without
//! requiring SQLite or external services.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use super::backend::{MemoryBackend, MemorySearchResult};
use super::models::Memory;

/// In-memory memory backend. Stores memories in a `Vec` and provides
/// basic text-matching search. Good enough for single-instance proxy
/// deployments where persistence across restarts is not required.
pub struct LocalMemoryBackend {
    memories: Arc<tokio::sync::RwLock<Vec<Memory>>>,
}

impl LocalMemoryBackend {
    pub fn new() -> Self {
        Self {
            memories: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    /// Simple text relevance score: counts overlapping words between
    /// query and content, normalized by query length.
    fn text_relevance(query: &str, content: &str) -> f64 {
        let query_words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
        if query_words.is_empty() {
            return 0.0;
        }
        let content_lower = content.to_lowercase();
        let matches = query_words
            .iter()
            .filter(|w| content_lower.contains(w.as_str()))
            .count();
        matches as f64 / query_words.len() as f64
    }
}

impl Default for LocalMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MemoryBackend for LocalMemoryBackend {
    async fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        _facts: Option<&[String]>,
        entities: Option<&[String]>,
        _extracted_entities: Option<&[serde_json::Value]>,
        _relationships: Option<&[serde_json::Value]>,
        _extracted_relationships: Option<&[serde_json::Value]>,
    ) -> Result<Memory, Box<dyn std::error::Error + Send + Sync>> {
        let mut memory = Memory {
            content: content.to_string(),
            user_id: user_id.to_string(),
            importance,
            ..Memory::default()
        };
        if let Some(ents) = entities {
            memory.entity_refs = ents.to_vec();
        }
        let mut store = self.memories.write().await;
        store.push(memory.clone());
        Ok(memory)
    }

    async fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        top_k: usize,
        _include_related: bool,
    ) -> Result<Vec<MemorySearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let store = self.memories.read().await;
        let mut scored: Vec<MemorySearchResult> = store
            .iter()
            .filter(|m| m.user_id == user_id && m.is_current())
            .map(|m| {
                let score = Self::text_relevance(query, &m.content);
                MemorySearchResult {
                    memory: m.clone(),
                    score,
                    related_entities: Vec::new(),
                }
            })
            .filter(|r| r.score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);
        Ok(scored)
    }

    async fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        _user_id: &str,
        _reason: Option<&str>,
    ) -> Result<Memory, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = self.memories.write().await;
        if let Some(m) = store.iter_mut().find(|m| m.id == memory_id) {
            m.content = new_content.to_string();
            Ok(m.clone())
        } else {
            Err(format!("Memory {memory_id} not found").into())
        }
    }

    async fn delete_memory(
        &self,
        memory_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = self.memories.write().await;
        let before = store.len();
        store.retain(|m| m.id != memory_id);
        Ok(store.len() < before)
    }

    async fn get_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<Memory>, Box<dyn std::error::Error + Send + Sync>> {
        let store = self.memories.read().await;
        Ok(store.iter().find(|m| m.id == memory_id).cloned())
    }

    async fn clear_user(
        &self,
        user_id: &str,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = self.memories.write().await;
        let before = store.len();
        store.retain(|m| m.user_id != user_id);
        Ok(before - store.len())
    }

    async fn close(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_and_search() {
        let backend = LocalMemoryBackend::new();
        backend
            .save_memory(
                "I prefer dark mode",
                "alice",
                0.8,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        backend
            .save_memory(
                "My favorite language is Rust",
                "alice",
                0.6,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let results = backend
            .search_memories("dark mode", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0.0);
        assert_eq!(results[0].memory.content, "I prefer dark mode");
    }

    #[tokio::test]
    async fn search_filters_by_user() {
        let backend = LocalMemoryBackend::new();
        backend
            .save_memory("Alice's note", "alice", 0.5, None, None, None, None, None)
            .await
            .unwrap();
        backend
            .save_memory("Bob's note", "bob", 0.5, None, None, None, None, None)
            .await
            .unwrap();

        let results = backend
            .search_memories("note", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.user_id, "alice");
    }

    #[tokio::test]
    async fn update_and_delete() {
        let backend = LocalMemoryBackend::new();
        let m = backend
            .save_memory("original", "alice", 0.5, None, None, None, None, None)
            .await
            .unwrap();

        let updated = backend
            .update_memory(&m.id, "updated", "alice", None)
            .await
            .unwrap();
        assert_eq!(updated.content, "updated");

        assert!(backend.delete_memory(&m.id).await.unwrap());
        assert!(backend.get_memory(&m.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clear_user() {
        let backend = LocalMemoryBackend::new();
        backend
            .save_memory("a1", "alice", 0.5, None, None, None, None, None)
            .await
            .unwrap();
        backend
            .save_memory("a2", "alice", 0.5, None, None, None, None, None)
            .await
            .unwrap();
        backend
            .save_memory("b1", "bob", 0.5, None, None, None, None, None)
            .await
            .unwrap();

        let cleared = backend.clear_user("alice").await.unwrap();
        assert_eq!(cleared, 2);

        let results = backend
            .search_memories("a", "alice", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 0);

        let results = backend
            .search_memories("b", "bob", 10, false)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }
}
