//! Hierarchical memory data models.
//!
//! Mirrors Python's `headroom.memory.models`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Memory scope hierarchy levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeLevel {
    User,
    Session,
    Agent,
    Turn,
}

/// A hierarchically-scoped memory with temporal awareness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub user_id: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub turn_id: Option<String>,
    pub created_at: String,
    pub valid_from: String,
    pub valid_until: Option<String>,
    pub importance: f64,
    pub supersedes: Option<String>,
    pub superseded_by: Option<String>,
    pub promoted_from: Option<String>,
    pub promotion_chain: Vec<String>,
    pub access_count: u64,
    pub last_accessed: Option<String>,
    pub entity_refs: Vec<String>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: String::new(),
            user_id: String::new(),
            session_id: None,
            agent_id: None,
            turn_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            valid_from: chrono::Utc::now().to_rfc3339(),
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
}
