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
    CompressInput, Compressor, CompressorDescriptor, CompressorRegistry,
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
    /// SHA-256[:24] of sorted field names + types.
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
            // Generate unique hash for empty outputs
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let structure_hash = create_content_signature("empty", &ts.to_string(), None);
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

        // Merge field info from all sampled items
        let mut all_fields: HashMap<String, Vec<String>> = HashMap::new();
        for item in &sample_items {
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
        let mut string_count = 0;
        let mut numeric_count = 0;
        let mut boolean_count = 0;
        let mut array_count = 0;
        let mut object_count = 0;
        let mut has_nested = false;
        let mut has_arrays = false;
        let mut max_depth = 1;

        // Pattern detection
        let mut has_id = false;
        let mut has_score = false;
        let mut has_timestamp = false;
        let mut has_status = false;
        let mut has_error = false;
        let mut has_message = false;

        for item in &sample_items {
            let d = Self::calculate_depth(item);
            if d > max_depth {
                max_depth = d;
            }
        }

        for (key, types) in &all_fields {
            let types_no_null: Vec<&str> = types
                .iter()
                .filter(|t| *t != "null")
                .map(|s| s.as_str())
                .collect();

            let field_type = if types_no_null.len() == 1 {
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
            };

            match field_type.as_str() {
                "string" => string_count += 1,
                "boolean" => boolean_count += 1,
                "numeric" => numeric_count += 1,
                "array" => {
                    array_count += 1;
                    has_arrays = true;
                }
                "object" => {
                    object_count += 1;
                    has_nested = true;
                }
                _ => {}
            }

            // Pattern detection
            let key_lower = key.to_lowercase();
            if Self::matches_pattern(&key_lower, &["id", "uuid", "guid"])
                || key_lower.ends_with("key")
            {
                has_id = true;
            }
            if Self::matches_pattern(
                &key_lower,
                &["score", "rank", "rating", "relevance", "priority"],
            ) {
                has_score = true;
            }
            if Self::matches_pattern(&key_lower, &["time", "date", "timestamp"])
                || key_lower.ends_with("_at")
                || key_lower == "created"
                || key_lower == "updated"
            {
                has_timestamp = true;
            }
            if Self::matches_pattern(&key_lower, &["status", "state"])
                || key_lower == "level"
                || key_lower == "type"
                || key_lower == "kind"
            {
                has_status = true;
            }
            if Self::matches_pattern(&key_lower, &["error", "exception", "fail", "warning"]) {
                has_error = true;
            }
            if Self::matches_pattern(
                &key_lower,
                &["message", "msg", "text", "content", "body", "description"],
            ) {
                has_message = true;
            }

            field_info.push((key.clone(), field_type));
        }

        // Create structure hash (matching Python's json.dumps(sorted_fields, sort_keys=True))
        field_info.sort_by(|a, b| a.0.cmp(&b.0));
        let hash_input = serde_json::to_string(&field_info).unwrap_or_default();
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(hash_input.as_bytes());
        let structure_hash = hex::encode(hasher.finalize())[..24].to_string();

        Self {
            structure_hash,
            field_count: field_info.len(),
            has_nested_objects: has_nested,
            has_arrays,
            max_depth,
            string_field_count: string_count,
            numeric_field_count: numeric_count,
            boolean_field_count: boolean_count,
            array_field_count: array_count,
            object_field_count: object_count,
            has_id_like_field: has_id,
            has_score_like_field: has_score,
            has_timestamp_like_field: has_timestamp,
            has_status_like_field: has_status,
            has_error_like_field: has_error,
            has_message_like_field: has_message,
        }
    }

    fn calculate_depth(json: &Value) -> usize {
        match json {
            Value::Object(map) => {
                let inner = map
                    .values()
                    .map(|v| Self::calculate_depth(v))
                    .max()
                    .unwrap_or(0);
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
                let mut arrays = false;
                let mut max_d = depth;
                if let Some(first) = arr.first() {
                    let (_, n, a, d) = Self::analyze_json(first, depth + 1);
                    nested = n;
                    arrays = a;
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
    let toks: Vec<&str> = command.trim().split_whitespace().collect();
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

fn prose_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Z][a-z]+\s+\w+\s+\w+").expect("PROSE_PATTERN is valid"))
}

/// Detect if content contains multiple distinct types.
pub fn is_mixed_content(content: &str) -> bool {
    let indicators = [
        code_fence_pattern().is_match(content),
        json_block_start().is_match(content),
        prose_pattern().find_iter(content).count() > 5,
        search_result_pattern().is_match(content),
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
            } else if rc_content.starts_with('-') {
                rc_content[1..].chars().all(|c| c.is_ascii_digit())
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

    for i in start..lines.len() {
        let line = lines[i];
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
    let search_re = search_result_pattern();

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
        if search_re.is_match(line) {
            let mut search_lines: Vec<&str> = Vec::new();
            let start_line = i;
            while i < lines.len() && search_re.is_match(lines[i]) {
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
                || search_re.is_match(next_line)
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
    if result.content_type == ContentType::Html {
        if let Some(override_result) = super::content_detector::try_detect_log(&stripped)
            .or_else(|| super::content_detector::try_detect_search(&stripped))
        {
            return override_result.content_type;
        }
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
        ContentType::Tabular => CompressionStrategy::Kompress,
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

    // Never blank out a non-empty block (an empty user/tool block makes
    // providers reject the request); fall back so the built-in path runs.
    if !content.trim().is_empty() && out.content.trim().is_empty() {
        tracing::warn!(
            compressor = %name,
            "external compressor produced empty output; falling back to built-in"
        );
        return None;
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
        return None;
    }

    // Count with the router's OWN estimator, not the compressor's self-report.
    let compressed_tokens = out.content.split_whitespace().count();

    // Persist the hash -> original recovery map so a later /v1/retrieve resolves
    // each hash. Best-effort: a store failure leaves that entry unretrievable
    // but never breaks the request.
    let strategy_label = format!("external:{name}");
    for (ccr_hash, original) in &out.recoverable {
        if !store_recoverable(ccr_hash, original, &strategy_label) {
            tracing::warn!(
                compressor = %name,
                hash = %ccr_hash,
                "external compressor recoverable entry was not stored"
            );
        }
    }

    if !out.warnings.is_empty() {
        tracing::debug!(
            compressor = %name,
            warnings = %out.warnings.join("; "),
            "external compressor warnings"
        );
    }

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
            let (komp, komp_tokens, _chain) =
                try_kompress(&ll_content, config, context, &["kompress".to_string()]);
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

    match strategy {
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
                return try_kompress(content, config, context, &chain);
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
            if !compressed.is_empty() && tokens < original_tokens {
                return (compressed, tokens, vec!["html_extractor".to_string()]);
            }
            (
                content.to_string(),
                original_tokens,
                vec!["html_extractor".to_string(), "passthrough".to_string()],
            )
        }
        CompressionStrategy::Kompress | CompressionStrategy::Text if config.enable_kompress => {
            try_kompress(content, config, context, &["kompress".to_string()])
        }
        CompressionStrategy::Passthrough => (
            content.to_string(),
            original_tokens,
            vec!["passthrough".to_string()],
        ),
        _ => {
            // Strategy not enabled or unknown — passthrough
            (
                content.to_string(),
                original_tokens,
                vec!["passthrough".to_string()],
            )
        }
    }
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
            match super::kompress::Kompress::from_pretrained(
                super::kompress::KompressConfig::default(),
            ) {
                Ok(kompress) => {
                    let result = kompress.compress(content);
                    let tokens = result.compressed.split_whitespace().count();
                    if tokens < original_tokens {
                        let mut full_chain = chain.to_vec();
                        full_chain.push("kompress".to_string());
                        return (result.compressed, tokens, full_chain);
                    }
                }
                Err(_) => {} // Download failed — fall through
            }
        }
        Err(_) => {} // Load failed — fall through
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
                while i < block.len() && block[i].starts_with(|c: char| c == ' ' || c == '\t') {
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
    if segs.len() < 2 || max_records.map_or(false, |m| segs.len() > m) {
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
        if let Some(last) = runs.last_mut() {
            if last.keep == keep {
                last.text.push('\n');
                last.text.push_str(seg);
                continue;
            }
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
        if !frozen.verdicts.contains_key(&key) && frozen.verdicts.len() >= FROZEN_VERDICTS_MAX {
            if let Some(oldest) = frozen.order.pop_front() {
                frozen.verdicts.remove(&oldest);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- bash_program ---

    #[test]
    fn bash_program_simple() {
        let (prog, args) = bash_program("grep foo bar.txt");
        assert_eq!(prog, "grep");
        assert_eq!(args, vec!["foo", "bar.txt"]);
    }

    #[test]
    fn bash_program_with_wrapper() {
        let (prog, args) = bash_program("rtk grep pattern");
        assert_eq!(prog, "grep");
        assert_eq!(args, vec!["pattern"]);
    }

    #[test]
    fn bash_program_with_env() {
        let (prog, args) = bash_program("FOO=1 BAR=2 grep pattern");
        assert_eq!(prog, "grep");
        assert_eq!(args, vec!["pattern"]);
    }

    #[test]
    fn bash_program_timeout_with_args() {
        let (prog, args) = bash_program("timeout 30 rg pattern");
        assert_eq!(prog, "rg");
        assert_eq!(args, vec!["pattern"]);
    }

    #[test]
    fn bash_program_full_path() {
        let (prog, _args) = bash_program("/usr/bin/grep pattern");
        assert_eq!(prog, "grep");
    }

    // --- bash_command_is_search ---

    #[test]
    fn bash_command_is_search_true() {
        let cmds: HashSet<&str> = ["grep", "rg", "ripgrep", "ag", "ack"]
            .iter()
            .copied()
            .collect();
        assert!(bash_command_is_search("grep pattern", &cmds));
        assert!(bash_command_is_search("rg --json pattern", &cmds));
        assert!(bash_command_is_search("git grep pattern", &cmds));
    }

    #[test]
    fn bash_command_is_search_false() {
        let cmds: HashSet<&str> = ["grep", "rg"].iter().copied().collect();
        assert!(!bash_command_is_search("cat file.txt", &cmds));
        assert!(!bash_command_is_search("ls -la", &cmds));
    }

    #[test]
    fn bash_command_is_search_bash_c() {
        let cmds: HashSet<&str> = ["grep", "rg"].iter().copied().collect();
        assert!(bash_command_is_search("bash -c 'grep pattern file'", &cmds));
    }

    // --- is_mixed_content ---

    #[test]
    fn is_mixed_content_true() {
        let content = "# Title\n\n```python\ndef foo(): pass\n```\n\nSome prose here about something.\nMore prose.\nAnd more.\nAnd more.\nAnd more.\nAnd more.\nsrc/file.py:42: code";
        assert!(is_mixed_content(content));
    }

    #[test]
    fn is_mixed_content_false_pure_code() {
        let content = "def foo():\n    pass\n\ndef bar():\n    pass";
        assert!(!is_mixed_content(content));
    }

    #[test]
    fn is_mixed_content_false_pure_json() {
        let content = r#"[{"key": "value"}]"#;
        assert!(!is_mixed_content(content));
    }

    // --- json_shape ---

    #[test]
    fn json_shape_object() {
        let shape = json_shape(r#"{"a": 1, "b": 2}"#);
        assert_eq!(shape["is_json"], true);
        assert_eq!(shape["kind"], "object");
        assert_eq!(shape["length"], 2);
    }

    #[test]
    fn json_shape_array() {
        let shape = json_shape("[1, 2, 3]");
        assert_eq!(shape["is_json"], true);
        assert_eq!(shape["kind"], "array");
        assert_eq!(shape["length"], 3);
    }

    #[test]
    fn json_shape_invalid() {
        let shape = json_shape("not json");
        assert_eq!(shape["is_json"], false);
    }

    // --- gain_bucket ---

    #[test]
    fn gain_bucket_zero() {
        assert_eq!(gain_bucket(0.0), "0");
    }

    #[test]
    fn gain_bucket_small_positive() {
        assert_eq!(gain_bucket(50.0), "lt100");
    }

    #[test]
    fn gain_bucket_small_negative() {
        assert_eq!(gain_bucket(-50.0), "neg_lt100");
    }

    #[test]
    fn gain_bucket_medium() {
        assert_eq!(gain_bucket(500.0), "lt1k");
    }

    #[test]
    fn gain_bucket_large() {
        assert_eq!(gain_bucket(5000.0), "lt10k");
    }

    #[test]
    fn gain_bucket_very_large() {
        assert_eq!(gain_bucket(50000.0), "gte10k");
    }

    #[test]
    fn gain_bucket_nan() {
        assert_eq!(gain_bucket(f64::NAN), "nan");
    }

    // --- RoutingDecision ---

    #[test]
    fn routing_decision_ratio() {
        let d = RoutingDecision {
            content_type: ContentType::PlainText,
            strategy: CompressionStrategy::Kompress,
            original_tokens: 100,
            compressed_tokens: 50,
            confidence: 1.0,
            section_index: 0,
        };
        assert_eq!(d.compression_ratio(), 0.5);
    }

    #[test]
    fn routing_decision_ratio_zero_original() {
        let d = RoutingDecision {
            content_type: ContentType::PlainText,
            strategy: CompressionStrategy::Kompress,
            original_tokens: 0,
            compressed_tokens: 0,
            confidence: 1.0,
            section_index: 0,
        };
        assert_eq!(d.compression_ratio(), 1.0);
    }

    // --- RouterCompressionResult ---

    #[test]
    fn router_result_totals() {
        let r = RouterCompressionResult {
            compressed: "a b".to_string(),
            original: "a b c d".to_string(),
            strategy_used: CompressionStrategy::Mixed,
            routing_log: vec![
                RoutingDecision {
                    content_type: ContentType::PlainText,
                    strategy: CompressionStrategy::Kompress,
                    original_tokens: 100,
                    compressed_tokens: 50,
                    confidence: 1.0,
                    section_index: 0,
                },
                RoutingDecision {
                    content_type: ContentType::JsonArray,
                    strategy: CompressionStrategy::SmartCrusher,
                    original_tokens: 200,
                    compressed_tokens: 80,
                    confidence: 1.0,
                    section_index: 1,
                },
            ],
            sections_processed: 2,
            strategy_chain: vec!["kompress".to_string(), "smart_crusher".to_string()],
            cache_hit: false,
        };
        assert_eq!(r.total_original_tokens(), 300);
        assert_eq!(r.total_compressed_tokens(), 130);
        assert!((r.compression_ratio() - 0.433).abs() < 0.01);
        assert_eq!(r.tokens_saved(), 170);
    }

    #[test]
    fn router_result_summary_mixed() {
        let r = RouterCompressionResult {
            compressed: String::new(),
            original: String::new(),
            strategy_used: CompressionStrategy::Mixed,
            routing_log: vec![],
            sections_processed: 3,
            strategy_chain: vec![],
            cache_hit: false,
        };
        let s = r.summary();
        assert!(s.contains("Mixed content"));
        assert!(s.contains("3 sections"));
    }

    #[test]
    fn router_result_summary_pure() {
        let r = RouterCompressionResult {
            compressed: String::new(),
            original: String::new(),
            strategy_used: CompressionStrategy::Search,
            routing_log: vec![RoutingDecision {
                content_type: ContentType::SearchResults,
                strategy: CompressionStrategy::Search,
                original_tokens: 200,
                compressed_tokens: 100,
                confidence: 1.0,
                section_index: 0,
            }],
            sections_processed: 1,
            strategy_chain: vec![],
            cache_hit: false,
        };
        let s = r.summary();
        assert!(s.contains("Pure search"));
    }

    // --- CompressionCache ---

    #[test]
    fn cache_put_and_get() {
        let cache = CompressionCache::new(1800);
        cache.put(42, "compressed text", 0.5, "kompress");

        match cache.get(42) {
            CacheLookup::Hit {
                compressed,
                ratio,
                strategy,
            } => {
                assert_eq!(compressed, "compressed text");
                assert_eq!(ratio, 0.5);
                assert_eq!(strategy, "kompress");
            }
            _ => panic!("expected cache hit"),
        }
    }

    #[test]
    fn cache_miss() {
        let cache = CompressionCache::new(1800);
        assert!(matches!(cache.get(999), CacheLookup::Miss));
    }

    #[test]
    fn cache_skip() {
        let cache = CompressionCache::new(1800);
        cache.mark_skip(42);
        assert!(cache.is_skipped(42));
        assert!(matches!(cache.get(42), CacheLookup::Skip));
    }

    #[test]
    fn cache_move_to_skip() {
        let cache = CompressionCache::new(1800);
        cache.put(42, "compressed", 0.5, "kompress");
        cache.move_to_skip(42);
        assert!(!matches!(cache.get(42), CacheLookup::Hit { .. }));
        assert!(cache.is_skipped(42));
    }

    #[test]
    fn cache_stats() {
        let cache = CompressionCache::new(1800);
        cache.put(1, "a", 0.5, "log");
        cache.put(2, "b", 0.6, "search");
        cache.mark_skip(3);

        let _ = cache.get(1); // hit
        let _ = cache.get(999); // miss
        let _ = cache.get(3); // skip hit

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.skip_hits, 1);
        assert_eq!(stats.size, 2);
        assert_eq!(stats.skip_size, 1);
    }

    #[test]
    fn cache_clear() {
        let cache = CompressionCache::new(1800);
        cache.put(1, "a", 0.5, "log");
        cache.mark_skip(2);
        cache.clear();
        assert_eq!(cache.size(), 0);
        assert_eq!(cache.skip_size(), 0);
    }

    // --- tool_call_args_text ---

    #[test]
    fn tool_call_args_text_string() {
        let raw = json!("grep pattern file.txt");
        assert_eq!(tool_call_args_text(&raw), "grep pattern file.txt");
    }

    #[test]
    fn tool_call_args_text_dict() {
        let raw = json!({"command": "grep pattern", "path": "/tmp"});
        let text = tool_call_args_text(&raw);
        assert!(text.contains("grep pattern"));
        assert!(text.contains("/tmp"));
    }

    #[test]
    fn tool_call_args_text_capped() {
        let long = "word ".repeat(200);
        let raw = json!(long);
        let text = tool_call_args_text(&raw);
        assert!(text.len() <= 300);
    }

    #[test]
    fn tool_call_args_text_non_scalar_values() {
        let raw = json!({"nested": {"key": "val"}, "simple": "ok"});
        let text = tool_call_args_text(&raw);
        assert!(text.contains("ok"));
        assert!(!text.contains("nested"));
    }

    // --- tool_call_command_text ---

    #[test]
    fn tool_call_command_text_dict() {
        let raw = json!({"command": "grep pattern"});
        assert_eq!(tool_call_command_text(&raw), "grep pattern");
    }

    #[test]
    fn tool_call_command_text_json_string() {
        let raw = json!(r#"{"command": "ls -la"}"#);
        assert_eq!(tool_call_command_text(&raw), "ls -la");
    }

    #[test]
    fn tool_call_command_text_array_command() {
        let raw = json!({"command": ["grep", "-r", "pattern"]});
        assert_eq!(tool_call_command_text(&raw), "grep -r pattern");
    }

    #[test]
    fn tool_call_command_text_no_command() {
        let raw = json!({"name": "read_file"});
        assert_eq!(tool_call_command_text(&raw), "");
    }

    #[test]
    fn tool_call_command_text_invalid_json_string() {
        let raw = json!("not json at all");
        assert_eq!(tool_call_command_text(&raw), "");
    }

    // --- strip_detection_envelope ---

    #[test]
    fn strip_detection_envelope_output() {
        let content = "<output>\nline1\nline2\n</output>";
        let stripped = strip_detection_envelope(content);
        assert_eq!(stripped, "line1\nline2");
    }

    #[test]
    fn strip_detection_envelope_with_returncode() {
        let content = "<returncode>0</returncode>\n<output>result</output>";
        let stripped = strip_detection_envelope(content);
        assert_eq!(stripped, "result");
    }

    #[test]
    fn strip_detection_envelope_no_tag() {
        let content = "just plain text";
        assert_eq!(strip_detection_envelope(content), content);
    }

    #[test]
    fn strip_detection_envelope_partial_match() {
        let content = "before <output>middle</output> after";
        assert_eq!(strip_detection_envelope(content), content);
    }

    #[test]
    fn strip_detection_envelope_empty_body() {
        let content = "<output>\n</output>";
        assert_eq!(strip_detection_envelope(content), content);
    }

    // --- extract_json_block ---

    #[test]
    fn extract_json_block_array() {
        let lines = vec!["[1,", "  2,", "  3]"];
        let (content, end) = extract_json_block(&lines, 0);
        assert!(content.is_some());
        assert_eq!(end, 2);
    }

    #[test]
    fn extract_json_block_object() {
        let lines = vec![r#"{"key": "value","#, r#"  "num": 42}"#];
        let (content, end) = extract_json_block(&lines, 0);
        assert!(content.is_some());
        assert_eq!(end, 1);
    }

    #[test]
    fn extract_json_block_nested() {
        let lines = vec![r#"{"a": [1, 2],"#, r#"  "b": {"c": 3}}"#];
        let (content, end) = extract_json_block(&lines, 0);
        assert!(content.is_some());
        assert_eq!(end, 1);
    }

    #[test]
    fn extract_json_block_string_with_brackets() {
        let lines = vec![r#"{"path": "a]b"}"#];
        let (content, end) = extract_json_block(&lines, 0);
        assert!(content.is_some());
        assert_eq!(end, 0);
    }

    #[test]
    fn extract_json_block_incomplete() {
        let lines = vec!["[1,", "  2,"];
        let (content, end) = extract_json_block(&lines, 0);
        assert!(content.is_none());
        assert_eq!(end, 0);
    }

    // --- split_into_sections ---

    #[test]
    fn split_into_sections_pure_code() {
        let content = "def foo():\n    pass\n\ndef bar():\n    pass";
        let sections = split_into_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].content_type, ContentType::PlainText);
    }

    #[test]
    fn split_into_sections_code_fence() {
        let content = "# Title\n\n```python\ndef foo(): pass\n```\n\nMore text";
        let sections = split_into_sections(content);
        assert!(sections.len() >= 2);
        let code = sections
            .iter()
            .find(|s| s.content_type == ContentType::SourceCode);
        assert!(code.is_some());
        assert_eq!(code.unwrap().language.as_deref(), Some("python"));
    }

    #[test]
    fn split_into_sections_search_results() {
        let content = "src/a.py:42: code\nsrc/b.py:10: other";
        let sections = split_into_sections(content);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].content_type, ContentType::SearchResults);
    }

    #[test]
    fn split_into_sections_mixed() {
        let content = "# Title\n\n```python\ndef foo(): pass\n```\n\nsrc/a.py:42: code";
        let sections = split_into_sections(content);
        assert!(sections.len() >= 2);
        let has_code = sections
            .iter()
            .any(|s| s.content_type == ContentType::SourceCode);
        let has_search = sections
            .iter()
            .any(|s| s.content_type == ContentType::SearchResults);
        assert!(has_code || has_search);
    }

    #[test]
    fn split_into_sections_json() {
        let content = "text\n\n[1, 2, 3]\n\nmore text";
        let sections = split_into_sections(content);
        assert!(sections.len() >= 2);
        let json = sections
            .iter()
            .find(|s| s.content_type == ContentType::JsonArray);
        assert!(json.is_some());
    }

    // --- ContentRouterConfig ---

    #[test]
    fn config_default_values() {
        let config = ContentRouterConfig::default();
        assert!(!config.enable_code_aware);
        assert!(config.enable_kompress);
        assert!(config.enable_smart_crusher);
        assert!(config.enable_search_compressor);
        assert!(config.enable_log_compressor);
        assert!(config.enable_tabular_compressor);
        assert!(config.enable_html_extractor);
        assert!(config.enable_image_optimizer);
        assert!(config.prefer_code_aware_for_code);
        assert!(!config.force_kompress_all);
        assert!(!config.lossless);
        assert_eq!(config.min_section_tokens, 20);
        assert_eq!(config.fallback_strategy, CompressionStrategy::Kompress);
        assert!(config.skip_user_messages);
        assert_eq!(config.protect_recent_code, 4);
        assert!(config.protect_analysis_context);
        assert!(config.protect_error_outputs);
        assert_eq!(config.error_protection_max_chars, 8000);
        assert!(!config.compress_assistant_text_blocks);
        assert_eq!(config.min_chars_for_block_compression, 500);
        assert_eq!(config.protect_recent_reads_fraction, 0.0);
        assert_eq!(config.min_ratio_relaxed, 1.0);
        assert_eq!(config.min_ratio_aggressive, 1.0);
        assert!(config.ccr_enabled);
        assert!(config.ccr_inject_marker);
        assert!(config.smart_crusher_max_items_after_crush.is_none());
        assert!(config.smart_crusher_with_compaction);
        assert!(config.smart_crusher_lossless_only.is_none());
        assert!(config.relevance_split);
        assert_eq!(config.relevance_max_records, 0);
        assert!(config.relevance_adaptive_threshold);
        assert!(!config.compress_tagged_content);
        assert!(config.exclude_tools.is_none());
        assert!(config.bash_tool_names.contains("bash"));
        assert!(config.bash_search_commands.contains("grep"));
        assert!(!config.search_group_by_file);
    }

    // --- create_content_signature ---

    #[test]
    fn create_content_signature_deterministic() {
        let sig1 = create_content_signature("search", "file content", None);
        let sig2 = create_content_signature("search", "file content", None);
        assert_eq!(sig1, sig2);
        assert_eq!(sig1.len(), 24);
    }

    #[test]
    fn create_content_signature_differs_by_type() {
        let sig1 = create_content_signature("search", "content", None);
        let sig2 = create_content_signature("log", "content", None);
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn create_content_signature_differs_by_language() {
        let sig1 = create_content_signature("code", "content", Some("python"));
        let sig2 = create_content_signature("code", "content", Some("rust"));
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn create_content_signature_empty_content() {
        let sig = create_content_signature("text", "", None);
        assert_eq!(sig.len(), 24);
    }

    // --- netcost_message_tokens ---

    #[test]
    fn netcost_message_tokens_string() {
        let content = json!("hello world foo bar");
        assert_eq!(netcost_message_tokens(&content), 4);
    }

    #[test]
    fn netcost_message_tokens_text_block() {
        let content = json!([{"type": "text", "text": "hello world"}]);
        assert_eq!(netcost_message_tokens(&content), 2);
    }

    #[test]
    fn netcost_message_tokens_tool_result_string() {
        let content = json!([{"type": "tool_result", "content": "hello world"}]);
        assert_eq!(netcost_message_tokens(&content), 2);
    }

    #[test]
    fn netcost_message_tokens_tool_result_array() {
        let content = json!([
            {"type": "tool_result", "content": [
                {"type": "text", "text": "hello world"},
                {"type": "text", "text": "foo bar"}
            ]}
        ]);
        assert_eq!(netcost_message_tokens(&content), 4);
    }

    #[test]
    fn netcost_message_tokens_mixed_blocks() {
        let content = json!([
            {"type": "text", "text": "hello"},
            {"type": "tool_result", "content": "world"}
        ]);
        assert_eq!(netcost_message_tokens(&content), 2);
    }

    #[test]
    fn netcost_message_tokens_empty() {
        let content = json!("");
        assert_eq!(netcost_message_tokens(&content), 0);
    }

    #[test]
    fn netcost_message_tokens_null() {
        let content = json!(null);
        assert_eq!(netcost_message_tokens(&content), 0);
    }

    // --- ToolSignature ---

    #[test]
    fn tool_signature_from_json_object() {
        let json = json!({"name": "Alice", "age": 30, "active": true});
        let sig = ToolSignature::from_items(&[json.clone()]);
        assert_eq!(sig.field_count, 3);
        assert!(!sig.has_nested_objects);
        assert!(!sig.has_arrays);
        assert_eq!(sig.max_depth, 1); // Flat object has depth 1
        assert_eq!(sig.structure_hash.len(), 24);
    }

    #[test]
    fn tool_signature_from_json_nested() {
        let json = json!({"user": {"name": "Alice", "address": {"city": "NYC"}}});
        let sig = ToolSignature::from_items(&[json.clone()]);
        assert!(sig.has_nested_objects);
        assert!(sig.max_depth >= 2);
    }

    #[test]
    fn tool_signature_from_json_array() {
        let json = json!({"items": [1, 2, 3]});
        let sig = ToolSignature::from_items(&[json.clone()]);
        assert!(sig.has_arrays);
    }

    #[test]
    fn tool_signature_deterministic() {
        let json = json!({"key": "value"});
        let sig1 = ToolSignature::from_items(&[json.clone()]);
        let sig2 = ToolSignature::from_items(&[json.clone()]);
        assert_eq!(sig1.structure_hash, sig2.structure_hash);
    }

    #[test]
    fn tool_signature_for_content_type() {
        let sig = ToolSignature::for_content_type("search", "content", None);
        assert_eq!(sig.field_count, 0);
        assert_eq!(sig.structure_hash.len(), 24);
    }

    // --- detect_content_native ---

    #[test]
    fn detect_content_native_json() {
        let content = r#"[{"id": 1}, {"id": 2}]"#;
        let ct = detect_content_native(content);
        assert_eq!(ct, ContentType::JsonArray);
    }

    #[test]
    fn detect_content_native_code() {
        let content = "def foo():\n    pass\n\ndef bar():\n    pass\n\ndef baz():\n    pass\n\ndef qux():\n    return 42";
        let ct = detect_content_native(content);
        assert_eq!(ct, ContentType::SourceCode);
    }

    #[test]
    fn detect_content_native_search() {
        let content = "src/main.py:42: def process():\nsrc/main.py:43:     return None";
        let ct = detect_content_native(content);
        assert_eq!(ct, ContentType::SearchResults);
    }

    #[test]
    fn detect_content_native_diff() {
        let content = "diff --git a/f.py b/f.py\nindex 123..456 100644\n--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-old\n+new";
        let ct = detect_content_native(content);
        assert_eq!(ct, ContentType::GitDiff);
    }

    #[test]
    fn detect_content_native_envelope() {
        let content = "<output>\n[1, 2, 3]\n</output>";
        let ct = detect_content_native(content);
        assert_eq!(ct, ContentType::JsonArray);
    }

    // --- strategy_from_detection ---

    #[test]
    fn strategy_from_detection_json() {
        assert_eq!(
            strategy_from_detection(ContentType::JsonArray, false),
            CompressionStrategy::SmartCrusher
        );
    }

    #[test]
    fn strategy_from_detection_code() {
        assert_eq!(
            strategy_from_detection(ContentType::SourceCode, true),
            CompressionStrategy::CodeAware
        );
    }

    #[test]
    fn strategy_from_detection_search() {
        assert_eq!(
            strategy_from_detection(ContentType::SearchResults, false),
            CompressionStrategy::Search
        );
    }

    #[test]
    fn strategy_from_detection_log() {
        assert_eq!(
            strategy_from_detection(ContentType::BuildOutput, false),
            CompressionStrategy::Log
        );
    }

    #[test]
    fn strategy_from_detection_diff() {
        assert_eq!(
            strategy_from_detection(ContentType::GitDiff, true),
            CompressionStrategy::Diff
        );
    }

    #[test]
    fn strategy_from_detection_html() {
        assert_eq!(
            strategy_from_detection(ContentType::Html, true),
            CompressionStrategy::Html
        );
    }

    #[test]
    fn strategy_from_detection_text() {
        assert_eq!(
            strategy_from_detection(ContentType::PlainText, true),
            CompressionStrategy::Kompress
        );
    }

    // --- CompressionStrategy ---

    #[test]
    fn compression_strategy_from_str() {
        assert_eq!(
            CompressionStrategy::from_str("smart_crusher"),
            Some(CompressionStrategy::SmartCrusher)
        );
        assert_eq!(
            CompressionStrategy::from_str("search"),
            Some(CompressionStrategy::Search)
        );
        assert_eq!(CompressionStrategy::from_str("unknown"), None);
    }

    // --- apply_strategy ---

    #[test]
    fn apply_strategy_smart_crusher() {
        let config = ContentRouterConfig::default();
        let content = r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}, {"id": 3, "name": "Charlie"}]"#;
        let (compressed, tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::SmartCrusher,
            &config,
            "",
            None,
            1.0,
        );
        assert!(!compressed.is_empty());
        assert!(tokens <= 15); // SmartCrusher should compress
        assert_eq!(chain, vec!["smart_crusher"]);
    }

    #[test]
    fn apply_strategy_log() {
        let config = ContentRouterConfig::default();
        let content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10";
        let (compressed, _tokens, chain) =
            apply_strategy(content, CompressionStrategy::Log, &config, "", None, 1.0);
        assert!(!compressed.is_empty());
        assert_eq!(chain, vec!["log"]);
    }

    #[test]
    fn apply_strategy_search() {
        // Two files, one match each, sharing a parent directory: the FILE fold
        // saves nothing here, but `search_dir_heading` factors out `src/`. The
        // STAGE 0 lossless fold therefore wins and the chain reports
        // `lossless_search` instead of reaching the lossy search compressor.
        // Byte-for-byte what Python's `compact_lossless(content, "search")`
        // returns for this input.
        let config = ContentRouterConfig::default();
        let content = "src/a.py:42: code\nsrc/b.py:10: other";
        let (compressed, _tokens, chain) =
            apply_strategy(content, CompressionStrategy::Search, &config, "", None, 1.0);
        assert_eq!(compressed, "src/\na.py:42: code\nb.py:10: other");
        assert_eq!(chain, vec!["lossless_search"]);
    }

    #[test]
    fn apply_strategy_diff() {
        // STAGE 0 lossless-first (60af15f9): the `index` bookkeeping line folds
        // away byte-exact, so a diff whose fold shrinks returns `lossless_diff`
        // rather than reaching the lossy DiffCompressor.
        let config = ContentRouterConfig::default();
        let content = "diff --git a/f.py b/f.py\nindex 123..456\n--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-old\n+new";
        let (compressed, _tokens, chain) =
            apply_strategy(content, CompressionStrategy::Diff, &config, "", None, 1.0);
        assert!(!compressed.is_empty());
        assert_eq!(chain, vec!["lossless_diff"]);
    }

    #[test]
    fn apply_strategy_passthrough() {
        let config = ContentRouterConfig::default();
        let content = "hello world";
        let (compressed, tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::Passthrough,
            &config,
            "",
            None,
            1.0,
        );
        assert_eq!(compressed, content);
        assert_eq!(tokens, 2);
        assert_eq!(chain, vec!["passthrough"]);
    }

    #[test]
    fn apply_strategy_disabled_compressor() {
        let mut config = ContentRouterConfig::default();
        config.enable_smart_crusher = false;
        let content = "[1, 2, 3]";
        let (compressed, tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::SmartCrusher,
            &config,
            "",
            None,
            1.0,
        );
        assert_eq!(compressed, content);
        assert_eq!(chain, vec!["passthrough"]);
    }

    #[test]
    fn apply_strategy_smart_crusher_fallback_to_log() {
        let mut config = ContentRouterConfig::default();
        config.enable_log_compressor = true;
        config.enable_kompress = false; // Skip Kompress to test Log fallback directly
                                        // Non-JSON content — SmartCrusher passes through unchanged, then
                                        // Kompress is skipped (disabled), then Log is attempted.
        let content = "unique one-off error that cannot be deduplicated or compressed";
        let original_tokens = content.split_whitespace().count();
        let (compressed, tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::SmartCrusher,
            &config,
            "",
            None,
            1.0,
        );
        assert!(!compressed.is_empty());
        // SmartCrusher can't compress non-JSON, so fallback fires
        assert!(tokens >= original_tokens);
        // Chain records all attempted strategies even when they don't help
        assert_eq!(chain, vec!["smart_crusher", "kompress", "log"]);
    }

    // --- segment ---

    #[test]
    fn segment_empty() {
        assert_eq!(segment("", 8, 1200), Vec::<String>::new());
    }

    #[test]
    fn segment_single_line() {
        assert_eq!(segment("hello", 8, 1200), vec!["hello"]);
    }

    #[test]
    fn segment_blank_line_delimited() {
        let content = "line1\nline2\n\nline3\nline4";
        let segs = segment(content, 8, 1200);
        assert_eq!(segs.len(), 2);
        // Segments include trailing newlines from the split
        assert!(segs[0].contains("line1"));
        assert!(segs[0].contains("line2"));
        assert!(segs[1].contains("line3"));
        assert!(segs[1].contains("line4"));
    }

    #[test]
    fn segment_dense_block() {
        let lines: Vec<String> = (0..20).map(|i| format!("line{}", i)).collect();
        let content = lines.join("\n");
        let segs = segment(&content, 8, 1200);
        assert!(segs.len() >= 3); // 20 lines / 8 per window = 3 windows
                                  // Verify lossless
        let rejoined = segs.join("\n");
        assert_eq!(rejoined, content);
    }

    #[test]
    fn segment_long_line() {
        let long_line = "x".repeat(2000);
        let segs = segment(&long_line, 8, 1200);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], long_line);
    }

    // --- otsu_threshold ---

    #[test]
    fn otsu_threshold_bimodal() {
        // Two clear groups: [0.1, 0.2, 0.3] and [0.8, 0.9, 1.0]
        let values = vec![0.1, 0.2, 0.3, 0.8, 0.9, 1.0];
        let t = otsu_threshold(&values);
        assert!(
            t > 0.3 && t < 0.8,
            "threshold should be between groups: {}",
            t
        );
    }

    #[test]
    fn otsu_threshold_uniform() {
        let values = vec![0.5, 0.5, 0.5];
        let t = otsu_threshold(&values);
        assert_eq!(t, 0.5);
    }

    #[test]
    fn otsu_threshold_empty() {
        assert_eq!(otsu_threshold(&[]), 0.0);
    }

    // --- adaptive_threshold ---

    #[test]
    fn adaptive_threshold_with_floor() {
        let values = vec![0.1, 0.2, 0.3, 0.8, 0.9, 1.0];
        let t = adaptive_threshold(&values, 0.5);
        assert!(t >= 0.5, "threshold should be at least floor: {}", t);
    }

    #[test]
    fn adaptive_threshold_uniform_returns_floor() {
        let values = vec![0.5, 0.5, 0.5];
        let t = adaptive_threshold(&values, 0.3);
        assert_eq!(t, 0.3);
    }

    // --- plan_relevance_split ---

    #[test]
    fn plan_relevance_split_empty_query() {
        let content = "line1\n\nline2\n\nline3";
        let runs = plan_relevance_split(content, "", &[0.5, 0.3, 0.8], 0.3, true, None);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].keep);
    }

    #[test]
    fn plan_relevance_split_high_scores() {
        // Need enough lines to trigger segmentation (window=8, max_chars=1200)
        let content = "line1\nline2\n\nline3\nline4\n\nline5\nline6\n\nline7\nline8";
        let segs = segment(content, 8, 1200);
        // Should have 4 segments (blank-line delimited)
        assert_eq!(segs.len(), 4);
        // All scores above threshold -> all kept -> merged into 1 run
        let scores: Vec<f64> = segs.iter().map(|_| 0.9).collect();
        let runs = plan_relevance_split(content, "query", &scores, 0.3, true, None);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].keep);
    }

    #[test]
    fn plan_relevance_split_mixed_scores() {
        let content = "line1\n\nline2\n\nline3\n\nline4";
        let runs = plan_relevance_split(content, "query", &[0.9, 0.1, 0.9, 0.1], 0.5, false, None);
        // Should have alternating keep/drop runs
        assert!(runs.len() >= 2);
        assert!(runs[0].keep);
        assert!(!runs[1].keep);
    }

    #[test]
    fn plan_relevance_split_single_segment() {
        let content = "just one line";
        let runs = plan_relevance_split(content, "query", &[0.5], 0.3, true, None);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].keep);
    }

    #[test]
    fn plan_relevance_split_max_records() {
        let lines: Vec<String> = (0..20).map(|i| format!("line{}\n", i)).collect();
        let content = lines.join("\n");
        let scores: Vec<f64> = (0..20)
            .map(|i| if i % 2 == 0 { 0.9 } else { 0.1 })
            .collect();
        let runs = plan_relevance_split(&content, "query", &scores, 0.5, true, Some(5));
        // Should be single keep run because segments > max_records
        assert_eq!(runs.len(), 1);
        assert!(runs[0].keep);
    }

    // ─── Savings profile tests ──────────────────────────────────────────

    #[test]
    fn savings_profile_agent90_settings() {
        let mut config = ContentRouterConfig::default();
        SavingsProfile::Agent90.apply_to(&mut config);
        assert_eq!(config.target_ratio, Some(0.10));
        assert_eq!(config.compress_user_messages, Some(true));
        assert_eq!(config.compress_system_messages, Some(true));
        assert_eq!(config.protect_recent_code, 2);
        assert!(config.force_kompress_all);
    }

    #[test]
    fn savings_profile_balanced_settings() {
        let mut config = ContentRouterConfig::default();
        SavingsProfile::Balanced.apply_to(&mut config);
        assert_eq!(config.target_ratio, Some(0.30));
        assert_eq!(config.compress_user_messages, Some(false));
        assert_eq!(config.compress_system_messages, Some(false));
        assert_eq!(config.protect_recent_code, 4);
        assert!(!config.force_kompress_all);
    }

    #[test]
    fn savings_profile_coding_settings() {
        let mut config = ContentRouterConfig::default();
        SavingsProfile::Coding.apply_to(&mut config);
        assert_eq!(config.target_ratio, None);
        assert_eq!(config.compress_user_messages, Some(false));
        assert_eq!(config.protect_recent_code, 2);
    }

    #[test]
    fn savings_profile_general_settings() {
        let mut config = ContentRouterConfig::default();
        SavingsProfile::General.apply_to(&mut config);
        assert_eq!(config.target_ratio, None);
        assert_eq!(config.protect_recent_code, 0);
    }

    #[test]
    fn savings_profile_from_str_roundtrip() {
        assert_eq!(
            SavingsProfile::from_str("agent-90"),
            Some(SavingsProfile::Agent90)
        );
        assert_eq!(
            SavingsProfile::from_str("balanced"),
            Some(SavingsProfile::Balanced)
        );
        assert_eq!(
            SavingsProfile::from_str("coding"),
            Some(SavingsProfile::Coding)
        );
        assert_eq!(
            SavingsProfile::from_str("general"),
            Some(SavingsProfile::General)
        );
        assert_eq!(SavingsProfile::from_str("unknown"), None);
    }

    #[test]
    fn savings_profile_serde_roundtrip() {
        let json = serde_json::to_string(&SavingsProfile::Balanced).unwrap();
        assert_eq!(json, "\"balanced\"");
        let back: SavingsProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SavingsProfile::Balanced);
    }

    #[test]
    fn target_ratio_in_config() {
        let mut config = ContentRouterConfig::default();
        assert_eq!(config.target_ratio, None);
        config.target_ratio = Some(0.25);
        assert_eq!(config.target_ratio, Some(0.25));
    }

    #[test]
    fn per_provider_kompress_disable() {
        let mut config = ContentRouterConfig::default();
        config
            .disable_kompress_per_provider
            .insert("anthropic".to_string(), true);
        config
            .disable_kompress_per_provider
            .insert("openai".to_string(), false);
        assert!(config.disable_kompress_per_provider["anthropic"]);
        assert!(!config.disable_kompress_per_provider["openai"]);
    }

    #[test]
    fn compress_user_system_messages_in_config() {
        let mut config = ContentRouterConfig::default();
        assert_eq!(config.compress_user_messages, None);
        assert_eq!(config.compress_system_messages, None);
        config.compress_user_messages = Some(true);
        config.compress_system_messages = Some(false);
        assert_eq!(config.compress_user_messages, Some(true));
        assert_eq!(config.compress_system_messages, Some(false));
    }

    // --- apply_strategy: untested paths ---

    #[test]
    fn apply_strategy_code_aware_compresses_code() {
        let mut config = ContentRouterConfig::default();
        config.enable_code_aware = true;
        config.enable_kompress = false; // Don't fall back to Kompress
        let content = "function hello() {\n  console.log('world');\n  return 42;\n}";
        let (compressed, _tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::CodeAware,
            &config,
            "",
            Some("javascript"),
            1.0,
        );
        assert!(!compressed.is_empty());
        assert_eq!(chain, vec!["code_aware"]);
    }

    #[test]
    fn apply_strategy_code_aware_disabled_falls_through() {
        let mut config = ContentRouterConfig::default();
        config.enable_code_aware = false;
        let content = "function hello() { return 42; }";
        let (compressed, tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::CodeAware,
            &config,
            "",
            Some("javascript"),
            1.0,
        );
        // Disabled → falls through to _ arm → passthrough
        assert_eq!(compressed, content);
        assert_eq!(chain, vec!["passthrough"]);
    }

    #[test]
    fn apply_strategy_html_extracts_content() {
        let config = ContentRouterConfig::default();
        let content = "<!DOCTYPE html>\n<html>\n<head>\n<title>Test</title>\n<script>var analytics = {track: true}; function init() { console.log('boot'); }</script>\n<style>nav { background: #333; } footer { color: #666; }</style>\n</head>\n<body>\n<nav><a href=\"/\">Home</a> <a href=\"/about\">About</a></nav>\n<article>\n<h1>Main Heading</h1>\n<p>This is the first paragraph of actual article content with details.</p>\n<p>This is the second paragraph carrying even more meaningful details.</p>\n</article>\n<footer>Copyright 2024 | Privacy Policy | Terms of Service</footer>\n</body>\n</html>";
        let (compressed, tokens, chain) =
            apply_strategy(content, CompressionStrategy::Html, &config, "", None, 1.0);
        // Extraction wins: main content survives, boilerplate is stripped
        assert_eq!(chain, vec!["html_extractor"]);
        assert!(compressed.contains("first paragraph"));
        assert!(!compressed.contains("analytics"));
        assert!(tokens < content.split_whitespace().count());
    }

    #[test]
    fn apply_strategy_html_disabled_falls_through() {
        let mut config = ContentRouterConfig::default();
        config.enable_html_extractor = false;
        let content = "<html><body><h1>Hello</h1></body></html>";
        let (compressed, _tokens, chain) =
            apply_strategy(content, CompressionStrategy::Html, &config, "", None, 1.0);
        // Disabled → falls through to _ arm → passthrough
        assert_eq!(compressed, content);
        assert_eq!(chain, vec!["passthrough"]);
    }

    #[test]
    fn apply_strategy_html_inflation_guard() {
        let config = ContentRouterConfig::default();
        // Tiny fragment: extraction yields nothing (or nothing smaller) →
        // original content is kept, chain records the attempt
        let content = "<html><body></body></html>";
        let (compressed, _tokens, chain) =
            apply_strategy(content, CompressionStrategy::Html, &config, "", None, 1.0);
        assert_eq!(compressed, content);
        assert_eq!(chain, vec!["html_extractor", "passthrough"]);
    }

    #[test]
    fn apply_strategy_tabular_disabled_falls_through() {
        let mut config = ContentRouterConfig::default();
        config.enable_kompress = false;
        let content = "col1,col2,col3\n1,2,3\n4,5,6";
        let (compressed, tokens, chain) = apply_strategy(
            content,
            CompressionStrategy::Kompress,
            &config,
            "",
            None,
            1.0,
        );
        // Kompress disabled → falls through to _ (passthrough)
        assert_eq!(compressed, content);
        assert_eq!(chain, vec!["passthrough"]);
    }

    #[test]
    fn apply_strategy_text_disabled_falls_through() {
        let mut config = ContentRouterConfig::default();
        config.enable_kompress = false;
        let content = "just some plain text content here";
        let (compressed, tokens, chain) =
            apply_strategy(content, CompressionStrategy::Text, &config, "", None, 1.0);
        // Text maps to Kompress which is disabled → passthrough
        assert_eq!(compressed, content);
        assert_eq!(chain, vec!["passthrough"]);
    }

    // --- STAGE 0 lossless-first dispatch (parity: 60af15f9) ---

    fn grep_block() -> String {
        // Long, repeated path prefixes → search --heading fold collapses bytes
        // while word count stays flat/rises (heading re-emits path words).
        let paths = [
            "src/services/wallet/overdraft/automated_overdraft_initiation.py",
            "src/services/wallet/overdraft/capacity_limits.py",
        ];
        let mut lines = Vec::new();
        for p in paths {
            for ln in 1..40 {
                lines.push(format!(
                    "{p}:{ln}:    result = compute_overdraft_capacity(business_id, amount)"
                ));
            }
        }
        format!("{}\n", lines.join("\n"))
    }

    #[test]
    fn stage0_search_folds_lossless_byte_exact() {
        let block = grep_block();
        let config = ContentRouterConfig::default();
        let (out, _tokens, chain) =
            apply_strategy(&block, CompressionStrategy::Search, &config, "", None, 1.0);
        assert_eq!(chain, vec!["lossless_search"]);
        assert!(out.len() < block.len());
        // Word count is flat/higher — the byte-anchored fold still wins.
        assert!(out.split_whitespace().count() >= block.split_whitespace().count());
    }

    #[test]
    fn lossless_only_mode_leaves_non_foldable_verbatim() {
        // Source code has no byte-lossless fold; in lossless-only mode it must be
        // left verbatim (passthrough), not lossy-dropped.
        let code = "fn main() {\n    println!(\"hi\");\n}\n";
        let config = ContentRouterConfig {
            lossless: true,
            ..Default::default()
        };
        let (out, _t, chain) =
            apply_strategy(code, CompressionStrategy::CodeAware, &config, "", None, 1.0);
        assert_eq!(out, code);
        assert_eq!(chain, vec!["passthrough"]);
    }

    #[test]
    fn lossless_only_mode_folds_search() {
        let block = grep_block();
        let config = ContentRouterConfig {
            lossless: true,
            ..Default::default()
        };
        let (out, _t, chain) =
            apply_strategy(&block, CompressionStrategy::Search, &config, "", None, 1.0);
        assert_eq!(chain, vec!["lossless_search"]);
        assert!(out.len() < block.len());
    }

    #[test]
    fn looks_like_diff_detects_unified_and_git() {
        assert!(looks_like_diff("diff --git a/x b/x\n"));
        assert!(looks_like_diff("--- a/x\n+++ b/x\n"));
        assert!(looks_like_diff("@@ -1,2 +1,2 @@\n"));
        assert!(looks_like_diff("foo\n@@ -1 +1 @@\n"));
        assert!(!looks_like_diff("just some text\nwith @@ inline"));
    }

    // ─── Frozen block verdicts (upstream addition) ───────────────────────

    /// The env flag is process-global, so these tests share one lock rather
    /// than racing each other under the default multi-threaded test runner.
    fn freeze_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct FreezeFlag(Option<String>);
    impl FreezeFlag {
        fn on() -> Self {
            let prev = std::env::var("HEADROOM_FREEZE_BLOCK_DECISION").ok();
            std::env::set_var("HEADROOM_FREEZE_BLOCK_DECISION", "1");
            Self(prev)
        }
    }
    impl Drop for FreezeFlag {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("HEADROOM_FREEZE_BLOCK_DECISION", v),
                None => std::env::remove_var("HEADROOM_FREEZE_BLOCK_DECISION"),
            }
        }
    }

    #[test]
    fn frozen_verdict_store_round_trips_and_clears() {
        let cache = CompressionCache::new(1800);
        assert_eq!(cache.frozen_verdict(7), None);
        cache.record_frozen_verdict(7, true);
        cache.record_frozen_verdict(8, false);
        assert_eq!(cache.frozen_verdict(7), Some(true));
        assert_eq!(cache.frozen_verdict(8), Some(false));
        // Clearing the cache must drop verdicts: they describe entries that no
        // longer exist.
        cache.clear();
        assert_eq!(cache.frozen_verdict(7), None);
    }

    #[test]
    fn frozen_verdicts_evict_oldest_first_and_stay_bounded() {
        let cache = CompressionCache::new(1800);
        for k in 0..(FROZEN_VERDICTS_MAX as i64 + 10) {
            cache.record_frozen_verdict(k, true);
        }
        // Oldest evicted, newest retained, size capped.
        assert_eq!(cache.frozen_verdict(0), None);
        assert_eq!(
            cache.frozen_verdict(FROZEN_VERDICTS_MAX as i64 + 9),
            Some(true)
        );
        assert_eq!(
            cache.frozen.lock().unwrap().verdicts.len(),
            FROZEN_VERDICTS_MAX
        );
    }

    #[test]
    fn re_recording_a_key_does_not_duplicate_its_eviction_slot() {
        let cache = CompressionCache::new(1800);
        cache.record_frozen_verdict(1, true);
        cache.record_frozen_verdict(1, false);
        let frozen = cache.frozen.lock().unwrap();
        assert_eq!(frozen.verdicts.len(), 1);
        assert_eq!(frozen.order.len(), 1, "order must not gain a second entry");
        assert_eq!(frozen.verdicts[&1], false, "later verdict wins");
    }

    #[test]
    fn frozen_compress_verdict_pins_the_accept_threshold() {
        let _guard = freeze_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _flag = FreezeFlag::on();
        let cache = CompressionCache::new(1800);
        // Unfrozen: the live per-turn gate applies.
        assert_eq!(cache.accept_threshold(1, 0.6), 0.6);
        // Frozen "compress": pinned to 1.0 so a tightened min_ratio can never
        // downgrade the block and bust the provider prefix cache.
        cache.record_frozen_verdict(1, true);
        assert_eq!(cache.accept_threshold(1, 0.6), 1.0);
        // A frozen "skip" is not a pin — it never warms the result cache.
        cache.record_frozen_verdict(2, false);
        assert_eq!(cache.accept_threshold(2, 0.6), 0.6);
    }

    #[test]
    fn freeze_is_inert_while_the_flag_is_off() {
        let _guard = freeze_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HEADROOM_FREEZE_BLOCK_DECISION");
        let cache = CompressionCache::new(1800);
        cache.record_frozen_verdict(1, true);
        assert_eq!(
            cache.accept_threshold(1, 0.6),
            0.6,
            "flag off must be byte-identical to the unfrozen path"
        );
    }

    #[test]
    fn unrecoverable_lossy_compressions_are_never_frozen() {
        // #1307: pinning a lossy-unmarked strategy with no CCR marker would
        // keep serving a summary the agent cannot restore.
        for s in ["kompress", "text", "code_aware"] {
            assert!(
                !frozen_verdict_recoverable(s, Some("a fabricated summary")),
                "{s} without a marker must not be frozen"
            );
            assert!(
                frozen_verdict_recoverable(s, Some("summary <<ccr:abc123def456>>")),
                "{s} WITH a marker is recoverable"
            );
        }
        // Non-lossy strategies are always safe to pin.
        assert!(frozen_verdict_recoverable("lossless_search", Some("x")));
        assert!(frozen_verdict_recoverable("search", None));
    }

    #[test]
    fn freeze_pin_accumulates_preserved_chars() {
        let cache = CompressionCache::new(1800);
        assert_eq!(cache.freeze_pin_stats(), (0, 0));
        cache.record_freeze_pin(&"x".repeat(100), 0.4);
        let (pins, chars) = cache.freeze_pin_stats();
        assert_eq!(pins, 1);
        assert_eq!(chars, 60, "100 chars at ratio 0.4 preserves ~60");
        // A ratio above 1.0 must not underflow the unsigned counter.
        cache.record_freeze_pin("short", 1.5);
        assert_eq!(cache.freeze_pin_stats().1, 60);
    }

    #[test]
    fn lossless_first_no_fold_returns_none() {
        let (out, label) = lossless_first("short text", CompressionStrategy::Kompress);
        assert_eq!(out, "short text");
        assert!(label.is_none());
    }

    #[test]
    fn lossless_first_never_diff_folds_non_diff_content() {
        // `diff_strip_index` is purely subtractive with no inverse check: it
        // deletes any `index <hex>..<hex>` line. On non-diff content that is
        // unrecoverable data loss dressed up as a lossless fold. The fold order
        // must therefore exclude "diff" unless the content really is a diff.
        let text = "Build log follows\nindex 1a2b3c4..5d6e7f8 100644\nrestore from index 1a2b3c4..5d6e7f8 100644\nall done\n";
        for strategy in [
            CompressionStrategy::Log,
            CompressionStrategy::Text,
            CompressionStrategy::Kompress,
            CompressionStrategy::Search,
        ] {
            let (out, label) = lossless_first(text, strategy);
            assert!(
                out.contains("index 1a2b3c4..5d6e7f8"),
                "the index line was silently dropped under {strategy:?} (label {label:?}): {out}"
            );
        }
    }

    #[test]
    fn lossless_first_still_diff_folds_real_diffs() {
        // The guard must not cost us the fold on genuine diff content.
        let diff = "diff --git a/x b/x\nindex 1a2b3c4..5d6e7f8 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let (out, label) = lossless_first(diff, CompressionStrategy::Diff);
        assert!(
            !out.contains("index 1a2b3c4"),
            "diff index line should fold"
        );
        assert_eq!(label.as_deref(), Some("lossless_diff"));
        // And also when the strategy is wrong but the content sniffs as a diff.
        let (out2, _) = lossless_first(diff, CompressionStrategy::Text);
        assert!(!out2.contains("index 1a2b3c4"));
    }

    #[test]
    fn lossless_first_can_choose_the_new_config_and_paths_folds() {
        let conf = "key: 1\nkey: 1\nkey: 1\n  - name: a\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n  - name: b\n    image: repo/svc:1\n    port: 8080\n    mem: 512Mi\n";
        let (out, label) = lossless_first(conf, CompressionStrategy::Config);
        assert!(out.len() < conf.len(), "config fold should shrink");
        assert_eq!(label.as_deref(), Some("lossless_config"));

        let paths = "src/handlers/alpha.rs\nsrc/handlers/beta.rs\nsrc/handlers/gamma.rs\n";
        let (out2, label2) = lossless_first(paths, CompressionStrategy::Text);
        assert!(out2.len() < paths.len(), "paths fold should shrink");
        assert_eq!(label2.as_deref(), Some("lossless_paths"));
    }
}

