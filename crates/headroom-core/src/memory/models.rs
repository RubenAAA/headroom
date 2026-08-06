//! Hierarchical memory data models (Rust port of `headroom/memory/models.py`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Memory scope hierarchy levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeLevel {
    /// Persistent across all sessions.
    User,
    /// Persistent within a task/conversation.
    Session,
    /// Persistent within an agent's lifetime.
    Agent,
    /// Ephemeral, single LLM call.
    Turn,
}

/// A hierarchically-scoped memory with temporal awareness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    // Identity
    pub id: String,
    pub content: String,

    // Hierarchical scoping (required: user_id, optional: narrower scopes)
    pub user_id: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub turn_id: Option<String>,

    // Temporal
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,

    // Classification
    pub importance: f64,

    // Lineage (for supersession and bubbling)
    pub supersedes: Option<String>,
    pub superseded_by: Option<String>,
    pub promoted_from: Option<String>,
    pub promotion_chain: Vec<String>,

    // Access tracking
    pub access_count: i64,
    pub last_accessed: Option<DateTime<Utc>>,

    // Entity references
    pub entity_refs: Vec<String>,

    // Metadata
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Default for Memory {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: String::new(),
            user_id: String::new(),
            session_id: None,
            agent_id: None,
            turn_id: None,
            created_at: now,
            valid_from: now,
            valid_until: None,
            importance: 0.5,
            supersedes: None,
            superseded_by: None,
            promoted_from: None,
            promotion_chain: Vec::new(),
            access_count: 0,
            last_accessed: None,
            entity_refs: Vec::new(),
            metadata: HashMap::new(),
        }
    }
}

impl Memory {
    /// Compute the scope level from hierarchy fields.
    pub fn scope_level(&self) -> ScopeLevel {
        if self.turn_id.is_some() {
            ScopeLevel::Turn
        } else if self.agent_id.is_some() {
            ScopeLevel::Agent
        } else if self.session_id.is_some() {
            ScopeLevel::Session
        } else {
            ScopeLevel::User
        }
    }

    /// Check if this memory is current (not superseded).
    pub fn is_current(&self) -> bool {
        self.valid_until.is_none()
    }

    /// Construct a builder-style new Memory with a given content and user_id.
    pub fn new(content: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            user_id: user_id.into(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_memory_has_uuid_and_now() {
        let m = Memory::default();
        assert!(!m.id.is_empty());
        assert_eq!(m.content, "");
        assert_eq!(m.user_id, "");
        assert_eq!(m.importance, 0.5);
        assert!(m.is_current());
    }

    #[test]
    fn scope_level_user_when_no_narrower_scopes() {
        let m = Memory::new("test", "alice");
        assert_eq!(m.scope_level(), ScopeLevel::User);
    }

    #[test]
    fn scope_level_session() {
        let m = Memory {
            session_id: Some("s1".into()),
            ..Memory::new("test", "alice")
        };
        assert_eq!(m.scope_level(), ScopeLevel::Session);
    }

    #[test]
    fn scope_level_agent() {
        let m = Memory {
            session_id: Some("s1".into()),
            agent_id: Some("a1".into()),
            ..Memory::new("test", "alice")
        };
        assert_eq!(m.scope_level(), ScopeLevel::Agent);
    }

    #[test]
    fn scope_level_turn() {
        let m = Memory {
            session_id: Some("s1".into()),
            agent_id: Some("a1".into()),
            turn_id: Some("t1".into()),
            ..Memory::new("test", "alice")
        };
        assert_eq!(m.scope_level(), ScopeLevel::Turn);
    }

    #[test]
    fn is_current_false_when_superseded() {
        let m = Memory {
            valid_until: Some(Utc::now()),
            ..Memory::new("test", "alice")
        };
        assert!(!m.is_current());
    }

    #[test]
    fn new_builder_sets_fields() {
        let m = Memory::new("hello world", "bob");
        assert_eq!(m.content, "hello world");
        assert_eq!(m.user_id, "bob");
        assert!(m.session_id.is_none());
    }

    #[test]
    fn serialization_round_trip() {
        let m = Memory {
            id: "test-id-123".into(),
            content: "remember this".into(),
            user_id: "alice".into(),
            importance: 0.8,
            entity_refs: vec!["rust".into(), "proxy".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test-id-123");
        assert_eq!(back.content, "remember this");
        assert_eq!(back.importance, 0.8);
        assert_eq!(back.entity_refs, vec!["rust", "proxy"]);
    }

    #[test]
    fn scope_level_serde_round_trip() {
        let json = serde_json::to_string(&ScopeLevel::Agent).unwrap();
        assert_eq!(json, "\"agent\"");
        let back: ScopeLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ScopeLevel::Agent);
    }

    #[test]
    fn memory_with_lineage_fields() {
        let m = Memory {
            supersedes: Some("old-id".into()),
            promoted_from: Some("child-id".into()),
            promotion_chain: vec!["child-id".into(), "old-id".into()],
            ..Memory::new("updated", "alice")
        };
        assert_eq!(m.supersedes.as_deref(), Some("old-id"));
        assert_eq!(m.promoted_from.as_deref(), Some("child-id"));
        assert_eq!(m.promotion_chain.len(), 2);
    }
}
