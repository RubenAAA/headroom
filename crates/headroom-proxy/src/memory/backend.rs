//! ``MemoryBackend`` trait — async interface for memory storage.
//!
//! Backends (LocalBackend via PyO3, DirectMem0Adapter, etc.) implement
//! this trait. The proxy routes through it; the actual DB/embedder work
//! stays in Python.
//!
//! Mirrors the public API of Python's `LocalBackend`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::models::Memory;

/// Result of a memory search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f64,
    pub related_entities: Vec<String>,
}

/// Async interface for memory storage backends.
///
/// All methods are async because backends do I/O (SQLite, Qdrant, etc.).
/// The Rust proxy calls through this trait; implementations may live in
/// Python (via PyO3) or be reimplemented natively.
#[async_trait::async_trait]
pub trait MemoryBackend: Send + Sync {
    /// Save a memory entry.
    async fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        facts: Option<&[String]>,
        entities: Option<&[String]>,
        extracted_entities: Option<&[serde_json::Value]>,
        relationships: Option<&[serde_json::Value]>,
        extracted_relationships: Option<&[serde_json::Value]>,
    ) -> Result<Memory, Box<dyn std::error::Error + Send + Sync>>;

    /// Search memories by semantic similarity.
    async fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        top_k: usize,
        include_related: bool,
    ) -> Result<Vec<MemorySearchResult>, Box<dyn std::error::Error + Send + Sync>>;

    /// Update an existing memory.
    async fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        user_id: &str,
        reason: Option<&str>,
    ) -> Result<Memory, Box<dyn std::error::Error + Send + Sync>>;

    /// Delete a memory by ID.
    async fn delete_memory(
        &self,
        memory_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Get a single memory by ID.
    async fn get_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<Memory>, Box<dyn std::error::Error + Send + Sync>>;

    /// Clear all memories for a user.
    async fn clear_user(
        &self,
        user_id: &str,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>;

    /// Close the backend and release resources.
    async fn close(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Search with graph expansion (optional capability).
#[async_trait::async_trait]
pub trait GraphBackend: MemoryBackend {
    /// Query a subgraph around given entities.
    async fn query_subgraph(
        &self,
        entities: &[String],
        depth: usize,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

    /// Check if graph is supported.
    fn supports_graph(&self) -> bool;
}
