//! Connection-local memory of `headroom_retrieve` call ids (upstream
//! `682bf73f`).
//!
//! A Codex WebSocket continuation (`previous_response_id`) sends tool outputs
//! without the calls that produced them, so the live zone cannot tell a
//! retrieval result from any other output and would compress it back into a
//! marker. The relay records which call ids were retrievals as the upstream
//! completes each output item, and hands that set to compression.

use std::collections::HashSet;
use std::sync::Arc;

use headroom_core::tool_exclusion::{is_ccr_retrieve_tool, unwrap_tool_call_name};
use serde_json::Value;

/// Ids kept per connection. Evicting one could compress a retrieval the
/// model has not read yet, so at the cap compression stops instead.
pub(super) const RETRIEVAL_CALL_LIMIT: usize = 512;

#[derive(Debug, Default)]
pub(super) struct RetrievalCalls {
    ids: HashSet<String>,
    exhausted: bool,
}

impl RetrievalCalls {
    /// Record one `response.output_item.done` item. Only ids and the
    /// retrieval verdict are kept, never arguments or results. Returns `true`
    /// once, when this item hits the cap.
    pub(super) fn remember(&mut self, item: &Value) -> bool {
        if self.exhausted || item.get("type").and_then(Value::as_str) != Some("function_call") {
            return false;
        }
        let call_id = item.get("call_id").and_then(Value::as_str);
        let name = item.get("name").and_then(Value::as_str);
        let (Some(call_id), Some(name)) = (call_id.filter(|id| !id.is_empty()), name) else {
            return false;
        };
        // Same verdict the live zone gives a call in the current frame.
        let name = unwrap_tool_call_name(name, item.get("arguments"));
        if !(name.ends_with("__headroom_retrieve") || is_ccr_retrieve_tool(name)) {
            self.ids.remove(call_id);
            return false;
        }
        if !self.ids.contains(call_id) && self.ids.len() >= RETRIEVAL_CALL_LIMIT {
            self.exhausted = true;
            return true;
        }
        self.ids.insert(call_id.to_string());
        false
    }

    /// The ids to protect in the next frame, or `None` once the cap was hit:
    /// the frame must then go upstream uncompressed.
    pub(super) fn snapshot(&self) -> Option<Arc<HashSet<String>>> {
        (!self.exhausted).then(|| Arc::new(self.ids.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str) -> Value {
        json!({"type": "function_call", "call_id": id, "name": name, "arguments": "{}"})
    }

    #[test]
    fn keeps_retrieval_ids_and_drops_reused_ones() {
        let mut calls = RetrievalCalls::default();
        calls.remember(&call("r1", "headroom_retrieve"));
        calls.remember(&call("r2", "mcp__headroom__headroom_retrieve"));
        calls.remember(&call("s1", "shell"));
        calls.remember(&json!({"type": "message", "call_id": "m1", "name": "headroom_retrieve"}));
        let ids = calls.snapshot().unwrap();
        assert!(ids.contains("r1") && ids.contains("r2"));
        assert!(!ids.contains("s1") && !ids.contains("m1"));

        // An id reused by an ordinary call is no longer a retrieval.
        calls.remember(&call("r1", "shell"));
        assert!(!calls.snapshot().unwrap().contains("r1"));
    }

    #[test]
    fn the_cap_stops_compression_instead_of_evicting() {
        let mut calls = RetrievalCalls::default();
        for i in 0..RETRIEVAL_CALL_LIMIT {
            assert!(!calls.remember(&call(&format!("r{i}"), "headroom_retrieve")));
        }
        // A known id at the cap is fine; a new one exhausts the set.
        assert!(!calls.remember(&call("r0", "headroom_retrieve")));
        assert!(calls.snapshot().is_some());
        assert!(calls.remember(&call("over", "headroom_retrieve")));
        assert!(calls.snapshot().is_none());
        assert!(
            !calls.remember(&call("again", "headroom_retrieve")),
            "warn once"
        );
    }
}
