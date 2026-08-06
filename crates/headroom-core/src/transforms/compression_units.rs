//! Provider-neutral compression units.
//!
//! Provider adapters own request-envelope details and cache/live-zone decisions.
//! They extract only safe, mutable text ranges into `CompressionUnit` objects,
//! ask ContentRouter to compress each unit, then splice accepted replacements
//! back into their native request shape.
//!
//! This module provides the data model and pure helper functions. The
//! `compress_unit_with_router` function (which depends on ContentRouter) lives
//! in the provider adapter layer.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::transforms::content_router::{CompressionStrategy, RouterCompressionResult};

/// One provider-extracted, cache-safe text slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionUnit {
    pub text: String,
    pub provider: String,
    pub endpoint: String,
    pub role: String,
    pub item_type: String,
    #[serde(default = "default_cache_zone")]
    pub cache_zone: String,
    #[serde(default = "default_true")]
    pub mutable: bool,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default = "default_bias")]
    pub bias: f64,
    #[serde(default = "default_min_bytes")]
    pub min_bytes: usize,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

fn default_cache_zone() -> String {
    "live".to_string()
}

fn default_true() -> bool {
    true
}

fn default_bias() -> f64 {
    1.0
}

fn default_min_bytes() -> usize {
    512
}

/// Categorical buckets for unit-level outcomes.
pub fn categorize_reason(reason: Option<&str>) -> String {
    match reason {
        None => "applied".to_string(),
        Some(r) => {
            if let Some(cat) = get_unit_reason_categories().get(r) {
                cat.to_string()
            } else if r.starts_with("cache_zone_") {
                "cache_zone".to_string()
            } else {
                "other".to_string()
            }
        }
    }
}

/// Static mapping from reason strings to category buckets.
pub fn unit_reason_categories() -> &'static HashMap<&'static str, &'static str> {
    get_unit_reason_categories()
}

static UNIT_REASON_CATEGORIES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

fn get_unit_reason_categories() -> &'static HashMap<&'static str, &'static str> {
    UNIT_REASON_CATEGORIES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("protected_user_message", "protected_role");
        m.insert("protected_system_message", "protected_role");
        m.insert("protected_assistant_message", "protected_role");
        m.insert("immutable", "immutable");
        m.insert("below_unit_floor", "size_floor");
        m.insert("router_no_change", "compressor_noop");
        m.insert("already_compressed", "already_compressed");
        m.insert("rejected_not_smaller", "rejected_not_smaller");
        m
    })
}

/// Result of compressing a single unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitCompressionResult {
    pub original: String,
    pub compressed: String,
    pub modified: bool,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub tokens_saved: usize,
    pub transforms_applied: Vec<String>,
    pub strategy: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub text_bytes: usize,
    #[serde(default)]
    pub min_bytes: usize,
    #[serde(default = "default_applied_category")]
    pub reason_category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router_result: Option<RouterCompressionResult>,
}

/// Abstraction over a compression router. ContentRouter implements this
/// natively; tests inject a mock.
pub trait Compressor {
    fn compress(
        &self,
        content: &str,
        context: &str,
        question: Option<&str>,
        bias: f64,
    ) -> RouterCompressionResult;
}

/// Abstraction over a tokenizer for token counting. Tests inject a mock.
pub trait TokenCounter {
    fn count_text(&self, text: &str) -> usize;
}

