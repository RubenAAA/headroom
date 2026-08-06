//! `ImageCompressionDecision`: canonical "should this request have images compressed?" gate.
//!
//! Mirror of text `CompressionDecision`. Precedence (highest first):
//!
//! 1. `bypass_header`           — user's explicit opt-out
//! 2. `image_optimize_disabled` — operator `config.image_optimize=False`
//! 3. `no_messages`             — empty / missing messages
//! 4. otherwise → `should_compress=true`

use std::collections::HashMap;

/// Immutable, value-equal snapshot of the image-compression gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCompressionDecision {
    pub should_compress: bool,
    /// When `should_compress` is False, the canonical reason.
    pub passthrough_reason: Option<String>,
    pub bypass_header_set: bool,
    pub image_optimize_enabled: bool,
    pub has_messages: bool,
}

/// Return True when inbound headers request full Headroom passthrough.
///
/// Checks `x-headroom-bypass: true` or `x-headroom-mode: passthrough`.
pub fn headroom_bypass_enabled(headers: &HashMap<String, String>) -> bool {
    let bypass = headers
        .get("x-headroom-bypass")
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let passthrough = headers
        .get("x-headroom-mode")
        .map(|v| v.trim().eq_ignore_ascii_case("passthrough"))
        .unwrap_or(false);
    bypass || passthrough
}

impl ImageCompressionDecision {
    /// Compute the canonical image-compression decision.
    pub fn decide(
        headers: &HashMap<String, String>,
        image_optimize: bool,
        has_messages: bool,
    ) -> Self {
        let bypass = headroom_bypass_enabled(headers);

        let (should, reason) = if bypass {
            (false, Some("bypass_header".to_string()))
        } else if !image_optimize {
            (false, Some("image_optimize_disabled".to_string()))
        } else if !has_messages {
            (false, Some("no_messages".to_string()))
        } else {
            (true, None)
        };

        Self {
            should_compress: should,
            passthrough_reason: reason,
            bypass_header_set: bypass,
            image_optimize_enabled: image_optimize,
            has_messages,
        }
    }

    /// Stamp the skip reason into a tags dict for dashboard slicing.
    pub fn apply_to_tags(&self, tags: &mut HashMap<String, String>) {
        if let Some(ref reason) = self.passthrough_reason {
            tags.insert("image_skip_reason".to_string(), reason.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Value-type contract ────────────────────────────────────────

    #[test]
    fn decision_is_value_equal() {
        let a = ImageCompressionDecision::decide(&HashMap::new(), true, true);
        let b = ImageCompressionDecision::decide(&HashMap::new(), true, true);
        assert_eq!(a, b);
    }

    // ── Precedence ────────────────────────────────────────────────

    #[test]
    fn compresses_when_every_gate_open() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), true, true);
        assert!(d.should_compress);
        assert!(d.passthrough_reason.is_none());
    }