#[cfg(test)]
mod external_compressor_tests {
    use super::*;
    use crate::transforms::compressor_registry::CompressOutput;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    /// What the stub should return, so each test can drive a single branch.
    enum Behavior {
        /// Shrink the content to a fixed short string.
        Shrink,
        /// Return more bytes than it was given.
        Expand,
        /// Blank the block out entirely.
        Empty,
        /// Shrink, and also emit a recovery map entry.
        ShrinkWithRecoverable,
    }

    struct StubExternal {
        descriptor: CompressorDescriptor,
        behavior: Behavior,
    }

    impl StubExternal {
        fn new(name: &str, content_types: &[&str], behavior: Behavior) -> Arc<dyn Compressor> {
            Arc::new(Self {
                descriptor: CompressorDescriptor {
                    name: name.to_string(),
                    content_types: content_types.iter().map(|s| s.to_string()).collect(),
                    lossless: false,
                    cost_tier: "fast".to_string(),
                    recoverable: matches!(behavior, Behavior::ShrinkWithRecoverable),
                },
                behavior,
            })
        }
    }

    impl Compressor for StubExternal {
        fn descriptor(&self) -> &CompressorDescriptor {
            &self.descriptor
        }

        fn compress(&self, input: &CompressInput) -> CompressOutput {
            match self.behavior {
                Behavior::Shrink => CompressOutput {
                    content: "SHRUNK".to_string(),
                    ..Default::default()
                },
                Behavior::Expand => CompressOutput {
                    content: format!("{}{}", input.content, "x".repeat(64)),
                    ..Default::default()
                },
                Behavior::Empty => CompressOutput {
                    content: "   ".to_string(),
                    ..Default::default()
                },
                Behavior::ShrinkWithRecoverable => {
                    let mut recoverable = BTreeMap::new();
                    recoverable.insert("deadbeef".to_string(), input.content.clone());
                    CompressOutput {
                        content: "SHRUNK".to_string(),
                        recoverable,
                        ..Default::default()
                    }
                }
            }
        }
    }

