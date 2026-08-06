//! Cache alignment detector — Rust port of
//! `headroom.transforms.cache_aligner`.
//!
//! PR-A2 / P2-23 fix: this is a **detector-only** transform. It never
//! mutates messages. It scans system messages for volatile content
//! (UUIDs, ISO 8601 timestamps, JWTs, hex hashes), emits warnings, and
//! computes cache prefix metrics for observability.

use sha2::{Digest, Sha256};

use crate::tokenizer::Tokenizer;

// ─── Constants ───────────────────────────────────────────────────────────

/// MD5 = 32 hex chars, SHA1 = 40, SHA256 = 64.
const HEX_HASH_LENGTHS: &[usize] = &[32, 40, 64];

/// Canonical UUID (RFC 4122) with dashes is 36 chars.
const UUID_CANONICAL_LEN: usize = 36;

/// JWT: exactly 3 dot-separated segments, each >= 4 bytes.
const JWT_SEGMENT_COUNT: usize = 3;
const JWT_MIN_SEGMENT_BYTES: usize = 4;

// ─── Types ───────────────────────────────────────────────────────────────

/// Configuration for cache alignment detection.
#[derive(Debug, Clone)]
pub struct CacheAlignerConfig {
    pub enabled: bool,
}

impl Default for CacheAlignerConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// One detected piece of volatile content.
#[derive(Debug, Clone)]
pub struct VolatileFinding {
    pub label: &'static str,
    pub sample: String,
}

/// Cache prefix metrics for observability.
#[derive(Debug, Clone)]
pub struct CachePrefixMetrics {
    pub stable_prefix_bytes: usize,
    pub stable_prefix_tokens_est: usize,
    pub stable_prefix_hash: String,
    pub prefix_changed: bool,
    pub previous_hash: Option<String>,
}

/// Result of the cache alignment detector.
#[derive(Debug, Clone)]
pub struct CacheAlignerResult {
    pub messages: Vec<serde_json::Value>,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub transforms_applied: Vec<String>,
    pub markers_inserted: Vec<String>,
    pub warnings: Vec<String>,
    pub cache_metrics: Option<CachePrefixMetrics>,
}

/// Mutable state carried across calls (tracks previous prefix hash).
#[derive(Debug, Default)]
pub struct CacheAlignerState {
    pub previous_prefix_hash: Option<String>,
}

// ─── Volatile content detection ──────────────────────────────────────────

fn is_uuid(token: &str) -> bool {
    if token.len() != UUID_CANONICAL_LEN {
        return false;
    }
    if token.chars().filter(|&c| c == '-').count() != 4 {
        return false;
    }
    // Validate hex segments between dashes
    let parts: Vec<&str> = token.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    for part in &parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    // Lengths must match UUID v1-v5 layout: 8-4-4-4-12
    let expected = [8, 4, 4, 4, 12];
    for (part, &exp) in parts.iter().zip(expected.iter()) {
        if part.len() != exp {
            return false;
        }
    }
    true
}

fn is_iso8601(token: &str) -> bool {
    if token.len() < 8 {
        return false;
    }
    if !token.contains('T') && !token.contains('-') {
        return false;
    }
    // Try parsing as ISO 8601. Replace trailing Z with +00:00.
    let candidate = if token.ends_with('Z') {
        format!("{}+00:00", &token[..token.len() - 1])
    } else {
        token.to_string()
    };
    // Basic structural validation: must contain date-like pattern
    // YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS
    let bytes = candidate.as_bytes();
    if bytes.len() < 10 {
        return false;
    }
    // Check for YYYY-MM-DD at start
    let is_digit = |b: u8| b >= b'0' && b <= b'9';
    if bytes.len() >= 10
        && is_digit(bytes[0])
        && is_digit(bytes[1])
        && is_digit(bytes[2])
        && is_digit(bytes[3])
        && bytes[4] == b'-'
        && is_digit(bytes[5])
        && is_digit(bytes[6])
        && bytes[7] == b'-'
        && is_digit(bytes[8])
        && is_digit(bytes[9])
    {
        return true;
    }
    false
}

