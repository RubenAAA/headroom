//! Memory backend trait and search result types (Rust port of the
//! `MemoryBackend` Protocol from `headroom/memory/system.py` and
//! `MemorySearchResult` from `headroom/memory/ports.py`).
//!
//! The trait is synchronous — async I/O lives in the proxy crate where
//! tokio is available. This crate defines only the interface.

use super::models::Memory;

/// Result from a memory search, pairing a memory with its similarity score.
#[derive(Debug, Clone)]
pub struct MemorySearchResult {
    pub memory: Memory,
    /// Cosine similarity score (0.0 – 1.0).
    pub similarity: f64,
    /// Position in results (1-indexed).
    pub rank: usize,
}

/// Protocol defining the interface for memory storage backends.
///
/// Mirrors the Python `MemoryBackend` Protocol from `system.py`. Methods
/// are synchronous here; async wrappers live in the proxy crate.
pub trait MemoryBackend: Send + Sync {
    /// Save a new memory to the backend.
    fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        entities: Option<&[String]>,
        relationships: Option<&[Relationship]>,
        session_id: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Memory;

    /// Search memories by semantic similarity.
    fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        entities: Option<&[String]>,
        include_related: bool,
        top_k: usize,
        session_id: Option<&str>,
    ) -> Vec<MemorySearchResult>;

    /// Update an existing memory with new content (supersedes old version).
    fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        reason: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<Memory, UpdateError>;

    /// Delete a memory from the backend. Returns true if deleted.
    fn delete_memory(&self, memory_id: &str, reason: Option<&str>, user_id: Option<&str>) -> bool;

    /// Retrieve a specific memory by ID.
    fn get_memory(&self, memory_id: &str) -> Option<Memory>;

    /// Whether this backend supports graph/relationship queries.
    fn supports_graph(&self) -> bool;

    /// Whether this backend supports vector similarity search.
    fn supports_vector_search(&self) -> bool;

    /// Close the backend and release resources.
    fn close(&self);
}

/// A directed relationship between two entities.
#[derive(Debug, Clone, Default)]
pub struct Relationship {
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation_type: String,
    pub memory_id: Option<String>,
    pub weight: f64,
}

/// Error type for memory update operations.
#[derive(Debug, Clone)]
pub enum UpdateError {
    /// Memory with the given ID was not found.
    NotFound(String),
    /// User ID validation failed.
    Unauthorized { expected: String, got: String },
    /// Backend-specific error.
    Backend(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "memory not found: {id}"),
            Self::Unauthorized { expected, got } => {
                write!(f, "unauthorized: expected {expected}, got {got}")
            }
            Self::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// In-memory mock backend for testing. Stores memories in a Vec.
pub struct MockBackend {
    memories: std::sync::Mutex<Vec<Memory>>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self {
            memories: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl MockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of all stored memories.
    pub fn stored_memories(&self) -> Vec<Memory> {
        self.memories.lock().unwrap().clone()
    }
}

impl MemoryBackend for MockBackend {
    fn save_memory(
        &self,
        content: &str,
        user_id: &str,
        importance: f64,
        _entities: Option<&[String]>,
        _relationships: Option<&[Relationship]>,
        session_id: Option<&str>,
        _metadata: Option<&serde_json::Value>,
    ) -> Memory {
        let mut mem = Memory::new(content, user_id);
        mem.importance = importance;
        mem.session_id = session_id.map(String::from);
        self.memories.lock().unwrap().push(mem.clone());
        mem
    }

    fn search_memories(
        &self,
        query: &str,
        user_id: &str,
        _entities: Option<&[String]>,
        _include_related: bool,
        top_k: usize,
        _session_id: Option<&str>,
    ) -> Vec<MemorySearchResult> {
        let memories = self.memories.lock().unwrap();
        let mut results: Vec<MemorySearchResult> = memories
            .iter()
            .filter(|m| m.user_id == user_id && m.content.contains(query))
            .enumerate()
            .map(|(i, m)| MemorySearchResult {
                memory: m.clone(),
                similarity: 1.0 - (i as f64 * 0.1),
                rank: i + 1,
            })
            .take(top_k)
            .collect();
        results.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        results
    }

    fn update_memory(
        &self,
        memory_id: &str,
        new_content: &str,
        _reason: Option<&str>,
        _user_id: Option<&str>,
    ) -> Result<Memory, UpdateError> {
        let mut memories = self.memories.lock().unwrap();
        if let Some(existing) = memories.iter_mut().find(|m| m.id == memory_id) {
            let mut updated = existing.clone();
            updated.content = new_content.to_string();
            updated.supersedes = Some(existing.id.clone());
            existing.valid_until = Some(chrono::Utc::now());
            existing.superseded_by = Some(updated.id.clone());
            memories.push(updated.clone());
            Ok(updated)
        } else {
            Err(UpdateError::NotFound(memory_id.into()))
        }
    }

