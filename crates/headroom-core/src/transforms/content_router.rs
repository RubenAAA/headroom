//! Content router for intelligent compression strategy selection.
//!
//! Analyzes content and routes it to the optimal compressor. Handles mixed
//! content by splitting, routing each section, and reassembling.
//!
//! This module contains the pure-function helpers, data structures, and
//! the Rust-native dispatcher that calls into the core compressors directly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::compressor_registry::{
    CompressInput, CompressOutput, Compressor, CompressorDescriptor, CompressorRegistry,
};
use super::content_detector::ContentType;

// ─── Enums ───────────────────────────────────────────────────────────────

/// Available compression strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionStrategy {
    CodeAware,
    SmartCrusher,
    Search,
    Log,
    Kompress,
    Text,
    Diff,
    Html,
    Tabular,
    Config,
    Mixed,
    Passthrough,
}

impl CompressionStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CodeAware => "code_aware",
            Self::SmartCrusher => "smart_crusher",
            Self::Search => "search",
            Self::Log => "log",
            Self::Kompress => "kompress",
            Self::Text => "text",
            Self::Diff => "diff",
            Self::Html => "html",
            Self::Tabular => "tabular",
            Self::Config => "config",
            Self::Mixed => "mixed",
            Self::Passthrough => "passthrough",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "code_aware" => Some(Self::CodeAware),
            "smart_crusher" => Some(Self::SmartCrusher),
            "search" => Some(Self::Search),
            "log" => Some(Self::Log),
            "kompress" => Some(Self::Kompress),
            "text" => Some(Self::Text),
            "diff" => Some(Self::Diff),
            "html" => Some(Self::Html),
            "tabular" => Some(Self::Tabular),
            "config" => Some(Self::Config),
            "mixed" => Some(Self::Mixed),
            "passthrough" => Some(Self::Passthrough),
            _ => None,
        }
    }
}

// ─── Savings Profiles ────────────────────────────────────────────────────

/// Named compression profiles that configure the router for different
/// use cases. Each profile sets target_ratio, compress_user/system,
/// protect_recent, and force_kompress to match the profile's goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SavingsProfile {
    /// Aggressive: 90% savings target, compress everything, force Kompress.
    Agent90,
    /// Balanced: 70% savings, skip user/system messages, protect recent code.
    Balanced,
    /// Coding-focused: 50% savings, conservative, protect recent code.
    Coding,
    /// General: 60% savings, no message skipping, no recent protection.
    General,
}

impl SavingsProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Agent90 => "agent-90",
            Self::Balanced => "balanced",
            Self::Coding => "coding",
            Self::General => "general",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "agent-90" => Some(Self::Agent90),
            "balanced" => Some(Self::Balanced),
            "coding" => Some(Self::Coding),
            "general" => Some(Self::General),
            _ => None,
        }
    }

    /// Apply this profile's settings to a ContentRouterConfig.
    pub fn apply_to(self, config: &mut ContentRouterConfig) {
        match self {
            Self::Agent90 => {
                config.target_ratio = Some(0.10);
                config.compress_user_messages = Some(true);
                config.compress_system_messages = Some(true);
                config.protect_recent_code = 2;
                config.force_kompress_all = true;
            }
            Self::Balanced => {
                config.target_ratio = Some(0.30);
                config.compress_user_messages = Some(false);
                config.compress_system_messages = Some(false);
                config.protect_recent_code = 4;
                config.force_kompress_all = false;
            }
            Self::Coding => {
                config.target_ratio = None;
                config.compress_user_messages = Some(false);
                config.compress_system_messages = Some(false);
                config.protect_recent_code = 2;
                config.force_kompress_all = false;
            }
            Self::General => {
                config.target_ratio = None;
                config.compress_user_messages = Some(false);
                config.compress_system_messages = Some(false);
                config.protect_recent_code = 0;
                config.force_kompress_all = false;
            }
        }
    }
}

// ─── ToolSignature ───────────────────────────────────────────────────────

/// Anonymized signature of a tool's output structure for TOIN tracking.
///
/// Identifies similar tools across users without revealing tool names.
/// Two tools with the same field structure will have the same signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSignature {
    /// `SHA-256[:24]` of sorted field names + types.
    pub structure_hash: String,
    /// Number of top-level fields (0 for non-JSON).
    pub field_count: usize,
    /// Whether the output contains nested objects.
    pub has_nested_objects: bool,
    /// Whether the output contains arrays.
    pub has_arrays: bool,
    /// Maximum nesting depth (0 for non-JSON).
    pub max_depth: usize,
    pub string_field_count: usize,
    pub numeric_field_count: usize,
    pub boolean_field_count: usize,
    pub array_field_count: usize,
    pub object_field_count: usize,
    pub has_id_like_field: bool,
    pub has_score_like_field: bool,
    pub has_timestamp_like_field: bool,
    pub has_status_like_field: bool,
    pub has_error_like_field: bool,
    pub has_message_like_field: bool,
}

/// Per-field type tallies plus semantic pattern flags.
#[derive(Default)]
struct FieldCounts {
    string_count: usize,
    numeric_count: usize,
    boolean_count: usize,
    array_count: usize,
    object_count: usize,
    has_nested: bool,
    has_arrays: bool,
    has_id: bool,
    has_score: bool,
    has_timestamp: bool,
    has_status: bool,
    has_error: bool,
    has_message: bool,
}

/// Merged per-field observations plus the name-sorted pairs used for hashing.
struct MergedFields {
    merged: HashMap<String, Vec<String>>,
    sorted: Vec<(String, String)>,
}

impl ToolSignature {
    /// Create a signature for non-JSON content types (code, search, logs, text).
    /// The hash is deterministic and persists to disk.
    pub fn for_content_type(content_type: &str, content: &str, language: Option<&str>) -> Self {
        let structure_hash = create_content_signature(content_type, content, language);
        Self {
            structure_hash,
            field_count: 0,
            has_nested_objects: false,
            has_arrays: false,
            max_depth: 0,
            string_field_count: 0,
            numeric_field_count: 0,
            boolean_field_count: 0,
            array_field_count: 0,
            object_field_count: 0,
            has_id_like_field: false,
            has_score_like_field: false,
            has_timestamp_like_field: false,
            has_status_like_field: false,
            has_error_like_field: false,
            has_message_like_field: false,
        }
    }

    /// Create a signature from sample items (matching Python's `from_items`).
    ///
    /// Analyzes up to 5 items, merges field schemas, and computes a
    /// deterministic structure hash from sorted (field_name, field_type) pairs.
    pub fn from_items(items: &[Value]) -> Self {
        if items.is_empty() {
            // FINDING-013: fixed sentinel, not wall-clock — the hash is
            // documented deterministic ("persists to disk"), so every
            // empty output must map to one signature.
            let structure_hash = create_content_signature("empty", "", None);
            return Self {
                structure_hash,
                field_count: 0,
                has_nested_objects: false,
                has_arrays: false,
                max_depth: 0,
                string_field_count: 0,
                numeric_field_count: 0,
                boolean_field_count: 0,
                array_field_count: 0,
                object_field_count: 0,
                has_id_like_field: false,
                has_score_like_field: false,
                has_timestamp_like_field: false,
                has_status_like_field: false,
                has_error_like_field: false,
                has_message_like_field: false,
            };
        }

        let sample_items: Vec<&Value> = items.iter().take(5).collect();
        let max_depth = sample_items
            .iter()
            .map(|item| Self::calculate_depth(item))
            .max()
            .unwrap_or(1)
            .max(1);
        let field_info = Self::merge_field_types(&sample_items);
        let mut counts = FieldCounts::default();
        for (key, types) in &field_info.merged {
            Self::add_count(&mut counts, &Self::resolve_field_type(types), key);
        }
        let structure_hash = Self::hash_field_info(&field_info.sorted);

        Self {
            structure_hash,
            field_count: field_info.sorted.len(),
            has_nested_objects: counts.has_nested,
            has_arrays: counts.has_arrays,
            max_depth,
            string_field_count: counts.string_count,
            numeric_field_count: counts.numeric_count,
            boolean_field_count: counts.boolean_count,
            array_field_count: counts.array_count,
            object_field_count: counts.object_count,
            has_id_like_field: counts.has_id,
            has_score_like_field: counts.has_score,
            has_timestamp_like_field: counts.has_timestamp,
            has_status_like_field: counts.has_status,
            has_error_like_field: counts.has_error,
            has_message_like_field: counts.has_message,
        }
    }

    /// Merge per-field type observations across sampled items.
    ///
    /// Returns field names mapped to every observed type, plus the same pairs
    /// sorted by name for hashing.
    fn merge_field_types(sample_items: &[&Value]) -> MergedFields {
        // Merge field info from all sampled items
        let mut all_fields: HashMap<String, Vec<String>> = HashMap::new();
        for item in sample_items {
            if let Some(obj) = item.as_object() {
                for (key, value) in obj {
                    let type_name = match value {
                        Value::String(_) => "string",
                        Value::Bool(_) => "boolean",
                        Value::Number(_) => "numeric",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                        Value::Null => "null",
                    };
                    all_fields
                        .entry(key.clone())
                        .or_default()
                        .push(type_name.to_string());
                }
            }
        }

        // Build field_info with most common type per field
        let mut field_info: Vec<(String, String)> = Vec::new();
        for (key, types) in &all_fields {
            field_info.push((key.clone(), Self::resolve_field_type(types)));
        }

        // Create structure hash (matching Python's json.dumps(sorted_fields, sort_keys=True))
        field_info.sort_by(|a, b| a.0.cmp(&b.0));
        MergedFields {
            merged: all_fields,
            sorted: field_info,
        }
    }

    /// Resolve the dominant type for one field from its observations.
    ///
    /// Nulls are ignored; a single remaining type wins; multiple types resolve
    /// by priority (object > array > string > numeric > boolean); all-null
    /// falls back to the first observation.
    fn resolve_field_type(types: &[String]) -> String {
        let types_no_null: Vec<&str> = types
            .iter()
            .filter(|t| *t != "null")
            .map(|s| s.as_str())
            .collect();

        if types_no_null.len() == 1 {
            types_no_null[0].to_string()
        } else if !types_no_null.is_empty() {
            // Multiple types - pick by priority
            let mut found = "mixed".to_string();
            for t in &["object", "array", "string", "numeric", "boolean"] {
                if types_no_null.contains(t) {
                    found = t.to_string();
                    break;
                }
            }
            found
        } else {
            types.first().cloned().unwrap_or_else(|| "null".to_string())
        }
    }

    /// `SHA-256[:24]` of the sorted (field, type) pairs.
    fn hash_field_info(sorted_field_info: &[(String, String)]) -> String {
        let hash_input = serde_json::to_string(sorted_field_info).unwrap_or_default();
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(hash_input.as_bytes());
        hex::encode(hasher.finalize())[..24].to_string()
    }