fn is_jwt_shape(token: &str) -> bool {
    if token.matches('.').count() != JWT_SEGMENT_COUNT - 1 {
        return false;
    }
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() != JWT_SEGMENT_COUNT {
        return false;
    }
    for seg in &segments {
        if seg.len() < JWT_MIN_SEGMENT_BYTES {
            return false;
        }
        // base64url alphabet: A-Z, a-z, 0-9, -, _
        if !seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return false;
        }
    }
    true
}

fn is_hex_hash(token: &str) -> bool {
    if !HEX_HASH_LENGTHS.contains(&token.len()) {
        return false;
    }
    token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn classify_token(token: &str) -> Option<&'static str> {
    if is_uuid(token) {
        return Some("uuid");
    }
    if token.contains('.') && is_jwt_shape(token) {
        return Some("jwt");
    }
    if is_iso8601(token) {
        return Some("iso8601");
    }
    if is_hex_hash(token) {
        return Some("hex_hash");
    }
    None
}

fn split_tokens(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    content
        .split_whitespace()
        .map(|raw| {
            let cleaned = raw.trim_matches(|c: char| {
                matches!(
                    c,
                    '.' | ','
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | '"'
                        | '\''
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                )
            });
            cleaned
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Detect volatile/dynamic content in arbitrary text.
///
/// Pure detection: no regex, no mutation. Returns one finding per token
/// that matches any structural pattern.
pub fn detect_volatile_content(content: &str) -> Vec<VolatileFinding> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut findings = Vec::new();
    for token in split_tokens(content) {
        if let Some(label) = classify_token(token) {
            let sample = if token.len() <= 16 {
                token.to_string()
            } else {
                format!("{}...{}", &token[..8], &token[token.len() - 4..])
            };
            findings.push(VolatileFinding { label, sample });
        }
    }
    findings
}

// ─── Hash utility ────────────────────────────────────────────────────────

/// SHA256 truncated to 16 hex chars. Matches Python `compute_short_hash`.
fn compute_short_hash(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)[..16].to_string()
}

// ─── CacheAligner ────────────────────────────────────────────────────────

/// Cache alignment detector. Never mutates messages.
pub struct CacheAligner {
    config: CacheAlignerConfig,
}

impl CacheAligner {
    pub fn new(config: CacheAlignerConfig) -> Self {
        Self { config }
    }

    /// Return true iff detection is enabled and a system message exists.
    ///
    /// If `cache_aligner_enabled` is explicitly set in the caller's context,
    /// it overrides the config-level `enabled` flag (Phase F / PR-F2.1).
    pub fn should_apply(
        &self,
        messages: &[serde_json::Value],
        cache_aligner_enabled: Option<bool>,
    ) -> bool {
        let enabled = cache_aligner_enabled.unwrap_or(self.config.enabled);
        if !enabled {
            return false;
        }
        messages.iter().any(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("system")
                && m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
        })
    }

    /// Compute cache alignment score (0-100). Higher = fewer volatile patterns.
    pub fn get_alignment_score(&self, messages: &[serde_json::Value]) -> f64 {
        let mut score = 100.0f64;
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
                continue;
            }
            let content = match msg.get("content").and_then(|c| c.as_str()) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let findings = detect_volatile_content(content);
            score -= findings.len() as f64 * 10.0;
        }
        score.clamp(0.0, 100.0)
    }

    /// Run detection, compute cache metrics, return result.
    ///
    /// Messages are deep-copied and returned unchanged (detector-only).
    /// If `frozen_message_count` is provided, messages with index <
    /// `frozen_message_count` are skipped (they are in the frozen prefix).
    pub fn apply(
        &self,
        messages: &[serde_json::Value],
        tokenizer: &dyn Tokenizer,
        state: &mut CacheAlignerState,
        frozen_message_count: Option<usize>,
    ) -> CacheAlignerResult {
        let frozen = frozen_message_count.unwrap_or(0);
        let result_messages: Vec<serde_json::Value> = messages.iter().map(|m| m.clone()).collect();

        let tokens_before: usize = result_messages
            .iter()
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
            .map(|s| tokenizer.count_text(s))
            .sum();

        let mut warnings = Vec::new();
        let mut all_findings: Vec<VolatileFinding> = Vec::new();

        for (idx, msg) in result_messages.iter().enumerate() {
            if idx < frozen {
                continue;
            }
            if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
                continue;
            }
            let content = match msg.get("content").and_then(|c| c.as_str()) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let findings = detect_volatile_content(content);
            all_findings.extend(findings);
        }

        if !all_findings.is_empty() {
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for f in &all_findings {
                *counts.entry(f.label).or_insert(0) += 1;
            }
            let counts_str: Vec<String> =
                counts.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            let msg_text = format!(
                "CacheAligner: detected volatile content in system prompt ({}); \
                 cache prefix unstable. Move dynamic values out of the system \
                 prompt to recover cache hits.",
                counts_str.join(", ")
            );
            warnings.push(msg_text);
        }

        // Compute stable hash of all system messages
        let system_text: String = result_messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("system")
                    && m.get("content").and_then(|c| c.as_str()).is_some()
            })
            .map(|m| m.get("content").and_then(|c| c.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n---\n");

        let stable_hash = compute_short_hash(&system_text);
        let prefix_bytes = system_text.len();
        let prefix_tokens_est: usize = tokenizer.count_text(&system_text);
        let prefix_changed = state
            .previous_prefix_hash
            .as_deref()
            .map(|prev| prev != stable_hash)
            .unwrap_or(false);
        let previous_hash = state.previous_prefix_hash.clone();
        state.previous_prefix_hash = Some(stable_hash.clone());

        let cache_metrics = CachePrefixMetrics {
            stable_prefix_bytes: prefix_bytes,
            stable_prefix_tokens_est: prefix_tokens_est,
            stable_prefix_hash: stable_hash.clone(),
            prefix_changed,
            previous_hash,
        };

        let tokens_after: usize = result_messages
            .iter()
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
            .map(|s| tokenizer.count_text(s))
            .sum();

        CacheAlignerResult {
            messages: result_messages,
            tokens_before,
            tokens_after,
            transforms_applied: vec![],
            markers_inserted: vec![format!("stable_prefix_hash:{}", stable_hash)],
            warnings,
            cache_metrics: Some(cache_metrics),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::EstimatingCounter;

    fn estimating_tokenizer() -> impl crate::tokenizer::Tokenizer {
        EstimatingCounter::new(4.0)
    }

    fn system_user_messages(system_text: &str) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"role": "system", "content": system_text}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ]
    }

    #[test]
    fn detect_uuid() {
        let findings = detect_volatile_content("Session: 550e8400-e29b-41d4-a716-446655440000");
        assert!(findings.iter().any(|f| f.label == "uuid"));
    }

    #[test]
    fn detect_iso8601() {
        let findings = detect_volatile_content("Now: 2024-01-15T10:30:00");
        assert!(findings.iter().any(|f| f.label == "iso8601"));
    }

    #[test]
    fn detect_jwt() {
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let findings = detect_volatile_content(&format!("Token: {}", jwt));
        assert!(findings.iter().any(|f| f.label == "jwt"));
    }

    #[test]
    fn detect_hex_hash() {
        let findings = detect_volatile_content("Hash: d41d8cd98f00b204e9800998ecf8427e");
        assert!(findings.iter().any(|f| f.label == "hex_hash"));
    }

    #[test]
    fn no_findings_on_normal_prose() {
        let findings = detect_volatile_content("You are a helpful assistant. Be polite.");
        assert!(findings.is_empty());
    }

    #[test]
    fn sample_truncation() {
        let long_token = "0123456789abcdef0123456789abcdef";
        let findings = detect_volatile_content(&format!("hash: {}", long_token));
        assert!(!findings.is_empty());
        assert!(findings[0].sample.contains("..."));
    }

    #[test]
    fn should_apply_false_when_disabled() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: false });
        let msgs = system_user_messages("Session: 550e8400-e29b-41d4-a716-446655440000");
        assert!(!aligner.should_apply(&msgs, None));
    }

    #[test]
    fn should_apply_true_when_enabled() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let msgs = system_user_messages("Session: 550e8400-e29b-41d4-a716-446655440000");
        assert!(aligner.should_apply(&msgs, None));
    }

    #[test]
    fn should_apply_false_without_system_message() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello"})];
        assert!(!aligner.should_apply(&msgs, None));
    }

    #[test]
    fn should_apply_override_from_policy() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let msgs = system_user_messages("Session: 550e8400-e29b-41d4-a716-446655440000");
        // Policy disables aligner even though config says enabled
        assert!(!aligner.should_apply(&msgs, Some(false)));
        // Policy enables aligner even though config says disabled
        let aligner_disabled = CacheAligner::new(CacheAlignerConfig { enabled: false });
        assert!(aligner_disabled.should_apply(&msgs, Some(true)));
    }

    #[test]
    fn apply_never_mutates_input() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();
        let msgs = system_user_messages("Session: 550e8400-e29b-41d4-a716-446655440000");
        let snapshot: Vec<serde_json::Value> = msgs.iter().map(|m| m.clone()).collect();
        let _result = aligner.apply(&msgs, &tok, &mut state, None);
        assert_eq!(msgs, snapshot);
    }

    #[test]
    fn apply_emits_warning_for_volatile_content() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();
        let msgs = system_user_messages("Session: 550e8400-e29b-41d4-a716-446655440000");
        let result = aligner.apply(&msgs, &tok, &mut state, None);
        assert!(!result.warnings.is_empty());
        assert!(result.warnings[0].contains("volatile content"));
    }

    #[test]
    fn apply_no_warning_for_clean_content() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();
        let msgs = system_user_messages("You are a helpful assistant.");
        let result = aligner.apply(&msgs, &tok, &mut state, None);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn apply_computes_cache_metrics() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();
        let msgs = system_user_messages("You are a helpful assistant.");
        let result = aligner.apply(&msgs, &tok, &mut state, None);
        let metrics = result.cache_metrics.unwrap();
        assert!(!metrics.stable_prefix_hash.is_empty());
        assert_eq!(metrics.stable_prefix_hash.len(), 16);
        assert!(metrics.stable_prefix_bytes > 0);
    }

    #[test]
    fn apply_detects_prefix_change() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();

        let msgs1 = system_user_messages("System prompt v1");
        let r1 = aligner.apply(&msgs1, &tok, &mut state, None);
        assert!(!r1.cache_metrics.as_ref().unwrap().prefix_changed);

        let msgs2 = system_user_messages("System prompt v2");
        let r2 = aligner.apply(&msgs2, &tok, &mut state, None);
        assert!(r2.cache_metrics.as_ref().unwrap().prefix_changed);
    }

    #[test]
    fn apply_markers_include_hash() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();
        let msgs = system_user_messages("System prompt");
        let result = aligner.apply(&msgs, &tok, &mut state, None);
        assert_eq!(result.markers_inserted.len(), 1);
        assert!(result.markers_inserted[0].starts_with("stable_prefix_hash:"));
    }

    #[test]
    fn apply_skips_frozen_messages() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let mut state = CacheAlignerState::default();
        let tok = estimating_tokenizer();
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "Session: 550e8400-e29b-41d4-a716-446655440000"}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        // frozen_message_count=1 skips the first message (system with volatile content)
        let result = aligner.apply(&msgs, &tok, &mut state, Some(1));
        assert!(
            result.warnings.is_empty(),
            "frozen message should be skipped"
        );
    }

    #[test]
    fn alignment_score_perfect_when_clean() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let score = aligner.get_alignment_score(&[
            serde_json::json!({"role": "system", "content": "You are helpful."}),
        ]);
        assert_eq!(score, 100.0);
    }

    #[test]
    fn alignment_score_decreases_with_findings() {
        let aligner = CacheAligner::new(CacheAlignerConfig { enabled: true });
        let score = aligner.get_alignment_score(
            &[serde_json::json!({
                "role": "system",
                "content": "Session: 550e8400-e29b-41d4-a716-446655440000\nHash: d41d8cd98f00b204e9800998ecf8427e"
            })],
        );
        assert!(score < 100.0);
    }
}