    fn delete_memory(
        &self,
        memory_id: &str,
        _reason: Option<&str>,
        _user_id: Option<&str>,
    ) -> bool {
        let mut memories = self.memories.lock().unwrap();
        let before = memories.len();
        memories.retain(|m| m.id != memory_id);
        memories.len() < before
    }

    fn get_memory(&self, memory_id: &str) -> Option<Memory> {
        self.memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == memory_id)
            .cloned()
    }

    fn supports_graph(&self) -> bool {
        false
    }

    fn supports_vector_search(&self) -> bool {
        false
    }

    fn close(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_backend_save_and_get() {
        let backend = MockBackend::new();
        let mem = backend.save_memory("remember this", "alice", 0.7, None, None, None, None);
        assert_eq!(mem.content, "remember this");
        assert_eq!(mem.user_id, "alice");
        assert_eq!(mem.importance, 0.7);

        let found = backend.get_memory(&mem.id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "remember this");
    }

    #[test]
    fn mock_backend_search_filters_by_user() {
        let backend = MockBackend::new();
        backend.save_memory("rust is great", "alice", 0.5, None, None, None, None);
        backend.save_memory("rust is fast", "bob", 0.5, None, None, None, None);

        let results = backend.search_memories("rust", "alice", None, false, 10, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.user_id, "alice");
    }

    #[test]
    fn mock_backend_search_filters_by_content() {
        let backend = MockBackend::new();
        backend.save_memory("rust tips", "alice", 0.5, None, None, None, None);
        backend.save_memory("python tips", "alice", 0.5, None, None, None, None);

        let results = backend.search_memories("rust", "alice", None, false, 10, None);
        assert_eq!(results.len(), 1);
        assert!(results[0].memory.content.contains("rust"));
    }

    #[test]
    fn mock_backend_search_respects_top_k() {
        let backend = MockBackend::new();
        for i in 0..5 {
            backend.save_memory(&format!("memory {i}"), "alice", 0.5, None, None, None, None);
        }
        let results = backend.search_memories("memory", "alice", None, false, 3, None);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn mock_backend_update_supersedes() {
        let backend = MockBackend::new();
        let mem = backend.save_memory("old content", "alice", 0.5, None, None, None, None);
        let updated = backend
            .update_memory(&mem.id, "new content", Some("correction"), None)
            .unwrap();
        assert_eq!(updated.content, "new content");
        assert_eq!(updated.supersedes.as_deref(), Some(mem.id.as_str()));

        // Original is marked as superseded.
        let original = backend.get_memory(&mem.id).unwrap();
        assert!(!original.is_current());
        assert_eq!(original.superseded_by.as_deref(), Some(updated.id.as_str()));
    }

    #[test]
    fn mock_backend_update_not_found() {
        let backend = MockBackend::new();
        let result = backend.update_memory("nonexistent", "new", None, None);
        assert!(result.is_err());
        match result.unwrap_err() {
            UpdateError::NotFound(id) => assert_eq!(id, "nonexistent"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn mock_backend_delete() {
        let backend = MockBackend::new();
        let mem = backend.save_memory("delete me", "alice", 0.5, None, None, None, None);
        assert!(backend.delete_memory(&mem.id, None, None));
        assert!(backend.get_memory(&mem.id).is_none());
        // Double delete returns false.
        assert!(!backend.delete_memory(&mem.id, None, None));
    }

    #[test]
    fn mock_backend_capabilities() {
        let backend = MockBackend::new();
        assert!(!backend.supports_graph());
        assert!(!backend.supports_vector_search());
    }

    #[test]
    fn search_result_rank_and_similarity() {
        let mem = Memory::new("test", "alice");
        let r = MemorySearchResult {
            memory: mem,
            similarity: 0.95,
            rank: 1,
        };
        assert_eq!(r.rank, 1);
        assert!((r.similarity - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn update_error_display() {
        let e = UpdateError::NotFound("abc".into());
        assert_eq!(e.to_string(), "memory not found: abc");

        let e = UpdateError::Unauthorized {
            expected: "alice".into(),
            got: "bob".into(),
        };
        assert!(e.to_string().contains("alice"));
        assert!(e.to_string().contains("bob"));

        let e = UpdateError::Backend("connection refused".into());
        assert!(e.to_string().contains("connection refused"));
    }

    #[test]
    fn relationship_default() {
        let r = Relationship::default();
        assert!(r.source_entity_id.is_empty());
        assert_eq!(r.weight, 0.0);
    }
}
