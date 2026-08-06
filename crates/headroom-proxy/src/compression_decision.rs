//! `CompressionDecision`: the canonical value type for "should this request
//! be compressed?".
//!
//! Rust port of `headroom/proxy/compression_decision.py`. This is the
//! input-side analog of `RequestOutcome` and the text-body sibling of
//! `image_compression_decision::ImageCompressionDecision`.
//!
//! Precedence (highest first):
//!
//! 1. `bypass_header`         — user's explicit `x-headroom-bypass: true` /
//!    `x-headroom-mode: passthrough` opt-out (a contract assertion about
//!    prefix-cache stability; honoured above all else).
//! 2. `compression_disabled` — operator kill switch (`config.optimize=False`).
//! 3. `no_messages`          — empty / missing messages on the body.
//! 4. `license_denied`       — commercial gate said no (only meaningful when
//!    there is something to compress, so it comes last).

use crate::headers::headroom_bypass_enabled;
use http::header::HeaderMap;
use std::collections::HashMap;

/// Canonical reason a request was passed through uncompressed.
///
/// `as_str()` values match the Python strings byte-for-byte so dashboards
/// slicing on `passthrough_reason` behave identically across the two runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassthroughReason {
    BypassHeader,
    CompressionDisabled,
    NoMessages,
    LicenseDenied,
}

impl PassthroughReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PassthroughReason::BypassHeader => "bypass_header",
            PassthroughReason::CompressionDisabled => "compression_disabled",
            PassthroughReason::NoMessages => "no_messages",
            PassthroughReason::LicenseDenied => "license_denied",
        }
    }
}

/// Immutable, value-equal snapshot of the input-side compression decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionDecision {
    pub should_compress: bool,
    /// When `should_compress` is False, the canonical reason.
    pub passthrough_reason: Option<PassthroughReason>,
    // ── Observability: every constituent boolean exposed ──────────
    pub bypass_header_set: bool,
    pub config_optimize_enabled: bool,
    pub license_allows: bool,
    pub has_messages: bool,
}

impl CompressionDecision {
    /// Compute the canonical decision for one request.
    ///
    /// Precedence: bypass > config.optimize > no_messages > license_denied,
    /// matching `compression_decision.py::CompressionDecision.decide`.
    pub fn decide(
        headers: &HeaderMap,
        config_optimize_enabled: bool,
        license_allows: bool,
        has_messages: bool,
    ) -> Self {
        let bypass = headroom_bypass_enabled(headers);

        let (should, reason) = if bypass {
            (false, Some(PassthroughReason::BypassHeader))
        } else if !config_optimize_enabled {
            (false, Some(PassthroughReason::CompressionDisabled))
        } else if !has_messages {
            (false, Some(PassthroughReason::NoMessages))
        } else if !license_allows {
            (false, Some(PassthroughReason::LicenseDenied))
        } else {
            (true, None)
        };

        Self {
            should_compress: should,
            passthrough_reason: reason,
            bypass_header_set: bypass,
            config_optimize_enabled,
            license_allows,
            has_messages,
        }
    }