    /// Fold one resolved (field, type) pair into running tallies and flags.
    fn add_count(counts: &mut FieldCounts, field_type: &str, key: &str) {
        match field_type {
            "string" => counts.string_count += 1,
            "boolean" => counts.boolean_count += 1,
            "numeric" => counts.numeric_count += 1,
            "array" => {
                counts.array_count += 1;
                counts.has_arrays = true;
            }
            "object" => {
                counts.object_count += 1;
                counts.has_nested = true;
            }
            _ => {}
        }

        // Pattern detection
        let key_lower = key.to_lowercase();
        if Self::matches_pattern(&key_lower, &["id", "uuid", "guid"]) || key_lower.ends_with("key")
        {
            counts.has_id = true;
        }
        if Self::matches_pattern(
            &key_lower,
            &["score", "rank", "rating", "relevance", "priority"],
        ) {
            counts.has_score = true;
        }
        if Self::matches_pattern(&key_lower, &["time", "date", "timestamp"])
            || key_lower.ends_with("_at")
            || key_lower == "created"
            || key_lower == "updated"
        {
            counts.has_timestamp = true;
        }
        if Self::matches_pattern(&key_lower, &["status", "state"])
            || key_lower == "level"
            || key_lower == "type"
            || key_lower == "kind"
        {
            counts.has_status = true;
        }
        if Self::matches_pattern(&key_lower, &["error", "exception", "fail", "warning"]) {
            counts.has_error = true;
        }
        if Self::matches_pattern(
            &key_lower,
            &["message", "msg", "text", "content", "body", "description"],
        ) {
            counts.has_message = true;
        }
    }

    fn calculate_depth(json: &Value) -> usize {
        match json {
            Value::Object(map) => {
                let inner = map.values().map(Self::calculate_depth).max().unwrap_or(0);
                1 + inner
            }
            Value::Array(arr) => {
                if let Some(first) = arr.first() {
                    1 + Self::calculate_depth(first)
                } else {
                    1
                }
            }
            _ => 0,
        }
    }

    /// Create a signature from a JSON value.
    pub fn from_json(json: &Value) -> Self {
        let (field_count, has_nested_objects, has_arrays, max_depth) = Self::analyze_json(json, 0);
        let structure_hash = Self::compute_json_hash(json);
        Self {
            structure_hash,
            field_count,
            has_nested_objects,
            has_arrays,
            max_depth,
            string_field_count: 0,
            numeric_field_count: 0,
            boolean_field_count: 0,
            array_field_count: 0,
            object_field_count: 0,
            has_id_like_field: false,
            has_score_like_field: false,
            has_timestamp_like_field: false,
            has_status_like_field: false,
            has_error_like_field: false,
            has_message_like_field: false,
        }
    }

    fn analyze_json(json: &Value, depth: usize) -> (usize, bool, bool, usize) {
        match json {
            Value::Object(map) => {
                let mut nested = false;
                let mut arrays = false;
                let mut max_d = depth;
                for v in map.values() {
                    match v {
                        Value::Object(_) => {
                            nested = true;
                            let (_, n, a, d) = Self::analyze_json(v, depth + 1);
                            if n {
                                nested = true;
                            }
                            if a {
                                arrays = true;
                            }
                            if d > max_d {
                                max_d = d;
                            }
                        }
                        Value::Array(arr) => {
                            arrays = true;
                            if let Some(first) = arr.first() {
                                let (_, n, a, d) = Self::analyze_json(first, depth + 1);
                                if n {
                                    nested = true;
                                }
                                if a {
                                    arrays = true;
                                }
                                if d > max_d {
                                    max_d = d;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                (map.len(), nested, arrays, max_d)
            }
            Value::Array(arr) => {
                let mut nested = false;
                let mut max_d = depth;
                if let Some(first) = arr.first() {
                    // The array flag the child reports is discarded on purpose:
                    // this node IS an array, so the answer is `true` whatever
                    // the child holds.
                    let (_, n, _, d) = Self::analyze_json(first, depth + 1);
                    nested = n;
                    max_d = d;
                }
                (0, nested, true, max_d)
            }
            _ => (0, false, false, depth),
        }
    }

    fn compute_json_hash(json: &Value) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_string(json).unwrap_or_default().as_bytes());
        hex::encode(hasher.finalize())[..24].to_string()
    }

    fn matches_pattern(key_lower: &str, patterns: &[&str]) -> bool {
        for pat in patterns {
            // Word boundary matching: key == pat, key starts with pat_, key ends with _pat
            if key_lower == *pat
                || key_lower.starts_with(&format!("{}_", pat))
                || key_lower.ends_with(&format!("_{}", pat))
            {
                return true;
            }
        }
        false
    }
}

// ─── Data structures ─────────────────────────────────────────────────────

/// Record of a single routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub content_type: ContentType,
    pub strategy: CompressionStrategy,
    pub original_tokens: usize,
    pub compressed_tokens: usize,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub section_index: usize,
}

fn default_confidence() -> f64 {
    1.0
}

impl RoutingDecision {
    pub fn compression_ratio(&self) -> f64 {
        if self.original_tokens == 0 {
            1.0
        } else {
            self.compressed_tokens as f64 / self.original_tokens as f64
        }
    }
}

/// A typed section of content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentSection {
    pub content: String,
    pub content_type: ContentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub start_line: usize,
    #[serde(default)]
    pub end_line: usize,
    #[serde(default)]
    pub is_code_fence: bool,
}

/// Result from ContentRouter with routing metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterCompressionResult {
    pub compressed: String,
    pub original: String,
    pub strategy_used: CompressionStrategy,
    #[serde(default)]
    pub routing_log: Vec<RoutingDecision>,
    #[serde(default = "default_sections_processed")]
    pub sections_processed: usize,
    #[serde(default)]
    pub strategy_chain: Vec<String>,
    #[serde(default)]
    pub cache_hit: bool,
}

fn default_sections_processed() -> usize {
    1
}

impl RouterCompressionResult {
    pub fn total_original_tokens(&self) -> usize {
        self.routing_log.iter().map(|r| r.original_tokens).sum()
    }

    pub fn total_compressed_tokens(&self) -> usize {
        self.routing_log.iter().map(|r| r.compressed_tokens).sum()
    }

    pub fn compression_ratio(&self) -> f64 {
        let orig = self.total_original_tokens();
        if orig == 0 {
            1.0
        } else {
            self.total_compressed_tokens() as f64 / orig as f64
        }
    }

    pub fn tokens_saved(&self) -> usize {
        self.total_original_tokens()
            .saturating_sub(self.total_compressed_tokens())
    }

    pub fn savings_percentage(&self) -> f64 {
        let orig = self.total_original_tokens();
        if orig == 0 {
            0.0
        } else {
            (self.tokens_saved() as f64 / orig as f64) * 100.0
        }
    }

    pub fn summary(&self) -> String {
        if self.strategy_used == CompressionStrategy::Mixed {
            let strategies: HashSet<&str> = self
                .routing_log
                .iter()
                .map(|r| r.strategy.as_str())
                .collect();
            format!(
                "Mixed content: {} sections, routed to {:?}. {}→{} tokens ({:.0}% saved)",
                self.sections_processed,
                strategies,
                self.total_original_tokens(),
                self.total_compressed_tokens(),
                self.savings_percentage()
            )
        } else {
            format!(
                "Pure {}: {}→{} tokens ({:.0}% saved)",
                self.strategy_used.as_str(),
                self.total_original_tokens(),
                self.total_compressed_tokens(),
                self.savings_percentage()
            )
        }
    }
}

// ─── ContentRouterConfig ─────────────────────────────────────────────────

/// Configuration for intelligent content routing.
#[derive(Debug, Clone)]
pub struct ContentRouterConfig {
    // Enable/disable specific compressors
    pub enable_code_aware: bool,
    pub enable_kompress: bool,
    pub enable_smart_crusher: bool,
    pub enable_search_compressor: bool,
    pub enable_log_compressor: bool,
    pub enable_tabular_compressor: bool,
    pub enable_html_extractor: bool,
    pub enable_image_optimizer: bool,

    // Routing preferences
    pub prefer_code_aware_for_code: bool,
    pub force_kompress_all: bool,

    // No-CCR lossless mode
    pub lossless: bool,
    pub min_section_tokens: usize,

    // Lossless-then-lossy: after a byte-exact lossless fold, run the aggressive
    // lossy compressor (Kompress) on the folded remainder and keep it iff it
    // removes a further meaningful chunk (>= `lossy_min_extra_savings` beyond the
    // fold). No-op in lossless-only mode; DIFF folds are never lossy-chained.
    pub lossless_then_lossy: bool,
    // Minimum extra token fraction Kompress must save beyond the fold for the
    // lossy-after-fold pass to replace the byte-exact fold (default 0.05).
    pub lossy_min_extra_savings: f64,

    // Fallback strategy
    pub fallback_strategy: CompressionStrategy,

    // Protection
    pub skip_user_messages: bool,
    pub protect_recent_code: usize,
    pub protect_analysis_context: bool,
    pub protect_error_outputs: bool,
    pub error_protection_max_chars: usize,

    // Cache safety
    pub compress_assistant_text_blocks: bool,
    pub min_chars_for_block_compression: usize,

    // Adaptive Read protection
    pub protect_recent_reads_fraction: f64,

    // Acceptance threshold
    pub min_ratio_relaxed: f64,
    pub min_ratio_aggressive: f64,

    // CCR settings
    pub ccr_enabled: bool,
    pub ccr_inject_marker: bool,
    pub smart_crusher_max_items_after_crush: Option<usize>,
    pub smart_crusher_with_compaction: bool,
    pub smart_crusher_lossless_only: Option<bool>,

    // Relevance split
    pub relevance_split: bool,
    pub relevance_max_records: usize,
    pub relevance_adaptive_threshold: bool,

    // Tag protection
    pub compress_tagged_content: bool,

    // Tool exclusion
    pub exclude_tools: Option<HashSet<String>>,

    // Shell tool names
    pub bash_tool_names: HashSet<String>,
    pub bash_search_commands: HashSet<String>,

    // Compressor config overrides (None = use defaults)
    pub smart_crusher_config: Option<Value>,
    pub search_compressor_config: Option<Value>,
    pub log_compressor_config: Option<Value>,
    pub diff_compressor_config: Option<Value>,
    pub text_crusher_config: Option<Value>,

    // Search grouping
    pub search_group_by_file: bool,

    /// Last-resort fallback for blocks no compressor could shrink: elide the
    /// middle of long, whitespace-free lines (minified JS/CSS, base64, RSC
    /// payloads). See [`super::dense_line_elider`].
    pub enable_dense_line_elision: bool,

    // Savings profile / target ratio
    /// Target compression ratio for Kompress (0.0 = auto). Lower = more aggressive.
    pub target_ratio: Option<f64>,
    /// Compress user-role messages (overrides skip_user_messages when true).
    pub compress_user_messages: Option<bool>,
    /// Compress system-role messages.
    pub compress_system_messages: Option<bool>,
    /// Per-provider Kompress disable. Key is provider name ("anthropic", "openai").
    /// Value true = disable Kompress for that provider.
    pub disable_kompress_per_provider: HashMap<String, bool>,
    /// When Kompress is disabled, route to passthrough instead of fallback.
    pub disable_kompress_fallback: bool,