    fn registry_with(compressor: Arc<dyn Compressor>) -> CompressorRegistry {
        let mut registry = CompressorRegistry::new();
        registry.register(compressor, false).unwrap();
        registry
    }

    fn selecting(names: &[&str]) -> ContentRouterConfig {
        ContentRouterConfig {
            active_external_compressors: names.iter().map(|s| s.to_string()).collect(),
            // Keep the built-in path cheap and deterministic for these tests.
            enable_kompress: false,
            ..Default::default()
        }
    }

    /// Long enough that a shrink to "SHRUNK" is unambiguous.
    const SAMPLE: &str = "the quick brown fox jumps over the lazy dog again and again and again";

    fn run(
        content: &str,
        config: &ContentRouterConfig,
        registry: &CompressorRegistry,
    ) -> (String, usize, Vec<String>) {
        apply_strategy_with_registry(
            content,
            CompressionStrategy::Text,
            config,
            "",
            None,
            0.5,
            None,
            registry,
            &|_, _, _| true,
        )
    }

    /// The safety property that makes this change inert by default: with no
    /// selection, the result is byte-identical to the built-in-only path.
    #[test]
    fn no_selection_means_the_external_path_is_never_reached() {
        let registry = registry_with(StubExternal::new("ext", &["text/plain"], Behavior::Shrink));
        let config = ContentRouterConfig {
            enable_kompress: false,
            ..Default::default()
        };
        assert!(config.active_external_compressors.is_empty());

        let (with_registry, _, chain) = run(SAMPLE, &config, &registry);
        let (builtin, _, builtin_chain) =
            apply_strategy(SAMPLE, CompressionStrategy::Text, &config, "", None, 0.5);

        assert_eq!(with_registry, builtin);
        assert_eq!(chain, builtin_chain);
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }

    #[test]
    fn a_selected_compressor_handles_a_matching_content_type() {
        let registry = registry_with(StubExternal::new("ext", &["text/plain"], Behavior::Shrink));
        let (out, tokens, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

        assert_eq!(out, "SHRUNK");
        assert_eq!(chain, vec!["external:ext".to_string()]);
        // Counted with the router's own estimator, not the compressor's
        // self-reported (and here deliberately zero) tokens_after.
        assert_eq!(tokens, 1);
    }

    #[test]
    fn a_wildcard_content_type_matches_anything() {
        for declared in [vec!["*"], vec!["*/*"], vec!["text/*"]] {
            let registry = registry_with(StubExternal::new("ext", &declared, Behavior::Shrink));
            let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);
            assert_eq!(
                out, "SHRUNK",
                "declared {declared:?} should match text/plain"
            );
            assert_eq!(chain, vec!["external:ext".to_string()]);
        }
    }

    #[test]
    fn a_non_matching_content_type_falls_through_to_the_builtin() {
        let registry = registry_with(StubExternal::new(
            "ext",
            &["application/json"],
            Behavior::Shrink,
        ));
        let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

        assert_ne!(out, "SHRUNK");
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }

    /// `image/*` must not match `text/plain` — the type wildcard is scoped to
    /// its own top-level type, otherwise it would be a full wildcard.
    #[test]
    fn a_type_wildcard_does_not_cross_content_types() {
        let registry = registry_with(StubExternal::new("ext", &["image/*"], Behavior::Shrink));
        let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

        assert_ne!(out, "SHRUNK");
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }

    #[test]
    fn an_expanding_compressor_is_rejected() {
        let registry = registry_with(StubExternal::new("ext", &["text/plain"], Behavior::Expand));
        let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

        assert!(out.len() <= SAMPLE.len());
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }

    /// Blanking a non-empty block makes providers reject the whole request, so
    /// an empty result must fall back rather than be returned.
    #[test]
    fn a_compressor_that_blanks_the_block_is_rejected() {
        let registry = registry_with(StubExternal::new("ext", &["text/plain"], Behavior::Empty));
        let (out, _, chain) = run(SAMPLE, &selecting(&["ext"]), &registry);

        assert!(!out.trim().is_empty());
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }

    #[test]
    fn selecting_an_unregistered_name_is_not_fatal() {
        let registry = registry_with(StubExternal::new("ext", &["text/plain"], Behavior::Shrink));
        let (out, _, chain) = run(SAMPLE, &selecting(&["ghost"]), &registry);

        assert_ne!(out, "SHRUNK");
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }

    #[test]
    fn the_recovery_map_is_handed_to_the_store() {
        let registry = registry_with(StubExternal::new(
            "ext",
            &["text/plain"],
            Behavior::ShrinkWithRecoverable,
        ));
        let stored: StdMutex<Vec<(String, String, String)>> = StdMutex::new(Vec::new());

        let (out, _, chain) = apply_strategy_with_registry(
            SAMPLE,
            CompressionStrategy::Text,
            &selecting(&["ext"]),
            "",
            None,
            0.5,
            None,
            &registry,
            &|hash, original, strategy| {
                stored.lock().unwrap().push((
                    hash.to_string(),
                    original.to_string(),
                    strategy.to_string(),
                ));
                true
            },
        );

        assert_eq!(out, "SHRUNK");
        assert_eq!(chain, vec!["external:ext".to_string()]);
        let entries = stored.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "deadbeef");
        assert_eq!(entries[0].1, SAMPLE);
        assert_eq!(entries[0].2, "external:ext");
    }

    /// A store failure must not break the request — the compressed block is
    /// still returned, only that entry is unretrievable.
    #[test]
    fn a_store_failure_still_returns_the_compressed_block() {
        let registry = registry_with(StubExternal::new(
            "ext",
            &["text/plain"],
            Behavior::ShrinkWithRecoverable,
        ));

        let (out, _, chain) = apply_strategy_with_registry(
            SAMPLE,
            CompressionStrategy::Text,
            &selecting(&["ext"]),
            "",
            None,
            0.5,
            None,
            &registry,
            &|_, _, _| false,
        );

        assert_eq!(out, "SHRUNK");
        assert_eq!(chain, vec!["external:ext".to_string()]);
    }

    #[test]
    fn strategy_to_mime_matches_python() {
        let cases = [
            (CompressionStrategy::CodeAware, "text/x-code"),
            (CompressionStrategy::SmartCrusher, "application/json"),
            (CompressionStrategy::Search, "text/x-search-results"),
            (CompressionStrategy::Log, "text/x-log"),
            (CompressionStrategy::Diff, "text/x-diff"),
            (CompressionStrategy::Html, "text/html"),
            (CompressionStrategy::Tabular, "text/csv"),
            (CompressionStrategy::Config, "text/x-config"),
            (CompressionStrategy::Text, "text/plain"),
            (CompressionStrategy::Kompress, "text/plain"),
            (CompressionStrategy::Passthrough, "text/plain"),
            // Unmapped in Python's dict → PLAIN_TEXT via `.get` default.
            (CompressionStrategy::Mixed, "text/plain"),
        ];
        for (strategy, expected) in cases {
            assert_eq!(
                content_type_mime(content_type_from_strategy(strategy)),
                expected,
                "{strategy:?}"
            );
        }
    }

    /// Lossless-only mode returns at STAGE 0, so an external compressor can
    /// never inject unrecoverable loss into a lossless-only session.
    #[test]
    fn lossless_only_mode_never_reaches_the_external_path() {
        let registry = registry_with(StubExternal::new("ext", &["*"], Behavior::Shrink));
        let config = ContentRouterConfig {
            lossless: true,
            ..selecting(&["ext"])
        };

        let (out, _, chain) = run(SAMPLE, &config, &registry);
        assert_ne!(out, "SHRUNK");
        assert!(!chain.iter().any(|c| c.starts_with("external:")));
    }
}