    #[test]
    fn bypass_header_wins() {
        let headers = HashMap::from([("x-headroom-bypass".to_string(), "true".to_string())]);
        let d = ImageCompressionDecision::decide(&headers, true, true);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason.as_deref(), Some("bypass_header"));
    }

    #[test]
    fn passthrough_mode_header_also_triggers_bypass_skip() {
        let headers = HashMap::from([("x-headroom-mode".to_string(), "passthrough".to_string())]);
        let d = ImageCompressionDecision::decide(&headers, true, true);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason.as_deref(), Some("bypass_header"));
    }

    #[test]
    fn image_optimize_disabled_is_skip() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), false, true);
        assert!(!d.should_compress);
        assert_eq!(
            d.passthrough_reason.as_deref(),
            Some("image_optimize_disabled")
        );
    }

    #[test]
    fn no_messages_is_skip() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), true, false);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason.as_deref(), Some("no_messages"));
    }

    // ── Precedence ordering ──────────────────────────────────────

    #[test]
    fn bypass_beats_image_optimize_disabled() {
        let headers = HashMap::from([("x-headroom-bypass".to_string(), "true".to_string())]);
        let d = ImageCompressionDecision::decide(&headers, false, true);
        assert_eq!(d.passthrough_reason.as_deref(), Some("bypass_header"));
    }

    #[test]
    fn bypass_beats_no_messages() {
        let headers = HashMap::from([("x-headroom-bypass".to_string(), "true".to_string())]);
        let d = ImageCompressionDecision::decide(&headers, true, false);
        assert_eq!(d.passthrough_reason.as_deref(), Some("bypass_header"));
    }

    #[test]
    fn image_optimize_disabled_beats_no_messages() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), false, false);
        assert_eq!(
            d.passthrough_reason.as_deref(),
            Some("image_optimize_disabled")
        );
    }

    // ── Observability fields ─────────────────────────────────────

    #[test]
    fn observability_booleans_populated_when_compressing() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), true, true);
        assert!(!d.bypass_header_set);
        assert!(d.image_optimize_enabled);
        assert!(d.has_messages);
    }

    #[test]
    fn observability_booleans_populated_when_passthrough() {
        let headers = HashMap::from([("x-headroom-bypass".to_string(), "true".to_string())]);
        let d = ImageCompressionDecision::decide(&headers, true, true);
        assert!(d.bypass_header_set);
        assert!(d.image_optimize_enabled);
        assert!(d.has_messages);
    }

    // ── apply_to_tags ────────────────────────────────────────────

    #[test]
    fn apply_to_tags_stamps_reason_when_passthrough() {
        let headers = HashMap::from([("x-headroom-bypass".to_string(), "true".to_string())]);
        let d = ImageCompressionDecision::decide(&headers, true, true);
        let mut tags = HashMap::new();
        d.apply_to_tags(&mut tags);
        assert_eq!(
            tags.get("image_skip_reason").map(|s| s.as_str()),
            Some("bypass_header")
        );
    }

    #[test]
    fn apply_to_tags_is_a_noop_when_compressing() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), true, true);
        let mut tags = HashMap::from([("client".to_string(), "codex".to_string())]);
        d.apply_to_tags(&mut tags);
        assert_eq!(tags.get("client").map(|s| s.as_str()), Some("codex"));
        assert!(tags.get("image_skip_reason").is_none());
    }

    #[test]
    fn apply_to_tags_preserves_pre_existing_entries() {
        let d = ImageCompressionDecision::decide(&HashMap::new(), false, true);
        let mut tags = HashMap::from([
            ("client".to_string(), "claude-code".to_string()),
            (
                "passthrough_reason".to_string(),
                "compression_disabled".to_string(),
            ),
            ("memory_skip_reason".to_string(), "no_user_id".to_string()),
        ]);
        d.apply_to_tags(&mut tags);
        assert_eq!(tags.get("client").map(|s| s.as_str()), Some("claude-code"));
        assert_eq!(
            tags.get("passthrough_reason").map(|s| s.as_str()),
            Some("compression_disabled")
        );
        assert_eq!(
            tags.get("memory_skip_reason").map(|s| s.as_str()),
            Some("no_user_id")
        );
        assert_eq!(
            tags.get("image_skip_reason").map(|s| s.as_str()),
            Some("image_optimize_disabled")
        );
    }

    #[test]
    fn apply_to_tags_for_every_skip_reason() {
        let cases: Vec<(&str, HashMap<String, String>, bool, bool)> = vec![
            (
                "bypass_header",
                HashMap::from([("x-headroom-bypass".to_string(), "true".to_string())]),
                true,
                true,
            ),
            ("image_optimize_disabled", HashMap::new(), false, true),
            ("no_messages", HashMap::new(), true, false),
        ];
        for (expected_reason, headers, image_opt, has_msgs) in cases {
            let d = ImageCompressionDecision::decide(&headers, image_opt, has_msgs);
            let mut tags = HashMap::new();
            d.apply_to_tags(&mut tags);
            assert_eq!(
                tags.get("image_skip_reason").map(|s| s.as_str()),
                Some(expected_reason),
                "failed for reason: {expected_reason}"
            );
        }
    }
}
