//! Base transform interface for Headroom.
//!
//! Provides the `split_frozen` utility for separating cached-prefix messages
//! from mutable ones, and the `Transform` trait that all pipeline transforms
//! implement.

use serde_json::Value;

/// Split messages into frozen (cached prefix) and mutable portions.
///
/// Frozen messages must not be modified — they are already cache-written
/// by the provider. Returns `(frozen, mutable)`.
pub fn split_frozen(messages: &[Value], frozen_message_count: usize) -> (&[Value], &[Value]) {
    if frozen_message_count == 0 || frozen_message_count >= messages.len() {
        return (&[], messages);
    }
    messages.split_at(frozen_message_count)
}

/// Result of applying a transform to messages.
#[derive(Debug, Clone)]
pub struct TransformResult {
    pub messages: Vec<Value>,
    pub transforms_applied: Vec<String>,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

impl Default for TransformResult {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            transforms_applied: Vec::new(),
            bytes_before: 0,
            bytes_after: 0,
        }
    }
}

/// Trait that all message transforms implement.
///
/// Mirrors Python's `Transform` ABC from `headroom.transforms.base`.
///
/// Nothing implements this yet. It belongs to the same unwired group as
/// [`crate::transforms::compressor_registry`] and
/// `content_router::apply_strategy`: ported for parity, and useful only on
/// per-token API pricing, where paying a compressor to shrink a request buys
/// something. On a subscription the arithmetic does not work, so the flags
/// that would switch these on are off, and no caller was ever written. Kept
/// deliberately — this is staged work, not a leftover.
pub trait Transform {
    /// Human-readable name for this transform.
    fn name(&self) -> &str;

    /// Apply the transform to messages.
    fn apply(&self, messages: &[Value], frozen_message_count: usize) -> TransformResult;

    /// Whether this transform should be applied. Default: always true.
    fn should_apply(&self, _messages: &[Value], _frozen_message_count: usize) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── split_frozen ─────────────────────────────────────────────────────

    #[test]
    fn split_frozen_normal() {
        let msgs = vec![json!("a"), json!("b"), json!("c"), json!("d")];
        let (frozen, mutable) = split_frozen(&msgs, 2);
        assert_eq!(frozen, &[json!("a"), json!("b")]);
        assert_eq!(mutable, &[json!("c"), json!("d")]);
    }

    #[test]
    fn split_frozen_zero() {
        let msgs = vec![json!("a"), json!("b")];
        let (frozen, mutable) = split_frozen(&msgs, 0);
        assert!(frozen.is_empty());
        assert_eq!(mutable, &msgs);
    }

    #[test]
    fn split_frozen_all() {
        let msgs = vec![json!("a"), json!("b")];
        let (frozen, mutable) = split_frozen(&msgs, 2);
        assert!(frozen.is_empty());
        assert_eq!(mutable, &msgs);
    }

    #[test]
    fn split_frozen_beyond_len() {
        let msgs = vec![json!("a")];
        let (frozen, mutable) = split_frozen(&msgs, 100);
        assert!(frozen.is_empty());
        assert_eq!(mutable, &msgs);
    }

    #[test]
    fn split_frozen_empty_messages() {
        let msgs: Vec<Value> = vec![];
        let (frozen, mutable) = split_frozen(&msgs, 5);
        assert!(frozen.is_empty());
        assert!(mutable.is_empty());
    }

    // ── TransformResult defaults ─────────────────────────────────────────

    #[test]
    fn transform_result_default() {
        let r = TransformResult::default();
        assert!(r.messages.is_empty());
        assert!(r.transforms_applied.is_empty());
        assert_eq!(r.bytes_before, 0);
        assert_eq!(r.bytes_after, 0);
    }
}