#[cfg(test)]
mod kompress_size_gate_tests {
    use super::*;

    /// `HEADROOM_KOMPRESS_MAX_TOKENS` is process-global, so these tests cannot
    /// run concurrently with each other.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Set the ceiling for the duration of a test, restoring it afterwards.
    struct CeilingGuard(Option<String>);

    impl CeilingGuard {
        fn set(value: Option<&str>) -> Self {
            let prior = std::env::var("HEADROOM_KOMPRESS_MAX_TOKENS").ok();
            match value {
                Some(v) => std::env::set_var("HEADROOM_KOMPRESS_MAX_TOKENS", v),
                None => std::env::remove_var("HEADROOM_KOMPRESS_MAX_TOKENS"),
            }
            Self(prior)
        }
    }

    impl Drop for CeilingGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("HEADROOM_KOMPRESS_MAX_TOKENS", v),
                None => std::env::remove_var("HEADROOM_KOMPRESS_MAX_TOKENS"),
            }
        }
    }

    #[test]
    fn the_default_ceiling_matches_python() {
        let _lock = env_lock();
        let _guard = CeilingGuard::set(None);
        assert_eq!(kompress_max_tokens(), 50_000);
        assert_eq!(DEFAULT_KOMPRESS_MAX_TOKENS, 50_000);
    }

    /// An unparseable value falls back to the default, matching Python's
    /// bare-except around `int(...)`.
    #[test]
    fn an_unparseable_ceiling_falls_back_to_the_default() {
        let _lock = env_lock();
        let _guard = CeilingGuard::set(Some("not-a-number"));
        assert_eq!(kompress_max_tokens(), DEFAULT_KOMPRESS_MAX_TOKENS);
    }

    /// The threshold is chars > tokens * 4, so it fires just past the boundary
    /// and not at it.
    #[test]
    fn the_gate_fires_only_above_the_ceiling() {
        let _lock = env_lock();
        let _guard = CeilingGuard::set(Some("10"));

        assert!(
            !kompress_size_gate_exceeded(&"x".repeat(40)),
            "at the boundary"
        );
        assert!(kompress_size_gate_exceeded(&"x".repeat(41)), "one past it");
    }

    /// A zero ceiling disables the gate entirely, however large the block.
    #[test]
    fn a_zero_ceiling_disables_the_gate() {
        let _lock = env_lock();
        let _guard = CeilingGuard::set(Some("0"));
        assert!(!kompress_size_gate_exceeded(&"x".repeat(1_000_000)));
    }

    /// The point of the gate: an oversized block must come back through the
    /// cheap path with the gate recorded in the chain, never reaching ML.
    #[test]
    fn an_oversized_block_is_routed_off_ml() {
        let _lock = env_lock();
        let _guard = CeilingGuard::set(Some("10"));

        let content = "the quick brown fox jumps over the lazy dog. ".repeat(40);
        let config = ContentRouterConfig::default();
        let (_out, _tokens, chain) = try_kompress(&content, &config, "", &["kompress".to_string()]);

        assert!(
            chain.contains(&"kompress_size_gate".to_string()),
            "chain should record the gate, got {chain:?}"
        );
    }

    /// Under the ceiling the gate must not appear in the chain at all.
    #[test]
    fn a_small_block_is_not_gated() {
        let _lock = env_lock();
        let _guard = CeilingGuard::set(Some("50000"));

        let config = ContentRouterConfig::default();
        let (_out, _tokens, chain) =
            try_kompress("short text", &config, "", &["kompress".to_string()]);

        assert!(!chain.contains(&"kompress_size_gate".to_string()));
    }

    /// An image block used to be stringified, embedding its whole base64
    /// payload in the count. S is the cache-bust cost, so that inflated S for
    /// every message before it and the break-even gate refused to compress
    /// any of them.
    #[test]
    fn image_blocks_are_not_counted_as_their_base64_payload() {
        let big_payload = "A".repeat(80_000);
        let with_image = serde_json::json!([
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "data": big_payload}},
        ]);
        let counted = netcost_message_tokens(&with_image);
        // text words + the flat image cost, nowhere near the payload size.
        assert!(
            counted < 2_000,
            "image priced at {counted} tokens; base64 payload leaked into the count"
        );
        assert!(counted >= crate::tokenizer::IMAGE_TOKENS);
    }

    #[test]
    fn text_and_tool_result_counting_is_unchanged() {
        let blocks = serde_json::json!([
            {"type": "text", "text": "one two three"},
            {"type": "tool_result", "content": "four five"},
        ]);
        assert_eq!(netcost_message_tokens(&blocks), 5);
        assert_eq!(netcost_message_tokens(&serde_json::json!("a b c")), 3);
    }
}
