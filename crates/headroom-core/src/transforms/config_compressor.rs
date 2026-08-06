//! Structured-config (YAML/TOML/INI) compression.
//!
//! Rust port of `headroom/transforms/config_compressor.py`. Three tiers, tried
//! together and resolved by size:
//!
//! * **Tier 1** — reversible run/stanza folding via
//!   [`compact_lossless`](super::lossless_compaction::compact_lossless) with the
//!   `config` kind. Self-verifying: it round-trips or returns the input.
//! * **Tier 2** — whole-line comment/blank elision behind a CCR marker. Lossy,
//!   so it only runs when the original can be stored for retrieval.
//! * **Tier 3** — TOML array-of-tables bridged to SmartCrusher's csv-schema.
//!   Wins on lockfiles and override-lists where repeated keys dominate.
//!
//! Tiers 2 and 3 both depend on the CCR store: nothing lossy is emitted unless
//! the original is recoverable.

use std::sync::OnceLock;

use regex::Regex;

use super::content_detector::{detect_content_type, ContentType};
use super::lossless_compaction::compact_lossless;

// ─── Flavor-specific patterns ────────────────────────────────────────────

/// Whole-line comment prefixes per flavor.
///
/// INI values can span indented continuation lines, so INI only elides
/// column-0 comment lines and keeps blanks — `configparser` keeps blank lines
/// inside multi-line values, and dropping them would corrupt the value.
fn comment_re(flavor: &str) -> &'static Regex {
    static YAML_TOML: OnceLock<Regex> = OnceLock::new();
    static INI: OnceLock<Regex> = OnceLock::new();
    match flavor {
        "ini" => INI.get_or_init(|| Regex::new(r"^[#;]").expect("valid")),
        // yaml, toml, and anything unrecognized use the YAML rule, matching
        // Python's `_COMMENT_RES.get(flavor, _COMMENT_RES["yaml"])`.
        _ => YAML_TOML.get_or_init(|| Regex::new(r"^\s*#").expect("valid")),
    }
}

/// A `#` line inside a YAML block scalar is DATA, not a comment.
fn yaml_block_scalar_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m):\s*[|>][+-]?\d*\s*$").expect("valid"))
}