/// Compress marker-free text by stripping whitespace, checking min_bytes,
/// and delegating to the compressor.
fn compress_marker_free_text(
    text: &str,
    unit: &CompressionUnit,
    compressor: &dyn Compressor,
    last_router_result: Option<RouterCompressionResult>,
) -> (String, Vec<String>, Option<RouterCompressionResult>) {
    // Strip leading/trailing whitespace, preserving the boundaries
    let boundary_re = get_boundary_re();
    let caps = match boundary_re.captures(text) {
        Some(c) => c,
        None => return (text.to_string(), vec![], last_router_result),
    };

    let leading = caps.get(1).map_or("", |m| m.as_str());
    let core = caps.get(2).map_or("", |m| m.as_str());
    let trailing = caps.get(3).map_or("", |m| m.as_str());

    if core.len() < unit.min_bytes {
        return (text.to_string(), vec![], last_router_result);
    }

    let router_result =
        compressor.compress(core, &unit.context, unit.question.as_deref(), unit.bias);
    if router_result.compressed == core {
        return (text.to_string(), vec![], Some(router_result));
    }

    let strategy = router_result.strategy_used.as_str().to_string();
    let transforms = vec![
        format!(
            "router:{}:{}:{}:{}",
            unit.provider, unit.endpoint, unit.item_type, strategy
        ),
        strategy,
    ];
    (
        format!("{}{}{}", leading, router_result.compressed, trailing),
        transforms,
        Some(router_result),
    )
}

fn get_boundary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)^(\s*)(.*?)(\s*)$").unwrap())
}

/// Compress text around CCR markers while preserving marker bytes.
fn compress_live_text_with_markers(
    unit: &CompressionUnit,
    compressor: &dyn Compressor,
) -> (String, Vec<String>, Option<RouterCompressionResult>) {
    let mut parts = Vec::new();
    let mut transforms = Vec::new();
    let mut last_end = 0;
    let mut last_router_result: Option<RouterCompressionResult> = None;
    let re = ccr_marker_re();

    for mat in re.find_iter(&unit.text) {
        let prefix = &unit.text[last_end..mat.start()];
        if !prefix.is_empty() {
            let (compressed, prefix_transforms, result) =
                compress_marker_free_text(prefix, unit, compressor, last_router_result);
            parts.push(compressed);
            transforms.extend(prefix_transforms);
            last_router_result = result;
        }
        parts.push(mat.as_str().to_string());
        last_end = mat.end();
    }

    let suffix = &unit.text[last_end..];
    if !suffix.is_empty() {
        let (compressed, suffix_transforms, result) =
            compress_marker_free_text(suffix, unit, compressor, last_router_result);
        parts.push(compressed);
        transforms.extend(suffix_transforms);
        last_router_result = result;
    }

    if !transforms.is_empty() {
        transforms.insert(0, "ccr_marker_preserving".to_string());
    }

    (parts.concat(), transforms, last_router_result)
}

fn default_applied_category() -> String {
    "applied".to_string()
}

/// A unit paired with its provider-owned slot reference.
#[derive(Debug, Clone)]
pub struct RoutedCompressionUnit {
    pub unit: CompressionUnit,
    /// Opaque slot reference — only the provider adapter knows how to splice it.
    pub slot: serde_json::Value,
}

/// Regex matching CCR retrieval markers in text.
pub fn ccr_marker_re() -> &'static Regex {
    get_ccr_marker_re()
}

static CCR_MARKER_RE: OnceLock<Regex> = OnceLock::new();

fn get_ccr_marker_re() -> &'static Regex {
    CCR_MARKER_RE.get_or_init(|| {
        Regex::new(r"(?m)^.*(?:Retrieve more: hash=|Retrieve original: hash=|<<ccr:[^>]+>>).*$")
            .unwrap()
    })
}

/// Lossy strategies that don't produce CCR markers (unrecoverable if not marked).
pub fn lossy_unmarked_strategies() -> &'static std::collections::HashSet<&'static str> {
    get_lossy_unmarked_strategies()
}

static LOSSY_UNMARKED_STRATEGIES: OnceLock<std::collections::HashSet<&'static str>> =
    OnceLock::new();

fn get_lossy_unmarked_strategies() -> &'static std::collections::HashSet<&'static str> {
    LOSSY_UNMARKED_STRATEGIES.get_or_init(|| {
        let mut s = std::collections::HashSet::new();
        s.insert(CompressionStrategy::Kompress.as_str());
        s.insert(CompressionStrategy::Text.as_str());
        s.insert(CompressionStrategy::CodeAware.as_str());
        s
    })
}

/// Check if text looks like structured shell output (3+ non-empty lines).
pub fn is_structured_shell_output(text: &str) -> bool {
    text.lines().filter(|l| !l.trim().is_empty()).count() >= 3
}