    /// Names of registered external compressors to activate, as an opt-in
    /// selection resolved by [`CompressorRegistry::select`]. Empty (the default)
    /// means no external compressor runs and the built-in dispatch is reached
    /// unchanged. The literal `"*"` activates everything registered.
    ///
    /// [`CompressorRegistry::select`]: super::compressor_registry::CompressorRegistry::select
    pub active_external_compressors: Vec<String>,
}

impl Default for ContentRouterConfig {
    fn default() -> Self {
        let mut bash_tool_names = HashSet::new();
        bash_tool_names.insert("bash".to_string());
        bash_tool_names.insert("shell".to_string());
        bash_tool_names.insert("local_shell".to_string());

        let mut bash_search_commands = HashSet::new();
        for cmd in &["grep", "egrep", "fgrep", "rg", "ripgrep", "ag", "ack"] {
            bash_search_commands.insert(cmd.to_string());
        }

        Self {
            enable_code_aware: false,
            enable_kompress: true,
            enable_smart_crusher: true,
            enable_search_compressor: true,
            enable_log_compressor: true,
            enable_tabular_compressor: true,
            enable_html_extractor: true,
            enable_image_optimizer: true,
            enable_dense_line_elision: true,
            // Route code to CodeAware over Kompress for higher, syntax-safe
            // compression.
            prefer_code_aware_for_code: true,
            force_kompress_all: false,
            lossless: false,
            min_section_tokens: 20,
            lossless_then_lossy: false,
            lossy_min_extra_savings: 0.05,
            fallback_strategy: CompressionStrategy::Kompress,
            skip_user_messages: true,
            protect_recent_code: 4,
            protect_analysis_context: true,
            protect_error_outputs: true,
            error_protection_max_chars: 8000,
            compress_assistant_text_blocks: false,
            min_chars_for_block_compression: 500,
            protect_recent_reads_fraction: 0.0,
            min_ratio_relaxed: 1.0,
            min_ratio_aggressive: 1.0,
            ccr_enabled: true,
            ccr_inject_marker: true,
            smart_crusher_max_items_after_crush: None,
            smart_crusher_with_compaction: true,
            smart_crusher_lossless_only: None,
            relevance_split: true,
            relevance_max_records: 0,
            relevance_adaptive_threshold: true,
            compress_tagged_content: false,
            exclude_tools: None,
            bash_tool_names,
            bash_search_commands,
            smart_crusher_config: None,
            search_compressor_config: None,
            log_compressor_config: None,
            diff_compressor_config: None,
            text_crusher_config: None,
            search_group_by_file: false,
            target_ratio: None,
            compress_user_messages: None,
            compress_system_messages: None,
            disable_kompress_per_provider: HashMap::new(),
            disable_kompress_fallback: true,
            // Opt-in: nothing external runs until it is named.
            active_external_compressors: Vec::new(),
        }
    }
}

// ─── Helper functions ────────────────────────────────────────────────────

/// Shell wrappers that prefix the real program.
fn shell_wrappers() -> &'static HashSet<&'static str> {
    static WRAPPERS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    WRAPPERS.get_or_init(|| {
        [
            "rtk", "sudo", "env", "time", "nice", "ionice", "nohup", "stdbuf", "command",
            "timeout", "xargs",
        ]
        .iter()
        .copied()
        .collect()
    })
}

/// Return `(program_basename_lower, trailing_tokens)` for a shell command.
///
/// Peels leading wrappers (`rtk grep` -> `grep`, `timeout 30 rg` -> `rg`)
/// and env assignments (`FOO=1 grep` -> `grep`).
pub fn bash_program(command: &str) -> (String, Vec<String>) {
    let toks: Vec<&str> = command.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        let tok = toks[i];
        if tok.contains('=') && !tok.starts_with('-') {
            i += 1;
            continue;
        }
        let base = tok.rsplit('/').next().unwrap_or(tok).to_lowercase();
        if shell_wrappers().contains(base.as_str()) {
            i += 1;
            // Skip wrapper's own option/numeric args
            while i < toks.len()
                && (toks[i].starts_with('-')
                    || toks[i].replace('.', "").chars().all(|c| c.is_ascii_digit()))
            {
                i += 1;
            }
            continue;
        }
        return (base, toks[i + 1..].iter().map(|s| s.to_string()).collect());
    }
    (String::new(), vec![])
}

/// True when `command` is a read-only search whose output folds byte-losslessly.
pub fn bash_command_is_search(command: &str, search_commands: &HashSet<&str>) -> bool {
    let (prog, rest) = bash_program(command);
    if prog.is_empty() {
        return false;
    }
    if ["sh", "bash", "zsh", "dash"].contains(&prog.as_str()) && !rest.is_empty() {
        for (j, tok) in rest.iter().enumerate() {
            if ["-c", "-lc", "-lic", "-ic"].contains(&tok.as_str()) && j + 1 < rest.len() {
                let inner = rest[j + 1..]
                    .join(" ")
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string();
                return bash_command_is_search(&inner, search_commands);
            }
        }
        return false;
    }
    if prog == "git" && rest.first().map(|s| s.to_lowercase()) == Some("grep".to_string()) {
        return true;
    }
    search_commands.contains(prog.as_str())
}

// ─── Regex patterns ──────────────────────────────────────────────────────

fn code_fence_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^```(\w*)\s*$").expect("CODE_FENCE_PATTERN is valid"))
}

fn json_block_start() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*[\[{]").expect("JSON_BLOCK_START is valid"))
}

fn search_result_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\S+:\d+:").expect("SEARCH_RESULT_PATTERN is valid"))
}

/// True for grep match lines and `grep -A`/`-B`/`-C` context lines.
///
/// Unions the legacy splitter pattern above (kept so every previously
/// carved line still carves) with both guarded context predicates from the
/// detector, so code in context lines routes to the search compressor
/// instead of the prose path (upstream #3599).
fn is_search_section_line(line: &str) -> bool {
    search_result_pattern().is_match(line)
        || super::content_detector::is_grep_context_line(line)
        || super::content_detector::is_grep_colon_dash_line(line)
}

fn prose_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Z][a-z]+\s+\w+\s+\w+").expect("PROSE_PATTERN is valid"))
}

/// Detect if content contains multiple distinct types.
pub fn is_mixed_content(content: &str) -> bool {
    // Mirror Python's `has_search_results`: legacy pattern or any guarded
    // grep context line, so pure-context-lines-plus-prose reads mixed on
    // both ports.
    let has_search = search_result_pattern().is_match(content)
        || content.lines().any(|l| {
            super::content_detector::is_grep_context_line(l)
                || super::content_detector::is_grep_colon_dash_line(l)
        });
    let indicators = [
        code_fence_pattern().is_match(content),
        json_block_start().is_match(content),
        prose_pattern().find_iter(content).count() > 5,
        has_search,
    ];
    indicators.iter().filter(|&&x| x).count() >= 2
}

/// Analyze content shape as JSON.
pub fn json_shape(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(parsed) => {
            if let Some(obj) = parsed.as_object() {
                serde_json::json!({
                    "is_json": true,
                    "kind": "object",
                    "keys": obj.keys().cloned().collect::<Vec<_>>(),
                    "length": obj.len(),
                })
            } else if let Some(arr) = parsed.as_array() {
                serde_json::json!({
                    "is_json": true,
                    "kind": "array",
                    "length": arr.len(),
                })
            } else {
                serde_json::json!({
                    "is_json": true,
                    "kind": "scalar",
                })
            }
        }
        Err(exc) => serde_json::json!({
            "is_json": false,
            "error": exc.to_string(),
        }),
    }
}

/// Quantize a net-cost gain into a coarse magnitude band for markers.
pub fn gain_bucket(gain: f64) -> String {
    if !gain.is_finite() {
        return "nan".to_string();
    }
    let mag = gain.abs();
    let band = if mag < 100.0 {
        "lt100"
    } else if mag < 1000.0 {
        "lt1k"
    } else if mag < 10000.0 {
        "lt10k"
    } else {
        "gte10k"
    };
    if gain == 0.0 {
        return "0".to_string();
    }
    let sign = if gain < 0.0 { "neg_" } else { "" };
    format!("{}{}", sign, band)
}

// ─── Tool call parsing ───────────────────────────────────────────────────

/// Compact, query-usable text from a tool call's args.
///
/// Anthropic passes `input` as a dict; OpenAI passes `arguments` as a JSON
/// string. Either way we want the scalar values as a short query fragment.
/// Capped at 300 chars so a giant arg blob can't dominate the relevance query.
pub fn tool_call_args_text(raw: &Value) -> String {
    let text = match raw {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .values()
            .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => return String::new(),
    };
    // Normalize whitespace and cap at 300 chars
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(300).collect()
}

/// Extract the raw shell command from a tool call's args, if present.
///
/// Anthropic `input` is a dict ({"command": "grep …"}); OpenAI `arguments`
/// is a JSON string; Codex's shell uses a `command` list.
pub fn tool_call_command_text(raw: &Value) -> String {
    let obj = match raw {
        Value::String(s) => {
            // Try to parse as JSON
            match serde_json::from_str::<Value>(s) {
                Ok(v) => v,
                Err(_) => return String::new(),
            }
        }
        Value::Object(map) => Value::Object(map.clone()),
        _ => return String::new(),
    };

    let obj = match obj.as_object() {
        Some(o) => o,
        None => return String::new(),
    };

    // Try "command" then "cmd"
    let cmd_val = obj.get("command").or_else(|| obj.get("cmd"));

    match cmd_val {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                _ => v.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

// ─── Envelope detection ──────────────────────────────────────────────────

/// Return the inner payload of a tool-output envelope, for detection only.
///
/// Only strips when the ENTIRE string is a single wrapper envelope, so content
/// that merely mentions these tags is left untouched. Never returns an empty
/// probe (falls back to the original when the body is blank).
pub fn strip_detection_envelope(content: &str) -> String {
    if !content.contains('<') {
        return content.to_string();
    }

    let trimmed = content.trim();

    // Strip optional leading <returncode>N</returncode> (N must be numeric)
    let after_returncode = if let Some(rest) = trimmed.strip_prefix("<returncode>") {
        if let Some(end) = rest.find("</returncode>") {
            let rc_content = rest[..end].trim();
            // Validate numeric (matching Python's -?\d+)
            // Validate numeric (matching Python's -?\d+): optional leading minus, then digits
            let valid = if rc_content.is_empty() {
                false
            } else if let Some(digits) = rc_content.strip_prefix('-') {
                digits.chars().all(|c| c.is_ascii_digit())
            } else {
                rc_content.chars().all(|c| c.is_ascii_digit())
            };
            if valid {
                rest[end + "</returncode>".len()..].trim()
            } else {
                trimmed
            }
        } else {
            trimmed
        }
    } else {
        trimmed
    };

    // Try each supported tag
    for tag in &["output", "stdout", "stderr", "tool_result", "result"] {
        let open_pattern = format!("<{tag}>");
        let close_pattern = format!("</{tag}>");

        if after_returncode.starts_with(&open_pattern) && after_returncode.ends_with(&close_pattern)
        {
            let inner =
                &after_returncode[open_pattern.len()..after_returncode.len() - close_pattern.len()];
            let inner = inner.trim();
            if !inner.is_empty() {
                return inner.to_string();
            }
        }
    }

    content.to_string()
}

// ─── JSON block extraction ───────────────────────────────────────────────

/// Extract a complete JSON block from lines starting at `start`.
///
/// Returns (json_content, end_line_index) or (None, start) if invalid.
pub fn extract_json_block(lines: &[&str], start: usize) -> (Option<String>, usize) {
    let mut bracket_count = 0i32;
    let mut brace_count = 0i32;
    let mut json_lines: Vec<&str> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (i, &line) in lines.iter().enumerate().skip(start) {
        json_lines.push(line);

        for ch in line.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                if in_string {
                    escaped = true;
                }
                continue;
            }
            if ch == '"' {
                in_string = !in_string;
                continue;
            }
            if in_string {
                continue;
            }
            match ch {
                '[' => bracket_count += 1,
                ']' => bracket_count -= 1,
                '{' => brace_count += 1,
                '}' => brace_count -= 1,
                _ => {}
            }
        }

        if bracket_count <= 0 && brace_count <= 0 && !json_lines.is_empty() {
            return (Some(json_lines.join("\n")), i);
        }
    }

    (None, start)
}

// ─── Section splitting ───────────────────────────────────────────────────

/// Parse mixed content into typed sections.
pub fn split_into_sections(content: &str) -> Vec<ContentSection> {
    let mut sections: Vec<ContentSection> = Vec::new();
    let lines: Vec<&str> = content.split('\n').collect();
    let code_re = code_fence_pattern();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Code fence: ```language
        if let Some(m) = code_re.captures(line) {
            let language = m
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            let mut code_lines: Vec<&str> = Vec::new();
            let start_line = i;
            i += 1;

            while i < lines.len() && !lines[i].starts_with("```") {
                code_lines.push(lines[i]);
                i += 1;
            }

            sections.push(ContentSection {
                content: code_lines.join("\n"),
                content_type: ContentType::SourceCode,
                language: Some(language.to_string()),
                start_line,
                end_line: i,
                is_code_fence: true,
            });
            i += 1; // Skip closing ```
            continue;
        }

        // JSON block
        if line.trim().starts_with('[') || line.trim().starts_with('{') {
            let (json_content, end_i) = extract_json_block(&lines, i);
            if let Some(content) = json_content {
                sections.push(ContentSection {
                    content,
                    content_type: ContentType::JsonArray,
                    language: None,
                    start_line: i,
                    end_line: end_i,
                    is_code_fence: false,
                });
                i = end_i + 1;
                continue;
            }
        }

        // Search result lines
        if is_search_section_line(line) {
            let mut search_lines: Vec<&str> = Vec::new();
            let start_line = i;
            while i < lines.len() && is_search_section_line(lines[i]) {
                search_lines.push(lines[i]);
                i += 1;
            }
            sections.push(ContentSection {
                content: search_lines.join("\n"),
                content_type: ContentType::SearchResults,
                language: None,
                start_line,
                end_line: i.saturating_sub(1),
                is_code_fence: false,
            });
            continue;
        }

        // Collect text until next special section
        let mut text_lines: Vec<&str> = Vec::new();
        let start_line = i;
        text_lines.push(line);
        i += 1;

        while i < lines.len() {
            let next_line = lines[i];
            // Stop if we hit a special section
            if code_re.is_match(next_line)
                || next_line.trim().starts_with('[')
                || next_line.trim().starts_with('{')
                || is_search_section_line(next_line)
            {
                break;
            }
            text_lines.push(next_line);
            i += 1;
        }

        // Only add non-empty text sections
        let text_content = text_lines.join("\n");
        if !text_content.trim().is_empty() {
            sections.push(ContentSection {
                content: text_content,
                content_type: ContentType::PlainText,
                language: None,
                start_line,
                end_line: i.saturating_sub(1),
                is_code_fence: false,
            });
        }
    }

    sections
}

