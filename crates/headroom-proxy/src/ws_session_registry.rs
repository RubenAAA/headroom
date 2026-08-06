//! WebSocket session registry for Codex relay lifecycle tracking.
//!
//! In-memory registry of active WS sessions. Provides first-class visibility
//! of active sessions, gauges for Prometheus, and a home for relay-task references.

use std::collections::HashMap;
use std::time::Instant;

/// Termination cause for a WebSocket session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationCause {
    ClientDisconnect,
    UpstreamDisconnect,
    UpstreamError,
    ClientError,
    ClientCancel,
    ResponseCompleted,
    ClientTimeout,
    Unknown,
}

impl std::fmt::Display for TerminationCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TerminationCause::ClientDisconnect => write!(f, "client_disconnect"),
            TerminationCause::UpstreamDisconnect => write!(f, "upstream_disconnect"),
            TerminationCause::UpstreamError => write!(f, "upstream_error"),
            TerminationCause::ClientError => write!(f, "client_error"),
            TerminationCause::ClientCancel => write!(f, "client_cancel"),
            TerminationCause::ResponseCompleted => write!(f, "response_completed"),
            TerminationCause::ClientTimeout => write!(f, "client_timeout"),
            TerminationCause::Unknown => write!(f, "unknown"),
        }
    }
}

/// Per-session state entry.
#[derive(Debug, Clone)]
pub struct WSSessionHandle {
    pub session_id: String,
    pub request_id: String,
    pub client_addr: Option<String>,
    pub upstream_url: Option<String>,
    pub started_at: Instant,
    pub last_activity_at: Instant,
    pub relay_task_count: usize,
    /// Names of active relay tasks (e.g. "codex-ws-c2u-<sid>").
    /// Matches Python's `relay_tasks: list[_TaskLike]` → `[t.get_name() ...]`.
    pub relay_task_names: Vec<String>,
    pub termination_cause: Option<TerminationCause>,
}

impl WSSessionHandle {
    pub fn new(session_id: String, request_id: String) -> Self {
        let now = Instant::now();
        Self {
            session_id,
            request_id,
            client_addr: None,
            upstream_url: None,
            started_at: now,
            last_activity_at: now,
            relay_task_count: 0,
            relay_task_names: Vec::new(),
            termination_cause: None,
        }
    }

    pub fn mark_activity(&mut self) {
        self.last_activity_at = Instant::now();
    }

    pub fn age_seconds(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    pub fn idle_seconds(&self) -> f64 {
        self.last_activity_at.elapsed().as_secs_f64()
    }

    pub fn to_snapshot_dict(&self) -> serde_json::Value {
        serde_json::json!({
            "session_id": self.session_id,
            "request_id": self.request_id,
            "client_addr": self.client_addr,
            "upstream_url": self.upstream_url,
            "age_seconds": self.age_seconds(),
            "idle_seconds": self.idle_seconds(),
            "relay_task_count": self.relay_task_count,
            "relay_task_names": self.relay_task_names,
            "termination_cause": self.termination_cause.as_ref().map(|c| c.to_string()),
        })
    }
}

/// In-memory registry of active Codex WS sessions.
pub struct WebSocketSessionRegistry {
    sessions: HashMap<String, WSSessionHandle>,
    active_relay_tasks: usize,
}

impl WebSocketSessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            active_relay_tasks: 0,
        }
    }

    /// Register a session. Idempotent by session_id.
    pub fn register(&mut self, handle: WSSessionHandle) {
        if let Some(existing) = self.sessions.get(&handle.session_id) {
            self.active_relay_tasks -= existing.relay_task_count;
        }
        self.active_relay_tasks += handle.relay_task_count;
        self.sessions.insert(handle.session_id.clone(), handle);
    }

    /// Remove a session. Idempotent: returns None if unknown.
    pub fn deregister(
        &mut self,
        session_id: &str,
        cause: TerminationCause,
    ) -> Option<(WSSessionHandle, usize)> {
        let mut handle = self.sessions.remove(session_id)?;
        handle.termination_cause = Some(cause);
        let released = handle.relay_task_count;
        self.active_relay_tasks = self.active_relay_tasks.saturating_sub(released);
        handle.relay_task_count = 0; // clear references
        Some((handle, released))
    }

    /// Get a session handle by ID.
    pub fn get(&self, session_id: &str) -> Option<&WSSessionHandle> {
        self.sessions.get(session_id)
    }

    /// Refresh a session's last-activity timestamp (Codex WS relay
    /// frame bookkeeping). No-op for an unknown session id.
    pub fn mark_activity(&mut self, session_id: &str) {
        if let Some(handle) = self.sessions.get_mut(session_id) {
            handle.mark_activity();
        }
    }

    /// Number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    /// Number of active relay tasks across all sessions.
    pub fn active_relay_task_count(&self) -> usize {
        self.active_relay_tasks
    }

    /// JSON-serializable view of the registry.
    pub fn snapshot(&self) -> Vec<serde_json::Value> {
        self.sessions
            .values()
            .map(|h| h.to_snapshot_dict())
            .collect()
    }
}

impl Default for WebSocketSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_count() {
        let mut reg = WebSocketSessionRegistry::new();
        let handle = WSSessionHandle::new("s1".into(), "r1".into());
        reg.register(handle);
        assert_eq!(reg.active_count(), 1);
    }

    #[test]
    fn deregister_returns_handle() {
        let mut reg = WebSocketSessionRegistry::new();
        reg.register(WSSessionHandle::new("s1".into(), "r1".into()));
        let result = reg.deregister("s1", TerminationCause::ClientDisconnect);
        assert!(result.is_some());
        let (handle, released) = result.unwrap();
        assert_eq!(handle.session_id, "s1");
        assert_eq!(released, 0);
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn deregister_unknown_returns_none() {
        let mut reg = WebSocketSessionRegistry::new();
        assert!(reg
            .deregister("unknown", TerminationCause::Unknown)
            .is_none());
    }

    #[test]
    fn idempotent_register() {
        let mut reg = WebSocketSessionRegistry::new();
        reg.register(WSSessionHandle::new("s1".into(), "r1".into()));
        reg.register(WSSessionHandle::new("s1".into(), "r2".into()));
        assert_eq!(reg.active_count(), 1);
    }

    #[test]
    fn relay_task_count_tracked() {
        let mut reg = WebSocketSessionRegistry::new();
        let mut handle = WSSessionHandle::new("s1".into(), "r1".into());
        handle.relay_task_count = 3;
        reg.register(handle);
        assert_eq!(reg.active_relay_task_count(), 3);

        let (_, released) = reg
            .deregister("s1", TerminationCause::ResponseCompleted)
            .unwrap();
        assert_eq!(released, 3);
        assert_eq!(reg.active_relay_task_count(), 0);
    }

    #[test]
    fn snapshot_returns_json() {
        let mut reg = WebSocketSessionRegistry::new();
        reg.register(WSSessionHandle::new("s1".into(), "r1".into()));
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0]["session_id"], "s1");
    }

    #[test]
    fn snapshot_includes_relay_task_names() {
        let mut handle = WSSessionHandle::new("s1".into(), "r1".into());
        handle.relay_task_names = vec!["codex-ws-c2u-s1".into(), "codex-ws-u2c-s1".into()];
        handle.relay_task_count = 2;
        let mut reg = WebSocketSessionRegistry::new();
        reg.register(handle);
        let snap = reg.snapshot();
        let names = snap[0]["relay_task_names"].as_array().unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "codex-ws-c2u-s1");
        assert_eq!(names[1], "codex-ws-u2c-s1");
    }
}