/// Compress one safe text unit through a Compressor.
///
/// The final accept/reject gate uses the provided tokenizer, not the
/// compressor's internal word-count estimates.
pub fn compress_unit_with_router(
    unit: &CompressionUnit,
    compressor: &dyn Compressor,
    tokenizer: &dyn TokenCounter,
) -> UnitCompressionResult {
    let tokens_before = tokenizer.count_text(&unit.text);
    let text_bytes = unit.text.len();
    let passthrough = CompressionStrategy::Passthrough.as_str().to_string();

    // ── Guard clauses ───────────────────────────────────────────────────
    // Each guard returns early with a reason; reason_category is derived automatically.
    macro_rules! guard {
        ($reason:expr) => {
            return UnitCompressionResult {
                original: unit.text.clone(),
                compressed: unit.text.clone(),
                modified: false,
                tokens_before,
                tokens_after: tokens_before,
                tokens_saved: 0,
                transforms_applied: vec![],
                strategy: passthrough.clone(),
                reason: Some($reason.to_string()),
                reason_category: categorize_reason(Some($reason)),
                text_bytes,
                min_bytes: unit.min_bytes,
                router_result: None,
            }
        };
    }

    if !unit.mutable {
        guard!("immutable");
    }
    if unit.role == "user" && unit.metadata.get("compress_user").map(|s| s.as_str()) != Some("true")
    {
        guard!("protected_user_message");
    }
    if unit.role == "system" || unit.role == "developer" {
        guard!("protected_system_message");
    }
    if unit.role == "assistant"
        && unit.metadata.get("compress_assistant").map(|s| s.as_str()) != Some("true")
    {
        guard!("protected_assistant_message");
    }
    if unit.cache_zone != "live" {
        guard!(&format!("cache_zone_{}", unit.cache_zone));
    }
    if unit.text.len() < unit.min_bytes {
        guard!("below_unit_floor");
    }

    // ── CCR marker-preserving path ──────────────────────────────────────
    if ccr_marker_re().is_match(&unit.text) {
        let (replacement, marker_transforms, router_result) =
            compress_live_text_with_markers(unit, compressor);

        if replacement == unit.text {
            return UnitCompressionResult {
                original: unit.text.clone(),
                compressed: unit.text.clone(),
                modified: false,
                tokens_before,
                tokens_after: tokens_before,
                tokens_saved: 0,
                transforms_applied: vec![],
                strategy: passthrough.clone(),
                reason: Some("already_compressed".to_string()),
                reason_category: categorize_reason(Some("already_compressed")),
                text_bytes,
                min_bytes: unit.min_bytes,
                router_result,
            };
        }

        let tokens_after = tokenizer.count_text(&replacement);
        if tokens_after >= tokens_before {
            return UnitCompressionResult {
                original: unit.text.clone(),
                compressed: replacement,
                modified: false,
                tokens_before,
                tokens_after,
                tokens_saved: 0,
                transforms_applied: vec![],
                strategy: "ccr_marker_preserving".to_string(),
                reason: Some("rejected_not_smaller".to_string()),
                reason_category: categorize_reason(Some("rejected_not_smaller")),
                text_bytes,
                min_bytes: unit.min_bytes,
                router_result,
            };
        }

        return UnitCompressionResult {
            original: unit.text.clone(),
            compressed: replacement,
            modified: true,
            tokens_before,
            tokens_after,
            tokens_saved: tokens_before - tokens_after,
            transforms_applied: marker_transforms,
            strategy: "ccr_marker_preserving".to_string(),
            reason: None,
            reason_category: "applied".to_string(),
            text_bytes,
            min_bytes: unit.min_bytes,
            router_result,
        };
    }

    // ── Standard compression path ───────────────────────────────────────
    let router_result = compressor.compress(
        &unit.text,
        &unit.context,
        unit.question.as_deref(),
        unit.bias,
    );
    let replacement = router_result.compressed.clone();
    let strategy = router_result.strategy_used.as_str().to_string();

    if replacement == unit.text {
        return UnitCompressionResult {
            original: unit.text.clone(),
            compressed: replacement,
            modified: false,
            tokens_before,
            tokens_after: tokens_before,
            tokens_saved: 0,
            transforms_applied: vec![],
            strategy: strategy.clone(),
            reason: Some("router_no_change".to_string()),
            reason_category: categorize_reason(Some("router_no_change")),
            text_bytes,
            min_bytes: unit.min_bytes,
            router_result: Some(router_result),
        };
    }

    let tokens_after = tokenizer.count_text(&replacement);
    if tokens_after >= tokens_before {
        return UnitCompressionResult {
            original: unit.text.clone(),
            compressed: replacement,
            modified: false,
            tokens_before,
            tokens_after,
            tokens_saved: 0,
            transforms_applied: vec![],
            strategy: strategy.clone(),
            reason: Some("rejected_not_smaller".to_string()),
            reason_category: categorize_reason(Some("rejected_not_smaller")),
            text_bytes,
            min_bytes: unit.min_bytes,
            router_result: Some(router_result),
        };
    }

    // ── Lossy unrecoverable tool output guard ────────────────────────────
    if unit.role == "tool"
        && unit.item_type == "local_shell_call_output"
        && is_structured_shell_output(&unit.text)
        && lossy_unmarked_strategies().contains(strategy.as_str())
    {
        if !ccr_marker_re().is_match(&replacement) {
            return UnitCompressionResult {
                original: unit.text.clone(),
                compressed: unit.text.clone(),
                modified: false,
                tokens_before,
                tokens_after,
                tokens_saved: 0,
                transforms_applied: vec![],
                strategy: strategy.clone(),
                reason: Some("lossy_unrecoverable_tool_output".to_string()),
                reason_category: categorize_reason(Some("lossy_unrecoverable_tool_output")),
                text_bytes,
                min_bytes: unit.min_bytes,
                router_result: Some(router_result),
            };
        }
    }

    // ── Success ──────────────────────────────────────────────────────────
    UnitCompressionResult {
        original: unit.text.clone(),
        compressed: replacement,
        modified: true,
        tokens_before,
        tokens_after,
        tokens_saved: tokens_before - tokens_after,
        transforms_applied: vec![
            format!(
                "router:{}:{}:{}:{}",
                unit.provider, unit.endpoint, unit.item_type, strategy
            ),
            strategy.clone(),
        ],
        strategy,
        reason: None,
        reason_category: "applied".to_string(),
        text_bytes,
        min_bytes: unit.min_bytes,
        router_result: Some(router_result),
    }
}