    /// Stamp the passthrough reason into a tags map for downstream
    /// observability. Mutates in place. No-op when `should_compress=True`
    /// (absence vs presence of the tag is itself the signal). Overwrites any
    /// pre-existing `passthrough_reason` entry, mirroring the Python dict
    /// assignment.
    pub fn apply_to_tags(&self, tags: &mut HashMap<String, String>) {
        if let Some(reason) = self.passthrough_reason {
            tags.insert(
                "passthrough_reason".to_string(),
                reason.as_str().to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    // ── Value-type contract ────────────────────────────────────────

    #[test]
    fn test_decision_is_value_equal() {
        let a = CompressionDecision::decide(&HeaderMap::new(), true, true, true);
        let b = CompressionDecision::decide(&HeaderMap::new(), true, true, true);
        assert_eq!(a, b);
    }

    #[test]
    fn test_compresses_when_every_gate_open() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, true);
        assert!(d.should_compress);
        assert!(d.passthrough_reason.is_none());
    }

    // ── Individual gates ──────────────────────────────────────────

    #[test]
    fn test_bypass_header_wins_over_every_other_gate() {
        let h = headers_with(&[("x-headroom-bypass", "true")]);
        let d = CompressionDecision::decide(&h, true, true, true);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::BypassHeader));
    }

    #[test]
    fn test_passthrough_mode_header_also_triggers_bypass() {
        let h = headers_with(&[("x-headroom-mode", "passthrough")]);
        let d = CompressionDecision::decide(&h, true, true, true);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::BypassHeader));
    }

    #[test]
    fn test_bypass_header_is_case_insensitive() {
        let h = headers_with(&[("x-headroom-bypass", "TRUE")]);
        let d = CompressionDecision::decide(&h, true, true, true);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::BypassHeader));
    }

    #[test]
    fn test_bypass_header_value_must_be_true() {
        let h = headers_with(&[("x-headroom-bypass", "1")]);
        let d = CompressionDecision::decide(&h, true, true, true);
        // "1" is not "true" → not bypass; every other gate open → compress.
        assert!(d.should_compress);
        assert!(!d.bypass_header_set);
    }

    #[test]
    fn test_config_optimize_disabled_is_passthrough() {
        let d = CompressionDecision::decide(&HeaderMap::new(), false, true, true);
        assert!(!d.should_compress);
        assert_eq!(
            d.passthrough_reason,
            Some(PassthroughReason::CompressionDisabled)
        );
    }

    #[test]
    fn test_no_messages_is_passthrough() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, false);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::NoMessages));
    }

    #[test]
    fn test_messages_none_is_passthrough() {
        // Rust models "None messages" and "[] messages" identically via
        // has_messages=false (the caller collapses both, as Python's
        // bool(messages) does).
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, false);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::NoMessages));
    }

    #[test]
    fn test_license_denied_is_passthrough() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, false, true);
        assert!(!d.should_compress);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::LicenseDenied));
    }

    #[test]
    fn test_usage_reporter_none_is_treated_as_license_allows() {
        // No licensing system configured ⇒ license_allows=true (caller passes
        // true). Compresses when the other gates are open.
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, true);
        assert!(d.should_compress);
        assert!(d.license_allows);
    }

    // ── Precedence ordering ──────────────────────────────────────

    #[test]
    fn test_bypass_beats_compression_disabled() {
        let h = headers_with(&[("x-headroom-bypass", "true")]);
        let d = CompressionDecision::decide(&h, false, true, true);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::BypassHeader));
    }

    #[test]
    fn test_bypass_beats_no_messages() {
        let h = headers_with(&[("x-headroom-bypass", "true")]);
        let d = CompressionDecision::decide(&h, true, true, false);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::BypassHeader));
    }

    #[test]
    fn test_bypass_beats_license_denied() {
        let h = headers_with(&[("x-headroom-bypass", "true")]);
        let d = CompressionDecision::decide(&h, true, false, true);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::BypassHeader));
    }

    #[test]
    fn test_config_disabled_beats_no_messages() {
        let d = CompressionDecision::decide(&HeaderMap::new(), false, true, false);
        assert_eq!(
            d.passthrough_reason,
            Some(PassthroughReason::CompressionDisabled)
        );
    }

    #[test]
    fn test_no_messages_beats_license_denied() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, false, false);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::NoMessages));
    }

    // ── Observability fields ─────────────────────────────────────

    #[test]
    fn test_observability_booleans_populated_when_compressing() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, true);
        assert!(!d.bypass_header_set);
        assert!(d.config_optimize_enabled);
        assert!(d.license_allows);
        assert!(d.has_messages);
    }

    #[test]
    fn test_observability_booleans_populated_when_passthrough() {
        let h = headers_with(&[("x-headroom-bypass", "true")]);
        let d = CompressionDecision::decide(&h, true, true, true);
        assert!(d.bypass_header_set);
        assert!(d.config_optimize_enabled);
        assert!(d.license_allows);
        assert!(d.has_messages);
    }

    #[test]
    fn test_decide_with_missing_messages_field_on_body() {
        // Missing messages field ⇒ has_messages=false ⇒ no_messages skip.
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, false);
        assert_eq!(d.passthrough_reason, Some(PassthroughReason::NoMessages));
    }

    // ── apply_to_tags ────────────────────────────────────────────

    #[test]
    fn test_apply_to_tags_stamps_reason_when_passthrough() {
        let h = headers_with(&[("x-headroom-bypass", "true")]);
        let d = CompressionDecision::decide(&h, true, true, true);
        let mut tags = HashMap::new();
        d.apply_to_tags(&mut tags);
        assert_eq!(
            tags.get("passthrough_reason").map(|s| s.as_str()),
            Some("bypass_header")
        );
    }

    #[test]
    fn test_apply_to_tags_is_a_noop_when_compressing() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, true);
        let mut tags = HashMap::from([("client".to_string(), "codex".to_string())]);
        d.apply_to_tags(&mut tags);
        assert_eq!(tags.get("client").map(|s| s.as_str()), Some("codex"));
        assert!(tags.get("passthrough_reason").is_none());
    }

    #[test]
    fn test_apply_to_tags_preserves_pre_existing_entries() {
        let d = CompressionDecision::decide(&HeaderMap::new(), false, true, true);
        let mut tags = HashMap::from([("client".to_string(), "claude-code".to_string())]);
        d.apply_to_tags(&mut tags);
        assert_eq!(tags.get("client").map(|s| s.as_str()), Some("claude-code"));
        assert_eq!(
            tags.get("passthrough_reason").map(|s| s.as_str()),
            Some("compression_disabled")
        );
    }

    #[test]
    fn test_apply_to_tags_for_every_passthrough_reason() {
        let cases: Vec<(&str, HeaderMap, bool, bool, bool)> = vec![
            (
                "bypass_header",
                headers_with(&[("x-headroom-bypass", "true")]),
                true,
                true,
                true,
            ),
            ("compression_disabled", HeaderMap::new(), false, true, true),
            ("no_messages", HeaderMap::new(), true, true, false),
            ("license_denied", HeaderMap::new(), true, false, true),
        ];
        for (expected, h, opt, lic, msgs) in cases {
            let d = CompressionDecision::decide(&h, opt, lic, msgs);
            let mut tags = HashMap::new();
            d.apply_to_tags(&mut tags);
            assert_eq!(
                tags.get("passthrough_reason").map(|s| s.as_str()),
                Some(expected),
                "failed for reason: {expected}"
            );
        }
    }

    #[test]
    fn test_apply_to_tags_overwrites_a_pre_existing_passthrough_reason() {
        let d = CompressionDecision::decide(&HeaderMap::new(), true, true, false);
        let mut tags =
            HashMap::from([("passthrough_reason".to_string(), "stale_value".to_string())]);
        d.apply_to_tags(&mut tags);
        assert_eq!(
            tags.get("passthrough_reason").map(|s| s.as_str()),
            Some("no_messages")
        );
    }
}