// ─── Net-cost helpers ────────────────────────────────────────────────────

/// Provider cache TTL (seconds) used to decay P_alive from idle time.
///
/// Defaults to Anthropic's 5-minute tier; overridable via
/// `HEADROOM_NET_COST_CACHE_TTL_SECONDS`.
pub fn net_cost_cache_ttl_seconds() -> f64 {
    const DEFAULT: f64 = 300.0;
    let raw = std::env::var("HEADROOM_NET_COST_CACHE_TTL_SECONDS").unwrap_or_default();
    if raw.is_empty() {
        return DEFAULT;
    }
    match raw.parse::<f64>() {
        Ok(ttl) if ttl.is_finite() && ttl > 0.0 => ttl,
        _ => {
            tracing::warn!(
                event = "net_cost_ttl_invalid",
                raw = %raw,
                default = DEFAULT,
                "HEADROOM_NET_COST_CACHE_TTL_SECONDS malformed; using default"
            );
            DEFAULT
        }
    }
}

/// Create a content signature hash for TOIN tracking.
///
/// Returns a 24-char SHA-256 hash that groups similar content types together
/// for pattern learning. The hash is deterministic and persists to disk, so
/// changing the algorithm would invalidate learned patterns.
pub fn create_content_signature(
    content_type: &str,
    content: &str,
    language: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};

    let hash_input = if let Some(lang) = language {
        format!("content:{}:{}", content_type, lang)
    } else {
        format!("content:{}", content_type)
    };

    // Add structural hint from first 100 characters (matching Python's content[:100])
    // Python string slicing is character-based, so chars().take(100) is equivalent.
    let content_sample: String = content.chars().take(100).collect();
    let mut structure_hint_hasher = Sha256::new();
    structure_hint_hasher.update(content_sample.as_bytes());
    let structure_hint = hex::encode(structure_hint_hasher.finalize())[..8].to_string();

    let full_input = format!("{}:{}", hash_input, structure_hint);

    let mut hasher = Sha256::new();
    hasher.update(full_input.as_bytes());
    let result = hex::encode(hasher.finalize());

    result[..24].to_string()
}

/// Token count of a message for net-cost suffix estimation.
///
/// Counts text-bearing fields in Anthropic block-list content rather than
/// stringifying the whole list, which would miscount.
pub fn netcost_message_tokens(content: &Value) -> usize {
    match content {
        Value::String(s) => s.split_whitespace().count(),
        Value::Array(blocks) => {
            let mut total = 0;
            for block in blocks {
                if let Some(obj) = block.as_object() {
                    if let Some(block_type) = obj.get("type").and_then(Value::as_str) {
                        match block_type {
                            "text" => {
                                total += obj
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .map(|t| t.split_whitespace().count())
                                    .unwrap_or(0);
                            }
                            "tool_result" => {
                                if let Some(tc) = obj.get("content") {
                                    match tc {
                                        Value::String(s) => {
                                            total += s.split_whitespace().count();
                                        }
                                        Value::Array(subs) => {
                                            for sub in subs {
                                                if let Some(sub_obj) = sub.as_object() {
                                                    if sub_obj.get("type").and_then(Value::as_str)
                                                        == Some("text")
                                                    {
                                                        total += sub_obj
                                                            .get("text")
                                                            .and_then(Value::as_str)
                                                            .map(|t| t.split_whitespace().count())
                                                            .unwrap_or(0);
                                                    } else {
                                                        total += sub
                                                            .to_string()
                                                            .split_whitespace()
                                                            .count();
                                                    }
                                                } else {
                                                    total +=
                                                        sub.to_string().split_whitespace().count();
                                                }
                                            }
                                        }
                                        _ => {
                                            total += tc.to_string().split_whitespace().count();
                                        }
                                    }
                                }
                            }
                            // Price media blocks at the canonical flat cost.
                            // Falling through to `block.to_string()` embeds the
                            // whole base64 payload, so one screenshot counted
                            // ~100,000 tokens instead of ~1,600 (57x-146x over,
                            // growing with image size). S is the cache-bust
                            // cost, so an image inflated S for *every message
                            // before it* and the break-even gate then refused to
                            // compress any of them.
                            "image" | "image_url" | "input_image" => {
                                total += crate::tokenizer::IMAGE_TOKENS;
                            }
                            "input_audio" | "audio" => {
                                total += crate::tokenizer::AUDIO_TOKENS;
                            }
                            _ => {
                                total += block.to_string().split_whitespace().count();
                            }
                        }
                    } else {
                        total += block.to_string().split_whitespace().count();
                    }
                } else {
                    total += block.to_string().split_whitespace().count();
                }
            }
            total
        }
        Value::Null => 0,
        _ => content.to_string().split_whitespace().count(),
    }
}

// ─── Content detection orchestration ─────────────────────────────────────

/// Resolve the content-detection backend from env var.
pub fn resolve_detect_backend() -> &'static str {
    let backend = std::env::var("HEADROOM_DETECT_BACKEND")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    match backend.as_str() {
        "python" => "python",
        "rust" => "rust",
        _ => "rust", // Default to Rust on non-Windows
    }
}

/// Strip envelope and detect content type using the Rust detection chain.
///
/// This is the Rust-native equivalent of Python's `_detect_content()`.
/// It strips tool-output envelopes, then delegates to the existing
/// `detect_content_type` function in `content_detector.rs`.
///
/// Includes the HTML misroute guard: when the detector says HTML, we check
/// if the content is actually a log or search result (dense punctuation in
/// grep output can look like markup). Trust structural log/search detectors
/// over the HTML verdict.
pub fn detect_content_native(content: &str) -> ContentType {
    let stripped = strip_detection_envelope(content);
    let result = super::content_detector::detect_content_type(&stripped);

    // HTML misroute guard: grep/build output with <> can be misclassified as HTML
    if result.content_type == ContentType::Html
        && let Some(override_result) = super::content_detector::try_detect_log(&stripped)
            .or_else(|| super::content_detector::try_detect_search(&stripped))
    {
        return override_result.content_type;
    }

    result.content_type
}

/// Map a ContentType to the best CompressionStrategy.
///
/// When `prefer_code_aware_for_code` is true (the default), source code routes
/// to CodeAware for higher, syntax-safe compression; when false it routes to
/// Kompress instead, letting code pass through unmangled.
pub fn strategy_from_detection(
    content_type: ContentType,
    prefer_code_aware_for_code: bool,
) -> CompressionStrategy {
    match content_type {
        ContentType::JsonArray => CompressionStrategy::SmartCrusher,
        ContentType::SourceCode => {
            if prefer_code_aware_for_code {
                CompressionStrategy::CodeAware
            } else {
                CompressionStrategy::Kompress
            }
        }
        ContentType::SearchResults => CompressionStrategy::Search,
        ContentType::BuildOutput => CompressionStrategy::Log,
        ContentType::GitDiff => CompressionStrategy::Diff,
        ContentType::Html => CompressionStrategy::Html,
        // Never Kompress: it drops words inside rows, so some rows would lose
        // a field and others not, with nothing marking which (upstream
        // `6c9aef1d`). No tabular compressor runs here, so tables pass through.
        ContentType::Tabular => CompressionStrategy::Tabular,
        ContentType::StructuredConfig => CompressionStrategy::Config,
        ContentType::PlainText => CompressionStrategy::Kompress,
    }
}