/// Compress provider-extracted units and preserve provider slot refs.
pub fn compress_units_with_router(
    units: &[RoutedCompressionUnit],
    compressor: &dyn Compressor,
    tokenizer: &dyn TokenCounter,
) -> Vec<(serde_json::Value, UnitCompressionResult)> {
    units
        .iter()
        .map(|routed| {
            let result = compress_unit_with_router(&routed.unit, compressor, tokenizer);
            (routed.slot.clone(), result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── CompressionUnit defaults ─────────────────────────────────────────

    #[test]
    fn compression_unit_defaults() {
        let unit = CompressionUnit {
            text: "hello".to_string(),
            provider: "openai".to_string(),
            endpoint: "responses".to_string(),
            role: "tool".to_string(),
            item_type: "function_call_output".to_string(),
            cache_zone: default_cache_zone(),
            mutable: default_true(),
            context: String::new(),
            question: None,
            bias: default_bias(),
            min_bytes: default_min_bytes(),
            metadata: HashMap::new(),
        };
        assert_eq!(unit.cache_zone, "live");
        assert!(unit.mutable);
        assert_eq!(unit.bias, 1.0);
        assert_eq!(unit.min_bytes, 512);
    }

    #[test]
    fn compression_unit_serialization_roundtrip() {
        let unit = CompressionUnit {
            text: "test content".to_string(),
            provider: "anthropic".to_string(),
            endpoint: "messages".to_string(),
            role: "tool".to_string(),
            item_type: "tool_result".to_string(),
            cache_zone: "prefix".to_string(),
            mutable: false,
            context: "ctx".to_string(),
            question: Some("q".to_string()),
            bias: 0.8,
            min_bytes: 256,
            metadata: HashMap::new(),
        };
        let json_str = serde_json::to_string(&unit).unwrap();
        let decoded: CompressionUnit = serde_json::from_str(&json_str).unwrap();
        assert_eq!(decoded.text, "test content");
        assert_eq!(decoded.cache_zone, "prefix");
        assert!(!decoded.mutable);
        assert_eq!(decoded.bias, 0.8);
    }

    // ── categorize_reason ────────────────────────────────────────────────

    #[test]
    fn categorize_reason_applied() {
        assert_eq!(categorize_reason(None), "applied");
    }

    #[test]
    fn categorize_reason_protected_roles() {
        assert_eq!(
            categorize_reason(Some("protected_user_message")),
            "protected_role"
        );
        assert_eq!(
            categorize_reason(Some("protected_system_message")),
            "protected_role"
        );
        assert_eq!(
            categorize_reason(Some("protected_assistant_message")),
            "protected_role"
        );
    }

    #[test]
    fn categorize_reason_immutable() {
        assert_eq!(categorize_reason(Some("immutable")), "immutable");
    }

    #[test]
    fn categorize_reason_size_floor() {
        assert_eq!(categorize_reason(Some("below_unit_floor")), "size_floor");
    }

    #[test]
    fn categorize_reason_cache_zone_dynamic() {
        assert_eq!(categorize_reason(Some("cache_zone_frozen")), "cache_zone");
        assert_eq!(categorize_reason(Some("cache_zone_prefix")), "cache_zone");
    }

    #[test]
    fn categorize_reason_other() {
        assert_eq!(categorize_reason(Some("some_unknown_reason")), "other");
    }

    // ── is_structured_shell_output ───────────────────────────────────────

    #[test]
    fn structured_shell_output() {
        assert!(is_structured_shell_output("line1\nline2\nline3"));
        assert!(is_structured_shell_output("line1\n\nline2\nline3\nline4"));
    }

    #[test]
    fn not_structured_shell_output() {
        assert!(!is_structured_shell_output("single line"));
        assert!(!is_structured_shell_output("line1\nline2"));
        assert!(!is_structured_shell_output(""));
    }

    // ── CCR marker regex ────────────────────────────────────────────────

    #[test]
    fn ccr_marker_detects_retrieve_more() {
        let text = "[100 items compressed to 10. Retrieve more: hash=abc123]";
        assert!(ccr_marker_re().is_match(text));
    }

    #[test]
    fn ccr_marker_detects_retrieve_original() {
        let text = "[Read content stale: /x/foo.py ... Retrieve original: hash=abc123]";
        assert!(ccr_marker_re().is_match(text));
    }

    #[test]
    fn ccr_marker_detects_ccr_tag() {
        let text = "some text <<ccr:abc123def456>> more text";
        assert!(ccr_marker_re().is_match(text));
    }

    #[test]
    fn ccr_marker_no_match_on_plain_text() {
        let text = "this is just regular text with no markers";
        assert!(!ccr_marker_re().is_match(text));
    }

    // ── UnitCompressionResult ────────────────────────────────────────────

    #[test]
    fn unit_compression_result_defaults() {
        let result = UnitCompressionResult {
            original: "a".to_string(),
            compressed: "b".to_string(),
            modified: true,
            tokens_before: 10,
            tokens_after: 5,
            tokens_saved: 5,
            transforms_applied: vec!["test".to_string()],
            strategy: "kompress".to_string(),
            reason: None,
            text_bytes: 100,
            min_bytes: 512,
            reason_category: default_applied_category(),
            router_result: None,
        };
        assert_eq!(result.reason_category, "applied");
    }

    #[test]
    fn unit_compression_result_with_reason() {
        let result = UnitCompressionResult {
            original: "a".to_string(),
            compressed: "a".to_string(),
            modified: false,
            tokens_before: 10,
            tokens_after: 10,
            tokens_saved: 0,
            transforms_applied: vec![],
            strategy: "passthrough".to_string(),
            reason: Some("immutable".to_string()),
            text_bytes: 100,
            min_bytes: 512,
            reason_category: categorize_reason(Some("immutable")),
            router_result: None,
        };
        assert_eq!(result.reason_category, "immutable");
        assert!(!result.modified);
    }

    // ── Lossy unmarked strategies ────────────────────────────────────────

    #[test]
    fn lossy_unmarked_strategies_set() {
        let strategies = lossy_unmarked_strategies();
        assert!(strategies.contains("kompress"));
        assert!(strategies.contains("text"));
        assert!(strategies.contains("code_aware"));
        assert!(!strategies.contains("search"));
        assert!(!strategies.contains("smart_crusher"));
    }

    // ── compress_unit_with_router (ported from Python) ───────────────────

    /// Mock compressor that always returns a fixed replacement.
    struct MockCompressor {
        compressed: String,
    }

    impl Compressor for MockCompressor {
        fn compress(
            &self,
            _content: &str,
            _context: &str,
            _question: Option<&str>,
            _bias: f64,
        ) -> RouterCompressionResult {
            RouterCompressionResult {
                compressed: self.compressed.clone(),
                original: String::new(),
                strategy_used: CompressionStrategy::Kompress,
                routing_log: vec![],
                sections_processed: 1,
                strategy_chain: vec![],
                cache_hit: false,
            }
        }
    }

    /// Mock tokenizer that counts words.
    struct WordCounter;

    impl TokenCounter for WordCounter {
        fn count_text(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
    }

    fn make_unit(text: &str, role: &str, item_type: &str) -> CompressionUnit {
        CompressionUnit {
            text: text.to_string(),
            provider: "openai".to_string(),
            endpoint: "responses".to_string(),
            role: role.to_string(),
            item_type: item_type.to_string(),
            cache_zone: "live".to_string(),
            mutable: true,
            context: String::new(),
            question: None,
            bias: 1.0,
            min_bytes: 1,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn accepts_token_shrinking_replacement() {
        let unit = CompressionUnit {
            text: "alpha beta gamma delta epsilon".to_string(),
            role: "assistant".to_string(),
            metadata: HashMap::from([("compress_assistant".to_string(), "true".to_string())]),
            ..make_unit("", "assistant", "message")
        };
        let compressor = MockCompressor {
            compressed: "alpha beta".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(result.modified);
        assert_eq!(result.tokens_saved, 3);
        assert_eq!(result.compressed, "alpha beta");
        assert!(result
            .transforms_applied
            .iter()
            .any(|t| t.contains("kompress")));
    }

    #[test]
    fn keeps_lossy_unmarked_tool_output_verbatim() {
        let original = "src/app.py:12 render shell status panel\nsrc/ui.py:44 draw health badge\nsrc/theme.py:9 set accent color";
        let unit = CompressionUnit {
            text: original.to_string(),
            role: "tool".to_string(),
            item_type: "local_shell_call_output".to_string(),
            ..make_unit("", "tool", "local_shell_call_output")
        };
        let compressor = MockCompressor {
            compressed: "shell output looks organized and green".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(!result.modified);
        assert_eq!(
            result.reason.as_deref(),
            Some("lossy_unrecoverable_tool_output")
        );
        assert_eq!(result.original, original);
        assert_eq!(result.compressed, original);
    }

    #[test]
    fn accepts_lossy_tool_output_when_recoverable() {
        let original = "alpha beta gamma delta epsilon zeta eta theta";
        let unit = CompressionUnit {
            text: original.to_string(),
            role: "tool".to_string(),
            item_type: "local_shell_call_output".to_string(),
            ..make_unit("", "tool", "local_shell_call_output")
        };
        let compressor = MockCompressor {
            compressed: "summary <<ccr:abc123>>".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(result.modified);
        assert!(result.reason.is_none());
        assert_eq!(result.compressed, "summary <<ccr:abc123>>");
    }

    #[test]
    fn still_compresses_non_shell_tool_output() {
        let unit = CompressionUnit {
            text: "alpha beta gamma delta epsilon zeta eta theta".to_string(),
            role: "tool".to_string(),
            item_type: "function_call_output".to_string(),
            ..make_unit("", "tool", "function_call_output")
        };
        let compressor = MockCompressor {
            compressed: "summary for tool=0".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(result.modified);
        assert!(result.reason.is_none());
        assert_eq!(result.compressed, "summary for tool=0");
    }

    #[test]
    fn still_compresses_assistant_text() {
        let unit = CompressionUnit {
            text: "alpha beta gamma delta epsilon".to_string(),
            role: "assistant".to_string(),
            item_type: "message".to_string(),
            metadata: HashMap::from([("compress_assistant".to_string(), "true".to_string())]),
            ..make_unit("", "assistant", "message")
        };
        let compressor = MockCompressor {
            compressed: "alpha beta".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(result.modified);
        assert!(result.reason.is_none());
        assert_eq!(result.compressed, "alpha beta");
    }

    #[test]
    fn compresses_user_text_when_opted_in() {
        // Ports test_user_text_block_compresses_when_user_messages_are_enabled:
        // user text honors the compress-user opt-in instead of falling
        // through to the default protection branch.
        let unit = CompressionUnit {
            text: "alpha beta gamma delta epsilon".to_string(),
            role: "user".to_string(),
            item_type: "message".to_string(),
            metadata: HashMap::from([("compress_user".to_string(), "true".to_string())]),
            ..make_unit("", "user", "message")
        };
        let compressor = MockCompressor {
            compressed: "alpha beta".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(result.modified);
        assert!(result.reason.is_none());
        assert_eq!(result.compressed, "alpha beta");
    }

    #[test]
    fn user_text_protected_by_default() {
        // Ports test_user_text_block_stays_protected_by_default: without
        // the opt-in, user text stays verbatim with the protection reason.
        let unit = make_unit("alpha beta gamma delta epsilon", "user", "message");
        let compressor = MockCompressor {
            compressed: "alpha beta".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(!result.modified);
        assert_eq!(result.reason.as_deref(), Some("protected_user_message"));
        assert_eq!(result.compressed, "alpha beta gamma delta epsilon");
    }

    #[test]
    fn user_cache_control_text_protected_even_when_opted_in() {
        // Ports test_user_cache_control_text_block_stays_protected_when_enabled:
        // a cache breakpoint (immutable unit) stays protected even with the
        // compress-user opt-in set — the immutable guard wins.
        let unit = CompressionUnit {
            text: "alpha beta gamma delta epsilon".to_string(),
            role: "user".to_string(),
            item_type: "message".to_string(),
            mutable: false,
            metadata: HashMap::from([("compress_user".to_string(), "true".to_string())]),
            ..make_unit("", "user", "message")
        };
        let compressor = MockCompressor {
            compressed: "alpha beta".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(!result.modified);
        assert_eq!(result.reason.as_deref(), Some("immutable"));
        assert_eq!(result.compressed, "alpha beta gamma delta epsilon");
    }

    #[test]
    fn rejects_non_shrinking_replacement() {
        let unit = make_unit("alpha beta", "tool", "tool_result");
        let compressor = MockCompressor {
            compressed: "alpha beta gamma".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(!result.modified);
        assert_eq!(result.reason.as_deref(), Some("rejected_not_smaller"));
        assert_eq!(result.original, "alpha beta");
    }

    #[test]
    fn respects_cache_zone_and_floor() {
        let frozen_unit = CompressionUnit {
            text: "alpha beta gamma delta".to_string(),
            cache_zone: "frozen".to_string(),
            ..make_unit("alpha beta gamma delta", "tool", "tool_result")
        };
        let compressor = MockCompressor {
            compressed: "alpha".to_string(),
        };
        let frozen = compress_unit_with_router(&frozen_unit, &compressor, &WordCounter);

        let small_unit = CompressionUnit {
            text: "small text".to_string(),
            min_bytes: 500,
            ..make_unit("small text", "tool", "function_call_output")
        };
        let small = compress_unit_with_router(&small_unit, &compressor, &WordCounter);

        assert!(!frozen.modified);
        assert_eq!(frozen.reason.as_deref(), Some("cache_zone_frozen"));
        assert!(!small.modified);
        assert_eq!(small.reason.as_deref(), Some("below_unit_floor"));
    }

    #[test]
    fn batch_preserves_provider_slot_references() {
        let routed = vec![
            RoutedCompressionUnit {
                unit: CompressionUnit {
                    text: "alpha beta gamma".to_string(),
                    role: "assistant".to_string(),
                    metadata: HashMap::from([(
                        "compress_assistant".to_string(),
                        "true".to_string(),
                    )]),
                    ..make_unit("alpha beta gamma", "assistant", "message")
                },
                slot: json!(["input", 3, "output"]),
            },
            RoutedCompressionUnit {
                unit: CompressionUnit {
                    text: "one two three".to_string(),
                    role: "user".to_string(),
                    ..make_unit("one two three", "user", "part.text")
                },
                slot: json!({"path": ["contents", 0, "parts", 0, "text"]}),
            },
        ];

        let compressor = MockCompressor {
            compressed: "short".to_string(),
        };
        let results = compress_units_with_router(&routed, &compressor, &WordCounter);

        assert_eq!(results[0].0, json!(["input", 3, "output"]));
        assert_eq!(
            results[1].0,
            json!({"path": ["contents", 0, "parts", 0, "text"]})
        );
        assert_eq!(
            results.iter().map(|(_, r)| r.modified).collect::<Vec<_>>(),
            vec![true, false]
        );
    }

    #[test]
    fn protects_prompt_roles() {
        let compressor = MockCompressor {
            compressed: "alpha".to_string(),
        };

        for (role, reason) in &[
            ("user", "protected_user_message"),
            ("developer", "protected_system_message"),
            ("system", "protected_system_message"),
            ("assistant", "protected_assistant_message"),
        ] {
            let unit = CompressionUnit {
                text: "alpha beta gamma delta".to_string(),
                role: role.to_string(),
                ..make_unit("alpha beta gamma delta", role, "message")
            };
            let result = compress_unit_with_router(&unit, &compressor, &WordCounter);
            assert!(!result.modified, "role={} should not be compressed", role);
            assert_eq!(result.reason.as_deref(), Some(*reason), "role={}", role);
        }
    }

    #[test]
    fn live_unit_with_retrieval_marker_compresses_surrounding_text() {
        let marker = "[100 items compressed to 10. Retrieve more: hash=abc123]";
        let text = format!(
            "alpha beta gamma delta epsilon\n{}\nzeta eta theta iota kappa",
            marker
        );

        let unit = CompressionUnit {
            text: text.clone(),
            role: "tool".to_string(),
            item_type: "function_call_output".to_string(),
            ..make_unit(&text, "tool", "function_call_output")
        };
        let compressor = MockCompressor {
            compressed: "short".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(result.modified);
        assert!(result.reason.is_none());
        assert_eq!(result.strategy, "ccr_marker_preserving");
        assert!(result.compressed.contains(marker));
        assert!(result.compressed.starts_with("short\n"));
        assert!(result.compressed.ends_with("\nshort"));
        assert!(result.tokens_saved > 0);
        assert!(result
            .transforms_applied
            .iter()
            .any(|t| t.contains("ccr_marker_preserving")));
    }

    #[test]
    fn non_live_unit_with_retrieval_marker_preserves_prefix_cache() {
        let marker = "[100 items compressed to 10. Retrieve more: hash=abc123]";
        let text = format!("alpha beta gamma delta epsilon\n{}\nzeta eta theta", marker);

        let unit = CompressionUnit {
            text: text.clone(),
            role: "tool".to_string(),
            item_type: "function_call_output".to_string(),
            cache_zone: "prefix".to_string(),
            ..make_unit(&text, "tool", "function_call_output")
        };
        let compressor = MockCompressor {
            compressed: "short".to_string(),
        };
        let result = compress_unit_with_router(&unit, &compressor, &WordCounter);

        assert!(!result.modified);
        assert_eq!(result.reason.as_deref(), Some("cache_zone_prefix"));
        assert_eq!(result.compressed, text);
    }
}