/// Likewise for a `#` inside a TOML multi-line string.
fn toml_multiline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""""|'''"#).expect("valid"))
}

// ─── Types ───────────────────────────────────────────────────────────────

/// Configuration for structured-config compression.
#[derive(Debug, Clone)]
pub struct ConfigCompressorConfig {
    /// Emit the CCR-marked comment/blank elision tier. The router wires this to
    /// its `ccr_inject_marker` setting; lossless mode turns it off.
    pub enable_ccr: bool,
    /// Bridge TOML array-of-tables to SmartCrusher csv-schema (Tier 3). Rides
    /// CCR for recovery, so it only runs when `enable_ccr` is also on.
    pub enable_schema_fold: bool,
    /// Only adopt a result strictly smaller than the original.
    pub min_savings_chars: usize,
}

impl Default for ConfigCompressorConfig {
    fn default() -> Self {
        Self {
            enable_ccr: true,
            enable_schema_fold: true,
            min_savings_chars: 1,
        }
    }
}

/// Result of structured-config compression.
#[derive(Debug, Clone)]
pub struct ConfigCompressionResult {
    pub compressed: String,
    pub original: String,
    pub was_modified: bool,
    /// `"yaml" | "toml" | "ini" | "unknown"`.
    pub flavor: String,
    pub lines_elided: usize,
    pub ccr_hash: Option<String>,
    pub strategy: String,
}

impl ConfigCompressionResult {
    fn passthrough(content: &str, flavor: &str) -> Self {
        Self {
            compressed: content.to_string(),
            original: content.to_string(),
            was_modified: false,
            flavor: flavor.to_string(),
            lines_elided: 0,
            ccr_hash: None,
            strategy: "config".to_string(),
        }
    }

    pub fn compression_ratio(&self) -> f64 {
        if self.original.is_empty() {
            return 0.0;
        }
        self.compressed.len() as f64 / self.original.len() as f64
    }
}

// ─── Compressor ──────────────────────────────────────────────────────────

/// Compresses YAML/TOML/INI text via reversible + CCR-recoverable tiers.
pub struct ConfigCompressor {
    pub config: ConfigCompressorConfig,
}

impl ConfigCompressor {
    pub fn new(config: ConfigCompressorConfig) -> Self {
        Self { config }
    }

    /// Compress `content`, storing originals through `store_original` when a
    /// lossy tier fires.
    ///
    /// `store_original` takes `(original, compressed)` and returns the CCR hash,
    /// or `None` when the write failed. A `None` disables the lossy tiers for
    /// this call rather than emitting an unrecoverable result — the engine does
    /// not own the store, so the caller supplies it (matching how the Rust port
    /// keeps CCR ownership in the dispatcher).
    pub fn compress(
        &self,
        content: &str,
        store_original: &dyn Fn(&str, &str) -> Option<String>,
    ) -> ConfigCompressionResult {
        let detection = detect_content_type(content);
        if detection.content_type != ContentType::StructuredConfig {
            return ConfigCompressionResult::passthrough(content, "unknown");
        }
        let flavor = detection
            .metadata
            .get("flavor")
            .and_then(|v| v.as_str())
            .unwrap_or("yaml")
            .to_string();

        // Tier 3 is computed FIRST so it can compete with the text tiers on
        // size; it wins on lockfiles and override-lists.
        let schema_fold = if self.config.enable_ccr && self.config.enable_schema_fold {
            self.schema_fold(content, &flavor, store_original)
        } else {
            None
        };

        let mut working = content.to_string();
        let mut lines_elided = 0usize;
        let mut ccr_hash: Option<String> = None;

        // Tier 2: comment/blank elision behind a CCR marker. The original is
        // persisted FIRST — the elided lines are only droppable because it is
        // recoverable.
        if self.config.enable_ccr && elision_safe(content, &flavor) {
            let (stripped, elided) = strip_comment_lines(content, &flavor);
            if elided > 0 {
                if let Some(hash) = store_original(content, &stripped) {
                    let marker = format!(
                        "[{elided} comment/blank lines elided. Retrieve original: hash={hash}]"
                    );
                    working = if stripped.ends_with('\n') {
                        format!("{stripped}{marker}")
                    } else {
                        format!("{stripped}\n{marker}")
                    };
                    lines_elided = elided;
                    ccr_hash = Some(hash);
                }
            }
        }

        // Tier 1: reversible folding; self-verified round-trip.
        let compressed = compact_lossless(&working, "config");

        // Prefer the schema fold when it beats the text tiers.
        if let Some((folded, hash)) = schema_fold {
            if folded.len() < compressed.len() {
                return ConfigCompressionResult {
                    compressed: folded,
                    original: content.to_string(),
                    was_modified: true,
                    flavor,
                    lines_elided: 0,
                    ccr_hash: Some(hash),
                    strategy: "config_schema_fold".to_string(),
                };
            }
        }

        let savings = content.len().saturating_sub(compressed.len());
        if savings < self.config.min_savings_chars {
            return ConfigCompressionResult::passthrough(content, &flavor);
        }

        ConfigCompressionResult {
            compressed,
            original: content.to_string(),
            was_modified: true,
            flavor,
            lines_elided,
            ccr_hash,
            strategy: "config".to_string(),
        }
    }

    /// Fold a TOML array-of-tables into SmartCrusher csv-schema.
    ///
    /// Returns `(folded_text_with_marker, ccr_hash)` when the fold is strictly
    /// smaller and its original is safely stored; otherwise `None` so the caller
    /// keeps the text tiers. Only TOML is bridged: it has a real parser here, so
    /// the extracted records are ground truth and the csv-schema rendering is
    /// itself lossless — the model reads a faithful, reformatted view.
    fn schema_fold(
        &self,
        content: &str,
        flavor: &str,
        store_original: &dyn Fn(&str, &str) -> Option<String>,
    ) -> Option<(String, String)> {
        if flavor != "toml" || !content.contains("[[") {
            return None;
        }
        let table: toml::Table = content.parse().ok()?;
        // A value we can't represent faithfully → don't fold.
        let json_str = serde_json::to_string(&table).ok()?;

        let crusher = super::smart_crusher::SmartCrusher::builder(Default::default())
            .with_default_oss_setup()
            .with_default_compaction()
            .build();
        let result = crusher.crush(&json_str, "", 1.0);
        // `passthrough` means SmartCrusher only re-canonicalized the JSON and
        // applied no schema fold, so there is nothing worth adopting.
        if !result.was_modified || result.strategy == "passthrough" {
            return None;
        }

        // Never emit a lossy form we can't recover.
        let hash = store_original(content, &result.compressed)?;
        let marker = format!("[config folded to schema. Retrieve original: hash={hash}]");
        let folded = format!("{}\n{marker}", result.compressed);
        if content.len().saturating_sub(folded.len()) < self.config.min_savings_chars {
            return None;
        }
        Some((folded, hash))
    }
}

/// False when a `#` line could be data rather than a comment.
///
/// Deliberately over-broad: when in doubt, Tier 2 stays off. A false negative
/// costs a little compression; a false positive silently deletes data.
fn elision_safe(content: &str, flavor: &str) -> bool {
    match flavor {
        "yaml" => !yaml_block_scalar_re().is_match(content),
        "toml" => !toml_multiline_re().is_match(content),
        _ => true,
    }
}

/// Drop whole-line comments (and, outside INI, blank lines).
///
/// Returns `(kept_text, elided_count)`.
fn strip_comment_lines(content: &str, flavor: &str) -> (String, usize) {
    let re = comment_re(flavor);
    let keep_blanks = flavor == "ini";
    let had_trailing = content.ends_with('\n');
    let body = if had_trailing {
        &content[..content.len() - 1]
    } else {
        content
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut elided = 0usize;
    for line in body.split('\n') {
        if re.is_match(line) || (!keep_blanks && line.trim().is_empty()) {
            elided += 1;
        } else {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n");
    if had_trailing {
        out.push('\n');
    }
    (out, elided)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Values verified against Python's `ConfigCompressor` on the same inputs.

    #[test]
    fn elision_safety_matches_python() {
        // Plain config: safe to elide comment lines.
        assert!(elision_safe("key: value\nother: 1\n", "yaml"));
        assert!(elision_safe("a = \"x\"\n", "toml"));
        assert!(elision_safe("[s]\nk = v\n", "ini"));
        // A `#` inside a YAML block scalar is DATA — eliding it would delete
        // part of a value, so the whole tier must switch off.
        assert!(!elision_safe(
            "script: |\n  # not a comment\n  echo hi\n",
            "yaml"
        ));
        // Same for a TOML multi-line string.
        assert!(!elision_safe(
            "a = \"\"\"\n# not a comment\n\"\"\"\n",
            "toml"
        ));
    }

    #[test]
    fn strip_comment_lines_matches_python() {
        // YAML: whole-line comments AND blanks go; 3 lines elided.
        let (out, n) = strip_comment_lines("# lead\nkey: value\n\n# mid\nother: 2\n", "yaml");
        assert_eq!(out, "key: value\nother: 2\n");
        assert_eq!(n, 3);

        // INI: only COLUMN-0 comments elide, and blanks are kept — configparser
        // keeps blank lines inside multi-line values, and an indented `;` is a
        // continuation line, not a comment.
        let (out, n) = strip_comment_lines("# lead\n[s]\n\nk = v\n  ; indented not col0\n", "ini");
        assert_eq!(out, "[s]\n\nk = v\n  ; indented not col0\n");
        assert_eq!(n, 1);
    }

    #[test]
    fn non_config_content_passes_through() {
        let cc = ConfigCompressor::new(ConfigCompressorConfig::default());
        let prose = "This is a sentence. Here is another one. And a third follows.\n";
        let r = cc.compress(prose, &|_, _| Some("deadbeef".into()));
        assert!(!r.was_modified);
        assert_eq!(r.flavor, "unknown");
        assert_eq!(r.compressed, prose);
    }

    #[test]
    fn elision_is_skipped_when_the_store_write_fails() {
        // Nothing lossy may be emitted without a recoverable original.
        let cc = ConfigCompressor::new(ConfigCompressorConfig::default());
        let yaml =
            "# lead\napiVersion: v1\nkind: Pod\nmetadata:\n  name: x\n  labels:\n    app: y\n";
        let r = cc.compress(yaml, &|_, _| None);
        assert_eq!(r.lines_elided, 0, "no elision without a stored original");
        assert!(r.ccr_hash.is_none());
        assert!(
            !r.compressed.contains("Retrieve original"),
            "must not emit a retrieval marker it cannot honour"
        );
    }

    #[test]
    fn elision_emits_a_marker_when_the_original_is_stored() {
        // Comment-heavy config: elision clearly beats the ~60-char marker.
        // Python reports modified, 12 elided, 848 -> 135 bytes.
        let cc = ConfigCompressor::new(ConfigCompressorConfig {
            enable_schema_fold: false,
            ..Default::default()
        });
        let mut yaml = String::new();
        for i in 0..12 {
            yaml.push_str(&format!(
                "# explanatory comment line number {i} describing the setting below\n"
            ));
        }
        yaml.push_str("apiVersion: v1\nkind: Pod\nmetadata:\n  name: x\n  labels:\n    app: y\n");
        let r = cc.compress(&yaml, &|_, _| Some("abc123def456".into()));
        assert!(r.was_modified);
        assert_eq!(r.lines_elided, 12);
        assert_eq!(r.ccr_hash.as_deref(), Some("abc123def456"));
        assert!(r
            .compressed
            .contains("[12 comment/blank lines elided. Retrieve original: hash=abc123def456]"));
    }

    #[test]
    fn elision_is_declined_when_the_marker_costs_more_than_it_saves() {
        // A couple of short comments don't pay for a ~60-char retrieval marker.
        // Python returns the original unchanged here; so must we, or we'd
        // "compress" a block into a larger one.
        let cc = ConfigCompressor::new(ConfigCompressorConfig {
            enable_schema_fold: false,
            ..Default::default()
        });
        let yaml = "# lead\napiVersion: v1\n\n# mid\nkind: Pod\nmetadata:\n  name: x\n  labels:\n    app: y\n";
        let r = cc.compress(yaml, &|_, _| Some("abc123def456".into()));
        assert!(!r.was_modified);
        assert_eq!(r.compressed, yaml);
        assert_eq!(r.lines_elided, 0);
    }

    #[test]
    fn a_yaml_block_scalar_disables_elision_end_to_end() {
        let cc = ConfigCompressor::new(ConfigCompressorConfig::default());
        let yaml = "script: |\n  # keep me\n  echo hi\nname: run\nother: 1\nmore: 2\n";
        let r = cc.compress(yaml, &|_, _| Some("abc123def456".into()));
        assert!(
            r.compressed.contains("# keep me"),
            "block-scalar content must survive: {}",
            r.compressed
        );
        assert_eq!(r.lines_elided, 0);
    }
}