// ─── ContentRouter dispatcher ────────────────────────────────────────────

/// Dispatch a compression strategy to the appropriate Rust compressor.
///
/// This is the core routing logic. It takes content + strategy and returns
/// the compressed result. All compressors are called directly in Rust
/// (no Python FFI needed for the hot path).
///
/// Byte/data-lossless first pass (intended design: always runs, pre-lossy).
///
/// Maps the (content-detected) strategy to its format-native lossless fold —
/// SEARCH → ripgrep --heading form, LOG → run-collapse + ANSI strip, DIFF →
/// drop `index` bookkeeping — and gives every other content type a trivial
/// blank-run collapse. `compact_lossless` is self-verifying (exact inverse or
/// unchanged) and returns the input when it cannot safely shrink, so this never
/// loses information and is a strict no-op when nothing folds.
///
/// Returns `(folded, Some("lossless_<kind>"))` when a real byte shrink happened,
/// else `(content, None)`.
fn lossless_first(content: &str, strategy: CompressionStrategy) -> (String, Option<String>) {
    use super::lossless_compaction::compact_lossless;

    // Apply losslessness to the OUTPUT structure, not to the classification:
    // try the fold implied by the detected strategy first, then the others.
    // Each compact_lossless call is self-verifying, so attempting a fold on
    // non-matching content is a safe no-op — this recovers folds on content the
    // detector misroutes. Keep the single fold that shrinks the most.
    let primary = match strategy {
        CompressionStrategy::Search => Some("search"),
        CompressionStrategy::Log => Some("log"),
        CompressionStrategy::Diff => Some("diff"),
        CompressionStrategy::Config => Some("config"),
        _ => None,
    };
    let mut order: Vec<&str> = primary
        .into_iter()
        .chain(
            ["search", "paths", "log", "diff", "text", "config"]
                .into_iter()
                .filter(|k| Some(*k) != primary),
        )
        .collect();
    // The "diff" fold (`diff_strip_index`) is the one `compact_lossless` kind
    // that is purely subtractive with NO exact-inverse check: it removes any
    // line shaped like `index <hex>..<hex>`. On non-diff content that happens to
    // contain such a line, that line is silently and unrecoverably dropped —
    // breaking the lossless contract, and unmarked in CCR mode. Only fold diffs
    // as diffs.
    if strategy != CompressionStrategy::Diff && !looks_like_diff(content) {
        order.retain(|k| *k != "diff");
    }

    let mut best = content.to_string();
    let mut best_label: Option<String> = None;
    for kind in order {
        let cand = compact_lossless(content, kind);
        if cand.len() < best.len() {
            best = cand;
            best_label = Some(format!("lossless_{}", kind));
        }
    }
    (best, best_label)
}

/// Cheap structural sniff for unified/git-diff content. Keeps the
/// lossy-after-fold pass (Kompress) OFF diff content — Kompressing hunks
/// corrupts `git apply`. Defense-in-depth beyond the DIFF-strategy and
/// `lossless_diff`-label checks.
fn looks_like_diff(content: &str) -> bool {
    content.contains("diff --git ")
        || content.contains("\n@@ ")
        || content.starts_with("@@ ")
        || content.starts_with("--- ")
}

/// Returns (compressed_text, compressed_tokens, strategy_chain).
/// The [`ContentType`] a strategy implies — Python's `_content_type_from_strategy`.
///
/// [`ContentType`]: super::content_detector::ContentType
fn content_type_from_strategy(strategy: CompressionStrategy) -> ContentType {
    match strategy {
        CompressionStrategy::CodeAware => ContentType::SourceCode,
        CompressionStrategy::SmartCrusher => ContentType::JsonArray,
        CompressionStrategy::Search => ContentType::SearchResults,
        CompressionStrategy::Log => ContentType::BuildOutput,
        CompressionStrategy::Diff => ContentType::GitDiff,
        CompressionStrategy::Html => ContentType::Html,
        CompressionStrategy::Tabular => ContentType::Tabular,
        CompressionStrategy::Config => ContentType::StructuredConfig,
        // TEXT, KOMPRESS, PASSTHROUGH and anything unmapped fall through to
        // plain text, matching Python's `mapping.get(strategy, PLAIN_TEXT)`.
        _ => ContentType::PlainText,
    }
}

/// MIME type for a detected content type — Python's `_CONTENT_TYPE_TO_MIME`.
///
/// Used only by the external-compressor path, to hand a plain string across the
/// pure-data contract boundary instead of a crate-internal enum.
fn content_type_mime(content_type: ContentType) -> &'static str {
    match content_type {
        ContentType::JsonArray => "application/json",
        ContentType::SourceCode => "text/x-code",
        ContentType::SearchResults => "text/x-search-results",
        ContentType::BuildOutput => "text/x-log",
        ContentType::GitDiff => "text/x-diff",
        ContentType::Html => "text/html",
        ContentType::Tabular => "text/csv",
        ContentType::StructuredConfig => "text/x-config",
        ContentType::PlainText => "text/plain",
    }
}

/// True if `descriptor` declares support for `content_mime`.
///
/// Accepts an exact MIME match, a full wildcard (`"*"` or `"*/*"`), or a type
/// wildcard (`"text/*"` matches `"text/plain"`). Anything else is a non-match,
/// so a selected external compressor only ever sees content it explicitly
/// declared it can handle.
fn external_compressor_matches(descriptor: &CompressorDescriptor, content_mime: &str) -> bool {
    if descriptor.content_types.iter().any(|d| d == content_mime) {
        return true;
    }
    let top = content_mime.split('/').next().unwrap_or("");
    let type_wildcard = format!("{top}/*");
    descriptor
        .content_types
        .iter()
        .any(|d| d == "*" || d == "*/*" || *d == type_wildcard)
}

/// True if `out` must be discarded in favour of the built-in path, logging why.
fn external_output_rejected(name: &str, content: &str, out: &CompressOutput) -> bool {
    // Never blank out a non-empty block (an empty user/tool block makes
    // providers reject the request); fall back so the built-in path runs.
    if !content.trim().is_empty() && out.content.trim().is_empty() {
        tracing::warn!(
            event = "content_router_external_empty_output",
            compressor = %name,
            "external compressor produced empty output; falling back to built-in"
        );
        return true;
    }
    // Never let an external compressor expand a block; fall back so the built-in
    // path (or passthrough) can do better.
    if out.content.len() > content.len() {
        tracing::debug!(
            compressor = %name,
            before = content.len(),
            after = out.content.len(),
            "external compressor expanded content; falling back"
        );
        return true;
    }
    false
}

/// Persist each `hash -> original` recovery entry, warning on a store failure.
fn store_external_recoverables(
    name: &str,
    out: &CompressOutput,
    strategy_label: &str,
    store_recoverable: &dyn Fn(&str, &str, &str) -> bool,
) {
    for (ccr_hash, original) in &out.recoverable {
        if !store_recoverable(ccr_hash, original, strategy_label) {
            tracing::warn!(
                event = "content_router_recoverable_unstored",
                compressor = %name,
                hash = %ccr_hash,
                "external compressor recoverable entry was not stored"
            );
        }
    }
}

/// Emit the compressor's non-fatal warnings, if it reported any.
fn log_external_warnings(name: &str, out: &CompressOutput) {
    if !out.warnings.is_empty() {
        tracing::debug!(
            compressor = %name,
            warnings = %out.warnings.join("; "),
            "external compressor warnings"
        );
    }
}

/// Invoke one external compressor via the contract; fail open to `None`.
fn run_external_compressor(
    compressor: &Arc<dyn Compressor>,
    name: &str,
    content: &str,
    content_mime: &str,
    context: &str,
    question: Option<&str>,
    store_recoverable: &dyn Fn(&str, &str, &str) -> bool,
) -> Option<(String, usize, Vec<String>)> {
    let input = CompressInput {
        content: content.to_string(),
        content_type: content_mime.to_string(),
        query: question
            .filter(|q| !q.is_empty())
            .unwrap_or(context)
            .to_string(),
        ..Default::default()
    };

    // Rust's type system already guarantees the "malformed output" case Python
    // has to check for at runtime, so that branch has no counterpart here.
    let out = compressor.compress(&input);

    if external_output_rejected(name, content, &out) {
        return None;
    }

    // Count with the router's OWN estimator, not the compressor's self-report.
    let compressed_tokens = out.content.split_whitespace().count();

    // Persist the hash -> original recovery map so a later /v1/retrieve resolves
    // each hash. Best-effort: a store failure leaves that entry unretrievable
    // but never breaks the request.
    let strategy_label = format!("external:{name}");
    store_external_recoverables(name, &out, &strategy_label, store_recoverable);

    log_external_warnings(name, &out);

    Some((out.content, compressed_tokens, vec![strategy_label]))
}

/// Route a block through a *selected* external compressor, or return `None`.
///
/// Opt-in and fail-open. Returns `None` — leaving the built-in dispatch to run
/// UNCHANGED — whenever no external compressor was selected (the default, a
/// single cheap guard so the request path is byte-identical to today), none of
/// the active compressors declares this block's content type, or the chosen one
/// returns empty output or would expand the content.
///
/// Reached only in lossy/CCR mode: [`apply_strategy_with_registry`] returns
/// earlier in lossless-only mode and on a successful STAGE 0 fold, so an
/// external compressor can never inject unrecoverable loss into a lossless-only
/// session, nor override a byte-exact fold.
fn try_external_compressor(
    content: &str,
    strategy: CompressionStrategy,
    config: &ContentRouterConfig,
    context: &str,
    question: Option<&str>,
    registry: &CompressorRegistry,
    store_recoverable: &dyn Fn(&str, &str, &str) -> bool,
) -> Option<(String, usize, Vec<String>)> {
    if config.active_external_compressors.is_empty() {
        return None;
    }
    let content_mime = content_type_mime(content_type_from_strategy(strategy));
    for compressor in registry.active(Some(&config.active_external_compressors)) {
        let descriptor = compressor.descriptor();
        if !external_compressor_matches(descriptor, content_mime) {
            continue;
        }
        let name = descriptor.name.clone();
        if let Some(result) = run_external_compressor(
            &compressor,
            &name,
            content,
            content_mime,
            context,
            question,
            store_recoverable,
        ) {
            return Some(result);
        }
    }
    None
}

/// Apply `strategy` to `content` using only the built-in compressors.
///
/// Thin wrapper over [`apply_strategy_with_registry`] with an empty registry, so
/// no external compressor can run. This is the parity-locked entry point every
/// existing caller uses.
pub fn apply_strategy(
    content: &str,
    strategy: CompressionStrategy,
    config: &ContentRouterConfig,
    context: &str,
    language: Option<&str>,
    bias: f64,
) -> (String, usize, Vec<String>) {
    let empty = CompressorRegistry::new();
    apply_strategy_with_registry(
        content,
        strategy,
        config,
        context,
        language,
        bias,
        None,
        &empty,
        &|_, _, _| true,
    )
}

