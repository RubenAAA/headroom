//! ``MemoryDecision``: canonical "should we inject memory context?" gate.
//!
//! Decision-only value type. Pure function — same Rust-port shape as
//! ``CompressionDecision``. Precedence: bypass > no_handler > no_user >
//! mode_disabled > mode_tool > inject.
//!
//! Mirrors Python's `headroom.proxy.memory_decision`.

use serde_json::Value;

/// Bypass header names checked for memory-injection suppression.
const BYPASS_HEADERS: &[&str] = &["x-headroom-bypass", "x-headroom-mode"];

/// Check if the bypass header is set to a value that disables mutation.
fn headroom_bypass_enabled(headers: &Value) -> bool {
    if let Some(obj) = headers.as_object() {
        for key in BYPASS_HEADERS {
            if let Some(val) = obj.get(*key).and_then(Value::as_str) {
                let lower = val.to_lowercase();
                if lower == "true" || lower == "1" || lower == "on" || lower == "passthrough" {
                    return true;
                }
            }
        }
    }
    false
}

/// Immutable snapshot of the memory-injection decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDecision {
    pub inject: bool,
    pub skip_reason: Option<String>,
    pub bypass_header_set: bool,
    pub memory_handler_present: bool,
    pub memory_user_id_present: bool,
    pub mode_name: String,
}

impl MemoryDecision {
    /// Compute the canonical memory-injection decision.
    ///
    /// `headers` — inbound request headers (any JSON value with string keys).
    /// `memory_handler_present` — whether a memory backend is configured.
    /// `memory_user_id` — per-request user_id, or None/empty.
    /// `mode_name` — "auto_tail" / "tool" / "disabled".
    pub fn decide(
        headers: &Value,
        memory_handler_present: bool,
        memory_user_id: Option<&str>,
        mode_name: &str,
    ) -> Self {
        let bypass = headroom_bypass_enabled(headers);
        let has_user = memory_user_id.map(|s| !s.is_empty()).unwrap_or(false);

        let (inject, skip_reason) = if bypass {
            (false, Some("bypass_header".to_string()))
        } else if !memory_handler_present {
            (false, Some("no_handler".to_string()))
        } else if !has_user {
            (false, Some("no_user_id".to_string()))
        } else if mode_name == "disabled" {
            (false, Some("mode_disabled".to_string()))
        } else if mode_name == "tool" {
            (false, Some("mode_tool".to_string()))
        } else {
            (true, None)
        };

        Self {
            inject,
            skip_reason,
            bypass_header_set: bypass,
            memory_handler_present,
            memory_user_id_present: has_user,
            mode_name: mode_name.to_string(),
        }
    }

    /// Stamp skip reason into a tags dict for dashboard slicing.
    pub fn apply_to_tags(&self, tags: &mut serde_json::Map<String, Value>) {
        if let Some(ref reason) = self.skip_reason {
            tags.insert(
                "memory_skip_reason".to_string(),
                Value::String(reason.clone()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inject_when_all_conditions_met() {
        let d = MemoryDecision::decide(&json!({}), true, Some("user-1"), "auto_tail");
        assert!(d.inject);
        assert!(d.skip_reason.is_none());
    }

    #[test]
    fn skip_bypass_header() {
        let d = MemoryDecision::decide(
            &json!({"x-headroom-bypass": "true"}),
            true,
            Some("u"),
            "auto_tail",
        );
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("bypass_header"));
        assert!(d.bypass_header_set);
    }

    #[test]
    fn skip_no_handler() {
        let d = MemoryDecision::decide(&json!({}), false, Some("u"), "auto_tail");
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("no_handler"));
    }

    #[test]
    fn skip_no_user_id() {
        let d = MemoryDecision::decide(&json!({}), true, None, "auto_tail");
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("no_user_id"));
    }

    #[test]
    fn skip_empty_user_id() {
        let d = MemoryDecision::decide(&json!({}), true, Some(""), "auto_tail");
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("no_user_id"));
    }

    #[test]
    fn skip_mode_disabled() {
        let d = MemoryDecision::decide(&json!({}), true, Some("u"), "disabled");
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("mode_disabled"));
    }

    #[test]
    fn skip_mode_tool() {
        let d = MemoryDecision::decide(&json!({}), true, Some("u"), "tool");
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("mode_tool"));
    }

    #[test]
    fn bypass_overrides_all() {
        let d = MemoryDecision::decide(&json!({"x-headroom-bypass": "1"}), false, None, "disabled");
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("bypass_header"));
    }

    #[test]
    fn apply_to_tags_with_reason() {
        let d = MemoryDecision::decide(&json!({}), false, None, "auto_tail");
        let mut tags = serde_json::Map::new();
        d.apply_to_tags(&mut tags);
        assert_eq!(tags["memory_skip_reason"], "no_handler");
    }

    #[test]
    fn apply_to_tags_no_reason() {
        let d = MemoryDecision::decide(&json!({}), true, Some("u"), "auto_tail");
        let mut tags = serde_json::Map::new();
        d.apply_to_tags(&mut tags);
        assert!(tags.is_empty());
    }

    #[test]
    fn bypass_x_headroom_mode() {
        let d = MemoryDecision::decide(
            &json!({"x-headroom-mode": "passthrough"}),
            true,
            Some("u"),
            "auto_tail",
        );
        assert!(!d.inject);
        assert_eq!(d.skip_reason.as_deref(), Some("bypass_header"));
    }
}