/// Apply `strategy` to `content`, optionally routing through a *selected*
/// external compressor first.
///
/// Python hangs its registry off the `ContentRouter` instance; this Rust module
/// is a free-function dispatcher with no router state, so the registry and the
/// CCR store hook are passed in. `store_recoverable(hash, original, strategy)`
/// returns whether the entry was persisted, and is only ever called for an
/// external compressor's recovery map.
///
/// Strategies whose result is still plain text, so dense-line elision can
/// run on top of them. SmartCrusher, tabular, config, and diff emit their own
/// structured (and CCR-marked) forms; Kompress is lossy with its own marker.
/// Eliding inside those would corrupt output another compressor owns.
/// CodeAware is here so a partial win on minified JS still loses its bundle
/// lines (upstream `9263b420`).
fn dense_elide_after(strategy: CompressionStrategy) -> bool {
    matches!(
        strategy,
        CompressionStrategy::Html
            | CompressionStrategy::Log
            | CompressionStrategy::Text
            | CompressionStrategy::CodeAware
            | CompressionStrategy::Search
    )
}

/// Dense-line elision with a CCR retrieval marker; `None` when no line is dense.
///
/// The elided middle is lossy, so the pre-elision block is stored via
/// `store_recoverable` and a `Retrieve original: hash=` marker is appended —
/// the same contract Kompress honours: the messages path discards a lossy
/// result that carries no marker, and the agent can get the exact bytes back.
/// Never in lossless mode, where nothing may be dropped.
#[allow(clippy::too_many_arguments)]
fn elide_dense(
    text: &str,
    context: &str,
    config: &ContentRouterConfig,
    store_recoverable: &dyn Fn(&str, &str, &str) -> bool,
) -> Option<(String, usize)> {
    if config.lossless {
        return None;
    }
    let (elided, n_dense) = super::dense_line_elider::elide_dense_lines(text);
    if n_dense == 0 {
        return None;
    }
    let mut out = elided;
    if config.ccr_inject_marker {
        let hash = crate::ccr::compute_key(text.as_bytes());
        if store_recoverable(&hash, text, "dense_elide") {
            out.push_str(&format!("\n{}", crate::ccr::marker_for(&hash)));
        }
    }
    let _ = context;
    // Char-length gate, not the word-count estimator: a whitespace-free
    // dump counts ~nothing in words, so the estimator cannot see the win
    // (and the marker words would read as a loss). Upstream gates the same
    // way (chars halved on its web-research replay).
    if out.len() >= text.len() {
        return None;
    }
    let tokens = out.split_whitespace().count();
    Some((out, tokens))
}

/// With an empty [`ContentRouterConfig::active_external_compressors`] this is
/// exactly [`apply_strategy`].
#[allow(clippy::too_many_arguments)]
pub fn apply_strategy_with_registry(
    content: &str,
    strategy: CompressionStrategy,
    config: &ContentRouterConfig,
    context: &str,
    language: Option<&str>,
    bias: f64,
    question: Option<&str>,
    registry: &CompressorRegistry,
    store_recoverable: &dyn Fn(&str, &str, &str) -> bool,
) -> (String, usize, Vec<String>) {
    let original_tokens = content.split_whitespace().count();

    // ── STAGE 0: LOSSLESS-FIRST (unconditional floor) ────────────────────
    // A byte/data-lossless fold has ZERO accuracy cost, so it ALWAYS runs
    // first, in every mode — it banks a guaranteed, fully-recoverable win up
    // front. `lossless_first` is self-verifying → never loses information, and
    // is a strict no-op returning (content, None) when nothing folds.
    let (ll_content, ll_label) = lossless_first(content, strategy);

    // ── LOSSLESS-ONLY mode: stop at the byte-exact fold ──────────────────
    // No-unrecoverable-loss contract: never layer a lossy drop on top. When it
    // folds, return it; otherwise leave the block verbatim (passthrough).
    if config.lossless {
        if let Some(label) = ll_label {
            let tokens = ll_content.split_whitespace().count();
            return (ll_content, tokens, vec![label]);
        }
        return (
            content.to_string(),
            original_tokens,
            vec!["passthrough".to_string()],
        );
    }

    // ── LOSSY / CCR mode: the fold is the floor ──────────────────────────
    // (The Python router's relevance-split branch runs here for LOG/SEARCH;
    // this Rust dispatch primitive leaves relevance-split to its caller.)
    // Return the STAGE 0 fold as the floor. Lossless-then-lossy: before
    // returning, run Kompress on the folded remainder and keep it IFF it removes
    // a further meaningful chunk (>= lossy_min_extra_savings beyond the fold).
    // DIFF folds are returned verbatim — Kompressing hunks corrupts `git apply`.
    if let Some(label) = ll_label {
        let lossy_after_fold = config.lossless_then_lossy
            && strategy != CompressionStrategy::Diff
            && label != "lossless_diff"
            && !looks_like_diff(content)
            && config.enable_kompress;
        if lossy_after_fold {
            let fold_tokens = ll_content.split_whitespace().count();
            let (komp, komp_tokens, _chain) = try_kompress(
                &ll_content,
                config,
                context,
                &["kompress".to_string()],
                store_recoverable,
            );
            if (komp_tokens as f64) <= fold_tokens as f64 * (1.0 - config.lossy_min_extra_savings)
                && komp.len() < ll_content.len()
            {
                return (
                    komp,
                    komp_tokens,
                    vec![label, CompressionStrategy::Kompress.as_str().to_string()],
                );
            }
        }
        // Dense-line elision gets first refusal on the stage-0 floor:
        // a repetition fold on a bundle dump keeps the dense lines whole,
        // and returning here would bypass the match-path hook below.
        if config.enable_dense_line_elision {
            // elide_dense char-gates internally; Some means shorter.
            if let Some((elided, elided_tokens)) =
                elide_dense(&ll_content, context, config, store_recoverable)
            {
                return (
                    elided,
                    elided_tokens,
                    vec![label, "dense_elide".to_string()],
                );
            }
        }
        let tokens = ll_content.split_whitespace().count();
        return (ll_content, tokens, vec![label]);
    }

    // ── EXTERNAL DISPATCH (opt-in) ───────────────────────────────────────
    // A selected external compressor gets first refusal on the block. Fails
    // open: any `None` here leaves the built-in dispatch below untouched, and
    // with no selection this is a single `is_empty()` check.
    if let Some(external) = try_external_compressor(
        content,
        strategy,
        config,
        context,
        question,
        registry,
        store_recoverable,
    ) {
        return external;
    }

    let strategy_result = match strategy {
        CompressionStrategy::SmartCrusher if config.enable_smart_crusher => {
            let crusher = super::smart_crusher::SmartCrusher::new(Default::default());
            let result = crusher.crush(content, context, bias);
            let compressed = result.compressed;
            let tokens = compressed.split_whitespace().count();
            if tokens >= original_tokens {
                // Fallback 1: Kompress
                if config.enable_kompress {
                    let (k_comp, k_tok, k_chain) = try_kompress(
                        content,
                        config,
                        context,
                        &["smart_crusher".into(), "kompress".into()],
                        store_recoverable,
                    );
                    if k_tok < tokens {
                        return (k_comp, k_tok, k_chain);
                    }
                }
                // Fallback 2: Log compressor (last resort — repetitive JSONL
                // that Kompress can't shrink but the log compressor can).
                // Always record "log" in the chain to match Python's
                // behaviour: the chain documents every strategy *attempted*,
                // not just the one that won.
                if config.enable_log_compressor {
                    let compressor = super::log_compressor::LogCompressor::new(Default::default());
                    let (log_result, _stats) = compressor.compress_with_store(content, bias, None);
                    let log_tokens = log_result.compressed.split_whitespace().count();
                    if log_tokens < tokens {
                        let chain = vec!["smart_crusher".into(), "kompress".into(), "log".into()];
                        return (log_result.compressed, log_tokens, chain);
                    }
                    // Log tried but didn't help — still record it
                    let chain = vec!["smart_crusher".into(), "kompress".into(), "log".into()];
                    return (compressed, tokens, chain);
                }
                // All fallbacks failed — fall through to return SmartCrusher result
            }
            (compressed, tokens, vec!["smart_crusher".to_string()])
        }
        CompressionStrategy::Search if config.enable_search_compressor => {
            let compressor = super::search_compressor::SearchCompressor::new(Default::default());
            let (result, _stats) = compressor.compress_with_store(content, context, bias, None);
            let tokens = result.compressed.split_whitespace().count();
            (result.compressed, tokens, vec!["search".to_string()])
        }
        CompressionStrategy::Log if config.enable_log_compressor => {
            let compressor = super::log_compressor::LogCompressor::new(Default::default());
            let (result, _stats) = compressor.compress_with_store(content, bias, None);
            let tokens = result.compressed.split_whitespace().count();
            (result.compressed, tokens, vec!["log".to_string()])
        }
        CompressionStrategy::Diff => {
            let compressor = super::diff_compressor::DiffCompressor::new(Default::default());
            let result = compressor.compress(content, context);
            let tokens = result.compressed.split_whitespace().count();
            (result.compressed, tokens, vec!["diff".to_string()])
        }
        CompressionStrategy::CodeAware if config.enable_code_aware => {
            let compressor = super::code_compressor::CodeAwareCompressor::new(Default::default());
            let result = compressor.compress_with(content, language, context);
            let compressed = result.compressed;
            let tokens = compressed.split_whitespace().count();
            // Fallback: if CodeAware saved nothing, try Kompress
            if tokens >= original_tokens && config.enable_kompress {
                let chain = vec!["code_aware".to_string(), "kompress".to_string()];
                return try_kompress(content, config, context, &chain, store_recoverable);
            }
            (compressed, tokens, vec!["code_aware".to_string()])
        }
        CompressionStrategy::Html if config.enable_html_extractor => {
            // Python: compressed = result.extracted, decision_reason
            // "html_extractor". Extraction failure yields "" there and the
            // downstream acceptance gate rejects it; here (like
            // `try_kompress`) the guard is inline: only adopt output that
            // is non-empty and actually smaller than the input.
            let extractor = super::html_extractor::HtmlExtractor::default();
            let result = extractor.extract(content, None);
            let compressed = result.extracted;
            let tokens = compressed.split_whitespace().count();
            // A page that is all <script>/<style> extracts to whitespace:
            // nothing extracted, so it falls through to passthrough (and
            // dense-line elision below), not a compression to zero tokens.
            if !compressed.trim().is_empty() && tokens < original_tokens {
                return (compressed, tokens, vec!["html_extractor".to_string()]);
            }
            (
                content.to_string(),
                original_tokens,
                vec!["html_extractor".to_string(), "passthrough".to_string()],
            )
        }
        CompressionStrategy::Kompress | CompressionStrategy::Text if config.enable_kompress => {
            try_kompress(
                content,
                config,
                context,
                &["kompress".to_string()],
                store_recoverable,
            )
        }
        CompressionStrategy::Passthrough => {
            if config.enable_dense_line_elision {
                // elide_dense char-gates internally; Some means shorter.
                if let Some((elided, elided_tokens)) =
                    elide_dense(content, context, config, store_recoverable)
                {
                    return (
                        elided,
                        elided_tokens,
                        vec!["passthrough".to_string(), "dense_elide".to_string()],
                    );
                }
            }
            (
                content.to_string(),
                original_tokens,
                vec!["passthrough".to_string()],
            )
        }
        _ => {
            // Strategy not enabled or unknown — passthrough, with dense-line
            // elision as the last resort (mirrors the try_kompress tail).
            if config.enable_dense_line_elision {
                // elide_dense char-gates internally; Some means shorter.
                if let Some((elided, elided_tokens)) =
                    elide_dense(content, context, config, store_recoverable)
                {
                    return (
                        elided,
                        elided_tokens,
                        vec!["passthrough".to_string(), "dense_elide".to_string()],
                    );
                }
            }
            (
                content.to_string(),
                original_tokens,
                vec!["passthrough".to_string()],
            )
        }
    };

    // Dense-line elision on the RESULT: whatever survived the chain, long
    // whitespace-free lines are still in it. Only after strategies that hand
    // back plain text — SmartCrusher, tabular, config, and diff emit their
    // own structured (and CCR-marked) forms; Kompress is lossy with its own
    // marker. Eliding inside those would corrupt output another owns.
    // Runs only when the strategy itself produced the result (the chain
    // ends with it), matching the Python `_DENSE_ELIDE_AFTER` gate: CodeAware
    // that fell back to Kompress hands back Kompress output. An HTML page
    // that extracted nothing hands back the original, which Python sends
    // down its passthrough path, dense elision included.
    let (compressed_out, tokens_out, mut chain_out) = strategy_result;
    let produced_by = chain_out.last().map(String::as_str);
    let strategy_produced = produced_by == Some(strategy.as_str())
        || (strategy == CompressionStrategy::Html
            && matches!(produced_by, Some("html_extractor" | "passthrough")));
    if config.enable_dense_line_elision && dense_elide_after(strategy) && strategy_produced {
        // elide_dense char-gates internally; Some means shorter.
        if let Some((elided, elided_tokens)) =
            elide_dense(&compressed_out, context, config, store_recoverable)
        {
            chain_out.push("dense_elide".to_string());
            return (elided, elided_tokens, chain_out);
        }
    }
    (compressed_out, tokens_out, chain_out)
}

/// Try Kompress ML compression. Returns (compressed, tokens, chain).
/// Kompress requires ONNX model — falls through to passthrough if not available.
/// Default ceiling, in tokens, above which a block is routed off ML.
///
/// Matches Python's `HEADROOM_KOMPRESS_MAX_TOKENS` default.
pub const DEFAULT_KOMPRESS_MAX_TOKENS: usize = 50_000;

/// The configured Kompress size ceiling in tokens; `0` disables the gate.
///
/// Read from `HEADROOM_KOMPRESS_MAX_TOKENS`, falling back to
/// [`DEFAULT_KOMPRESS_MAX_TOKENS`] when unset or unparseable — matching Python's
/// `int(os.environ.get(...))` with its bare-except fallback.
pub fn kompress_max_tokens() -> usize {
    std::env::var("HEADROOM_KOMPRESS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_KOMPRESS_MAX_TOKENS)
}

/// Whether `content` is too large to hand to the ML compressor.
///
/// Kompress ONNX inference is O(tokens) and runs synchronously on the request
/// thread. On a large or cold context it blows the request budget and leaks a
/// worker that cannot be preempted, so oversized blocks are routed to a cheap
/// compressor instead. The ceiling is compared against a chars≈tokens*4
/// estimate rather than a real token count, because tokenizing the block to
/// decide whether it is too expensive to tokenize would defeat the purpose.
///
/// Mirrors Python's `len(text) > self._kompress_max_tokens * 4` check.
pub fn kompress_size_gate_exceeded(content: &str) -> bool {
    let max_tokens = kompress_max_tokens();
    max_tokens > 0 && content.len() > max_tokens * 4
}

fn try_kompress(
    content: &str,
    config: &ContentRouterConfig,
    context: &str,
    chain: &[String],
    store_recoverable: &dyn Fn(&str, &str, &str) -> bool,
) -> (String, usize, Vec<String>) {
    let original_tokens = content.split_whitespace().count();

    // ── Size gate: the single ML boundary in this module ─────────────────
    // Above the ceiling, fall back to the cheap TextCrusher rather than
    // ModernBERT, keeping the request path bounded.
    if kompress_size_gate_exceeded(content) {
        super::observability::observe_kompress_size_gate("exceeded");
        tracing::info!(
            approx_tokens = content.len() / 4,
            ceiling = kompress_max_tokens(),
            "kompress size-gate fired; routing off ML"
        );
        let crusher = super::text_crusher::TextCrusher::new(Default::default());
        let crushed = crusher.compress(content, context, None).compressed;
        let tokens = crushed.split_whitespace().count();
        let mut gated_chain = chain.to_vec();
        gated_chain.push("kompress_size_gate".to_string());
        return (crushed, tokens, gated_chain);
    }
    if kompress_max_tokens() > 0 {
        // The counterpart outcome, so the gate's hit rate is measurable.
        super::observability::observe_kompress_size_gate("within");
    }

    // Try to load and run Kompress
    #[cfg(feature = "ml")]
    match super::kompress::Kompress::from_cache(super::kompress::KompressConfig::default()) {
        Ok(Some(kompress)) => {
            let result = kompress.compress(content);
            let tokens = result.compressed.split_whitespace().count();
            // Only adopt if Kompress actually saved tokens
            if tokens < original_tokens {
                let mut full_chain = chain.to_vec();
                full_chain.push("kompress".to_string());
                return (result.compressed, tokens, full_chain);
            }
        }
        Ok(None) => {
            // Model not cached — try downloading
            // Download failed: fall through.
            if let Ok(kompress) = super::kompress::Kompress::from_pretrained(
                super::kompress::KompressConfig::default(),
            ) {
                let result = kompress.compress(content);
                let tokens = result.compressed.split_whitespace().count();
                if tokens < original_tokens {
                    let mut full_chain = chain.to_vec();
                    full_chain.push("kompress".to_string());
                    return (result.compressed, tokens, full_chain);
                }
            }
        }
        Err(_) => {} // Load failed — fall through
    }

    // Kompress not available or didn't help — dense-line elision as the
    // last resort before passthrough (minified bundles, base64, RSC
    // payloads: no structural compressor understands them).
    // elide_dense char-gates internally; Some means shorter.
    if config.enable_dense_line_elision
        && let Some((elided, elided_tokens)) =
            elide_dense(content, context, config, store_recoverable)
    {
        let mut full_chain = chain.to_vec();
        full_chain.push("dense_elide".to_string());
        return (elided, elided_tokens, full_chain);
    }

    // Kompress not available or didn't help — passthrough
    let mut full_chain = chain.to_vec();
    full_chain.push("passthrough".to_string());
    (content.to_string(), original_tokens, full_chain)
}

// ─── Relevance split ─────────────────────────────────────────────────────

/// Partition content into coherent records for relevance scoring.
///
/// Lossless partition: joining all segments reproduces the original content.
/// Blank lines delimit records; oversized blocks are packed into windows.
/// Indented continuation lines stay attached to their window so stack traces
/// and pretty-printed JSON aren't split mid-unit.
pub fn segment(content: &str, window: usize, max_chars: usize) -> Vec<String> {
    if content.is_empty() {
        return vec![];
    }

    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() <= 1 {
        return vec![content.to_string()];
    }

    // Pass 1: blank-line-delimited blocks
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for ln in &lines {
        cur.push(ln);
        if ln.trim().is_empty() {
            blocks.push(cur);
            cur = Vec::new();
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    // Pass 2: pack/window each block with continuation-line awareness
    let mut segments: Vec<String> = Vec::new();
    for block in &blocks {
        let block_len: usize = block.iter().map(|l| l.len()).sum();
        if block.len() <= window && block_len <= max_chars {
            segments.push(block.join("\n"));
            continue;
        }

        // Dense block — pack into fixed windows, keeping indented continuations attached
        let mut i = 0;
        while i < block.len() {
            let mut window_lines: Vec<&str> = Vec::new();
            let mut window_chars = 0;
            while i < block.len()
                && window_lines.len() < window
                && window_chars + block[i].len() <= max_chars
            {
                window_lines.push(block[i]);
                window_chars += block[i].len();
                i += 1;
                // Python: `while j < n and block[j][:1] in (" ", "\t"): j += 1`
                // Extend window to include indented continuation lines
                while i < block.len() && block[i].starts_with([' ', '\t']) {
                    window_lines.push(block[i]);
                    window_chars += block[i].len();
                    i += 1;
                }
            }
            if window_lines.is_empty() {
                // Single line exceeds max_chars — take it anyway
                window_lines.push(block[i]);
                i += 1;
            }
            segments.push(window_lines.join("\n"));
        }
    }

    segments
}

/// Otsu's method: find the cut between two classes that maximizes
/// between-class variance. Parameter-free.
fn otsu_threshold(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut xs: Vec<f64> = values.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let n = xs.len() as f64;
    let total: f64 = xs.iter().sum();
    let mut w0 = 0.0;
    let mut sum0 = 0.0;
    let mut best_t = xs[0];
    let mut best_var = -1.0;

    for i in 0..xs.len() - 1 {
        w0 += 1.0;
        sum0 += xs[i];
        let w1 = n - w0;
        let m0 = sum0 / w0;
        let m1 = (total - sum0) / w1;
        let between = w0 * w1 * (m0 - m1).powi(2);
        if between > best_var {
            best_var = between;
            best_t = (xs[i] + xs[i + 1]) / 2.0;
        }
    }
    best_t
}

/// Data-driven KEEP/DROP cut for one output's relevance scores.
///
/// Uses Otsu's method to find the natural break, floored by `floor`.
pub fn adaptive_threshold(values: &[f64], floor: f64) -> f64 {
    // Check if all values are the same
    let unique_count = values
        .iter()
        .map(|v| (v * 1e9).round() as i64)
        .collect::<HashSet<_>>()
        .len();
    if unique_count < 2 {
        return floor;
    }
    f64::max(otsu_threshold(values), floor)
}

/// A single run in the relevance split output.
#[derive(Debug, Clone)]
pub struct RelevanceRun {
    /// Whether this run should be kept verbatim.
    pub keep: bool,
    /// The text content of this run.
    pub text: String,
}

/// Split content into ordered runs by relevance to query.
///
/// Keeps high-relevance records verbatim and drops low-relevance ones.
/// Consecutive same-disposition records are merged into runs.
pub fn plan_relevance_split(
    content: &str,
    query: &str,
    scores: &[f64],
    threshold: f64,
    adaptive: bool,
    max_records: Option<usize>,
) -> Vec<RelevanceRun> {
    if query.trim().is_empty() || scores.is_empty() {
        return vec![RelevanceRun {
            keep: true,
            text: content.to_string(),
        }];
    }

    let segs = segment(content, 8, 1200);
    if segs.len() < 2 || max_records.is_some_and(|m| segs.len() > m) {
        return vec![RelevanceRun {
            keep: true,
            text: content.to_string(),
        }];
    }

    if scores.len() != segs.len() {
        return vec![RelevanceRun {
            keep: true,
            text: content.to_string(),
        }];
    }

    let cut = if adaptive {
        adaptive_threshold(scores, threshold)
    } else {
        threshold
    };

    let mut runs: Vec<RelevanceRun> = Vec::new();
    for (seg, &score) in segs.iter().zip(scores.iter()) {
        let keep = score >= cut;
        if let Some(last) = runs.last_mut()
            && last.keep == keep
        {
            last.text.push('\n');
            last.text.push_str(seg);
            continue;
        }
        runs.push(RelevanceRun {
            keep,
            text: seg.clone(),
        });
    }

    runs
}

// ─── CompressionCache ────────────────────────────────────────────────────

/// Two-tier compression cache with TTL. Thread-safe.
///
/// Tier 1 (skip set): content hashes that won't compress — instant skip.
/// Tier 2 (result cache): compressed results for content that DID compress.
///
/// Entries expire after TTL (default 30min).
pub struct CompressionCache {
    results: Mutex<HashMap<i64, CacheEntry>>,
    skip: Mutex<HashMap<i64, Instant>>,
    ttl: Duration,
    // Metrics
    hits: Mutex<u64>,
    misses: Mutex<u64>,
    skip_hits: Mutex<u64>,
    evictions: Mutex<u64>,
    // Frozen per-block verdicts (see `record_frozen_verdict`). Owned by the
    // cache rather than a router because the keys are cache content keys and
    // the verdicts MUST die with the entries they describe — Python wires this
    // up via `register_on_clear`; here `clear()` drops both together.
    frozen: Mutex<FrozenVerdicts>,
    freeze_pin_hits: Mutex<u64>,
    freeze_pin_chars: Mutex<u64>,
}

/// Bounded FIFO verdict store. `order` mirrors Python's reliance on dict
/// insertion order for eviction.
#[derive(Default)]
struct FrozenVerdicts {
    verdicts: HashMap<i64, bool>,
    order: VecDeque<i64>,
}

/// Cap on retained verdicts, so a long-lived process cannot grow without bound.
const FROZEN_VERDICTS_MAX: usize = 4096;

/// Whether the per-block verdict freeze is active (default OFF).
///
/// Read from the environment on every call so it can be toggled per-process
/// without a restart in tests. Off → the verdict store is never touched and
/// behaviour is byte-identical to the unfrozen path.
pub fn freeze_block_decision_enabled() -> bool {
    matches!(
        std::env::var("HEADROOM_FREEZE_BLOCK_DECISION")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether a "compress" verdict is safe to freeze under the #1307 rule.
///
/// A lossy-unmarked strategy that emitted no CCR retrieval marker is
/// unrecoverable, so pinning it across turns would keep serving a fabricated
/// summary the agent cannot restore. Refuse to freeze those; recoverable
/// (marked, or simply not lossy) compressions may be pinned.
///
/// `strategy` is the strategy's string value — the fresh-compress path has a
/// `CompressionStrategy` and the cache-hit path only its label, so both sides
/// compare by value exactly as Python does.
pub fn frozen_verdict_recoverable(strategy: &str, compressed: Option<&str>) -> bool {
    if super::compression_units::lossy_unmarked_strategies().contains(strategy) {
        return super::compression_units::ccr_marker_re().is_match(compressed.unwrap_or(""));
    }
    true
}

struct CacheEntry {
    compressed: String,
    ratio: f64,
    strategy: String,
    created_at: Instant,
}

/// Result of a cache lookup.
pub enum CacheLookup {
    Hit {
        compressed: String,
        ratio: f64,
        strategy: String,
    },
    Miss,
    Skip,
}

impl CompressionCache {
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            results: Mutex::new(HashMap::new()),
            skip: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_seconds),
            hits: Mutex::new(0),
            misses: Mutex::new(0),
            skip_hits: Mutex::new(0),
            evictions: Mutex::new(0),
            frozen: Mutex::new(FrozenVerdicts::default()),
            freeze_pin_hits: Mutex::new(0),
            freeze_pin_chars: Mutex::new(0),
        }
    }

    /// Record a frozen verdict for a block, with bounded FIFO eviction.
    ///
    /// Once a block's "compress" verdict is frozen, the cache-hit path stops
    /// re-checking it against a per-turn `min_ratio`. That re-check is what
    /// would otherwise downgrade an already-compressed block to skip on a later
    /// turn, restoring the original bytes and busting the provider's prefix
    /// cache for everything after it.
    pub fn record_frozen_verdict(&self, key: i64, verdict: bool) {
        let mut frozen = self.frozen.lock().unwrap();
        if !frozen.verdicts.contains_key(&key)
            && frozen.verdicts.len() >= FROZEN_VERDICTS_MAX
            && let Some(oldest) = frozen.order.pop_front()
        {
            frozen.verdicts.remove(&oldest);
        }
        if frozen.verdicts.insert(key, verdict).is_none() {
            frozen.order.push_back(key);
        }
    }

    /// Read a frozen verdict. `None` when the block has never been frozen.
    pub fn frozen_verdict(&self, key: i64) -> Option<bool> {
        self.frozen.lock().unwrap().verdicts.get(&key).copied()
    }

    /// Count one freeze divergence: a frozen "compress" verdict overrode a
    /// tightened `min_ratio` that would have downgraded the block.
    ///
    /// `preserved` is a char-based proxy for the saving kept alive by not
    /// reverting to the original.
    pub fn record_freeze_pin(&self, content: &str, cached_ratio: f64) {
        let preserved = ((content.len() as f64) * (1.0 - cached_ratio)).max(0.0) as u64;
        let hits = {
            let mut h = self.freeze_pin_hits.lock().unwrap();
            *h += 1;
            *self.freeze_pin_chars.lock().unwrap() += preserved;
            *h
        };
        tracing::info!(
            event = "freeze_pin",
            pins = hits,
            cached_ratio = cached_ratio,
            preserved_chars = preserved,
            "FREEZE-PIN: frozen verdict avoided a cache bust"
        );
    }

    /// `(pins, preserved_chars)` — observability for the freeze pin.
    pub fn freeze_pin_stats(&self) -> (u64, u64) {
        (
            *self.freeze_pin_hits.lock().unwrap(),
            *self.freeze_pin_chars.lock().unwrap(),
        )
    }

    /// Drop all frozen verdicts. Fired on cache clear.
    pub fn clear_frozen_verdicts(&self) {
        let mut frozen = self.frozen.lock().unwrap();
        frozen.verdicts.clear();
        frozen.order.clear();
    }

    /// The accept threshold for a cache-hit block.
    ///
    /// A frozen "compress" verdict pins this to 1.0 — already decided, always
    /// accept — bypassing the per-turn `min_ratio` re-check. Unfrozen blocks get
    /// the live gate, identical to the flag-off path.
    pub fn accept_threshold(&self, key: i64, min_ratio: f64) -> f64 {
        if freeze_block_decision_enabled() && self.frozen_verdict(key) == Some(true) {
            1.0
        } else {
            min_ratio
        }
    }

    /// Get cached compression result. Returns CacheLookup::Hit/Miss/Skip.
    pub fn get(&self, key: i64) -> CacheLookup {
        // Check skip set first
        {
            let mut skip = self.skip.lock().unwrap();
            if let Some(ts) = skip.get(&key) {
                if ts.elapsed() < self.ttl {
                    *self.skip_hits.lock().unwrap() += 1;
                    return CacheLookup::Skip;
                } else {
                    skip.remove(&key);
                    *self.evictions.lock().unwrap() += 1;
                }
            }
        }

        // Check result cache
        let mut results = self.results.lock().unwrap();
        if let Some(entry) = results.get(&key) {
            if entry.created_at.elapsed() < self.ttl {
                *self.hits.lock().unwrap() += 1;
                return CacheLookup::Hit {
                    compressed: entry.compressed.clone(),
                    ratio: entry.ratio,
                    strategy: entry.strategy.clone(),
                };
            } else {
                results.remove(&key);
                *self.evictions.lock().unwrap() += 1;
            }
        }

        *self.misses.lock().unwrap() += 1;
        CacheLookup::Miss
    }

    /// Check if content is known non-compressible (Tier 1).
    pub fn is_skipped(&self, key: i64) -> bool {
        let mut skip = self.skip.lock().unwrap();
        if let Some(ts) = skip.get(&key) {
            if ts.elapsed() < self.ttl {
                *self.skip_hits.lock().unwrap() += 1;
                return true;
            } else {
                skip.remove(&key);
                *self.evictions.lock().unwrap() += 1;
            }
        }
        false
    }

    /// Store a compressed result (Tier 2).
    pub fn put(&self, key: i64, compressed: &str, ratio: f64, strategy: &str) {
        let mut results = self.results.lock().unwrap();
        results.insert(
            key,
            CacheEntry {
                compressed: compressed.to_string(),
                ratio,
                strategy: strategy.to_string(),
                created_at: Instant::now(),
            },
        );
    }

    /// Mark content as non-compressible (Tier 1).
    pub fn mark_skip(&self, key: i64) {
        let mut skip = self.skip.lock().unwrap();
        skip.insert(key, Instant::now());
    }

    /// Move a result to skip set (threshold tightened).
    pub fn move_to_skip(&self, key: i64) {
        self.results.lock().unwrap().remove(&key);
        self.skip.lock().unwrap().insert(key, Instant::now());
    }

    /// Number of cached results.
    pub fn size(&self) -> usize {
        self.results.lock().unwrap().len()
    }

    /// Number of skipped entries.
    pub fn skip_size(&self) -> usize {
        self.skip.lock().unwrap().len()
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        let hits = *self.hits.lock().unwrap();
        let misses = *self.misses.lock().unwrap();
        let skip_hits = *self.skip_hits.lock().unwrap();
        let evictions = *self.evictions.lock().unwrap();
        let size = self.results.lock().unwrap().len();
        let skip_size = self.skip.lock().unwrap().len();
        CacheStats {
            hits,
            misses,
            skip_hits,
            evictions,
            size,
            skip_size,
        }
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.results.lock().unwrap().clear();
        self.skip.lock().unwrap().clear();
        // Verdicts describe entries that no longer exist; keeping them would
        // pin decisions about content the cache has forgotten. Python wires
        // this through `register_on_clear`.
        self.clear_frozen_verdicts();
    }
}

/// Cache statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub skip_hits: u64,
    pub evictions: u64,
    pub size: usize,
    pub skip_size: usize,
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// The default config with `edit` applied.
#[cfg(test)]
fn config_with(edit: impl FnOnce(&mut ContentRouterConfig)) -> ContentRouterConfig {
    let mut config = ContentRouterConfig::default();
    edit(&mut config);
    config
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod external_compressor_tests;

#[cfg(test)]
mod kompress_size_gate_tests;
