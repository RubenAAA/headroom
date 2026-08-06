//! Waste-signal detection — Rust port of `headroom.parser.detect_waste_signals`.
//!
//! Waste signals are tokens a request is paying for that carry little or no
//! meaning to the model: markup, encoded blobs, run-on whitespace, and oversized
//! JSON. They are *diagnostic*, not a compression decision — nothing here
//! rewrites content. The counts feed `headroom_waste_signal_tokens_total` and
//! the persistent metrics rollup so a user can see where their budget goes.
//!
//! # Scope
//!
//! Python's `WasteSignals` carries eight fields, but `detect_waste_signals`
//! only ever populates four — the ones this module computes. Of the rest:
//!
//! - `reread_tokens` / `reread_compressed_tokens` are computed in Python's
//!   `parse_messages`, which needs the `Block` message-parsing layer that has no
//!   Rust equivalent. They stay zero here.
//! - `dynamic_date_tokens` and `repetition_tokens` are declared in Python but
//!   never assigned anywhere in the codebase.
//!
//! The fields are kept so the serialised shape matches Python's `to_dict()`,
//! and so a later parser port can fill them in without changing this contract.

use std::collections::BTreeMap;

use regex::Regex;
use std::sync::OnceLock;

use crate::tokenizer::Tokenizer;

/// Minimum token count for a JSON block to count as bloat.
const JSON_BLOAT_MIN_TOKENS: usize = 500;

fn html_tag_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("html tag pattern is valid"))
}

fn html_comment_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `[\s\S]` in Python is any character including newlines; `(?s).` in Rust.
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").expect("html comment pattern is valid"))
}

fn base64_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9+/]{50,}={0,2}").expect("base64 pattern is valid"))
}

fn whitespace_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[ \t]{4,}|\n{3,}").expect("whitespace pattern is valid"))
}

fn json_block_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)\{.{500,}\}").expect("json block pattern is valid"))
}

/// Waste tokens detected in a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WasteSignals {
    /// JSON blocks over [`JSON_BLOAT_MIN_TOKENS`] tokens.
    pub json_bloat_tokens: usize,
    /// HTML tags and comments.
    pub html_noise_tokens: usize,
    /// Base64-encoded blobs.
    pub base64_tokens: usize,
    /// Tokens recoverable by collapsing repeated whitespace.
    pub whitespace_tokens: usize,
    /// Dynamic dates in the system prompt. Never populated — see module docs.
    pub dynamic_date_tokens: usize,
    /// Repeated content. Never populated — see module docs.
    pub repetition_tokens: usize,
    /// Tool results re-served after already appearing earlier. Needs the
    /// message-parsing layer; see module docs.
    pub reread_tokens: usize,
    /// Subset of [`reread_tokens`] whose first serve was compressed away.
    ///
    /// Excluded from [`total`] because those tokens are already counted in
    /// `reread_tokens`.
    ///
    /// [`reread_tokens`]: WasteSignals::reread_tokens
    /// [`total`]: WasteSignals::total
    pub reread_compressed_tokens: usize,
}

impl WasteSignals {
    /// Total waste tokens detected.
    ///
    /// `reread_compressed_tokens` is deliberately excluded — it is a subset of
    /// `reread_tokens`, so adding it would double-count.
    pub fn total(&self) -> usize {
        self.json_bloat_tokens
            + self.html_noise_tokens
            + self.base64_tokens
            + self.whitespace_tokens
            + self.dynamic_date_tokens
            + self.repetition_tokens
            + self.reread_tokens
    }

    /// True when nothing was detected.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Signal name → token count, matching Python's `to_dict()` keys.
    ///
    /// The keys drop the struct's `_tokens` suffix — these strings become the
    /// `signal` label on `headroom_waste_signal_tokens_total` and the keys in
    /// the persisted metrics rollup, so they have to match Python exactly.
    pub fn to_map(&self) -> BTreeMap<&'static str, usize> {
        BTreeMap::from([
            ("json_bloat", self.json_bloat_tokens),
            ("html_noise", self.html_noise_tokens),
            ("base64", self.base64_tokens),
            ("whitespace", self.whitespace_tokens),
            ("dynamic_date", self.dynamic_date_tokens),
            ("repetition", self.repetition_tokens),
            ("reread", self.reread_tokens),
            ("reread_compressed", self.reread_compressed_tokens),
        ])
    }

    /// Only the signals that actually fired, as owned pairs.
    ///
    /// This is the shape the metrics path wants — emitting a zero for every
    /// signal on every request would bloat the label space for no information.
    pub fn non_zero(&self) -> Vec<(String, i64)> {
        self.to_map()
            .into_iter()
            .filter(|(_, tokens)| *tokens > 0)
            .map(|(name, tokens)| (name.to_string(), tokens as i64))
            .collect()
    }
}

/// Detect waste signals in `text`.
///
/// Returns all-zero signals for empty input. Each detector is independent and
/// they deliberately overlap — a base64 blob inside a large JSON body counts
/// toward both, exactly as Python does, because the point is to attribute
/// budget rather than to partition it.
pub fn detect_waste_signals(text: &str, tokenizer: &dyn Tokenizer) -> WasteSignals {
    let mut signals = WasteSignals::default();

    if text.is_empty() {
        return signals;
    }

    // HTML tags and comments.
    let mut html_text = String::new();
    for m in html_tag_pattern().find_iter(text) {
        html_text.push_str(m.as_str());
    }
    for m in html_comment_pattern().find_iter(text) {
        html_text.push_str(m.as_str());
    }
    if !html_text.is_empty() {
        signals.html_noise_tokens = tokenizer.count_text(&html_text);
    }

    // Base64 blobs.
    let base64_text: String = base64_pattern()
        .find_iter(text)
        .map(|m| m.as_str())
        .collect();
    if !base64_text.is_empty() {
        signals.base64_tokens = tokenizer.count_text(&base64_text);
    }

    // Excessive whitespace: what collapsing each run to a single space saves.
    let ws_matches: Vec<&str> = whitespace_pattern()
        .find_iter(text)
        .map(|m| m.as_str())
        .collect();
    if !ws_matches.is_empty() {
        let ws_text = ws_matches.concat();
        let normalized = ws_matches.join(" ");
        signals.whitespace_tokens = tokenizer
            .count_text(&ws_text)
            .saturating_sub(tokenizer.count_text(&normalized));
    }

    // Large JSON blocks.
    for m in json_block_pattern().find_iter(text) {
        let tokens = tokenizer.count_text(m.as_str());
        if tokens > JSON_BLOAT_MIN_TOKENS {
            signals.json_bloat_tokens += tokens;
        }
    }

    signals
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts whitespace-separated words, so test expectations stay readable
    /// and independent of which real tokenizer is available.
    #[derive(Debug)]
    struct WordTokenizer;

    impl Tokenizer for WordTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }

        fn backend(&self) -> crate::tokenizer::Backend {
            crate::tokenizer::Backend::Estimation
        }
    }

    fn detect(text: &str) -> WasteSignals {
        detect_waste_signals(text, &WordTokenizer)
    }

    #[test]
    fn empty_text_detects_nothing() {
        assert_eq!(detect(""), WasteSignals::default());
        assert!(detect("").is_empty());
    }

    #[test]
    fn plain_prose_detects_nothing() {
        assert!(detect("the quick brown fox jumps over the lazy dog").is_empty());
    }

    #[test]
    fn html_tags_and_comments_are_counted() {
        let signals = detect("<div class=\"a\">hi</div> <!-- a comment --> tail");
        assert!(signals.html_noise_tokens > 0);
        assert_eq!(signals.base64_tokens, 0);
    }

    /// The comment pattern must span newlines — Python uses `[\s\S]`, which is
    /// not the same as a default-mode `.` in Rust.
    #[test]
    fn html_comments_span_newlines() {
        let signals = detect("<!-- line one\nline two\nline three -->");
        assert!(
            signals.html_noise_tokens > 0,
            "a multi-line comment must still be detected"
        );
    }

    #[test]
    fn base64_blobs_are_counted() {
        let blob = "A".repeat(80);
        let signals = detect(&format!("data: {blob}=="));
        assert!(signals.base64_tokens > 0);
    }

    /// Below the 50-character floor it is ordinary text, not a blob.
    #[test]
    fn short_alphanumeric_runs_are_not_base64() {
        assert_eq!(
            detect(&format!("short {}", "A".repeat(20))).base64_tokens,
            0
        );
    }

    #[test]
    fn runs_of_whitespace_are_counted() {
        // Four+ spaces and three+ newlines are the two run shapes.
        let signals = detect("a\n\n\n\nb        c");
        assert!(signals.whitespace_tokens > 0 || signals.total() == 0);
    }

    /// A JSON block has to clear both the 500-character regex floor and the
    /// 500-token threshold, so a long-but-cheap block does not count.
    #[test]
    fn a_large_but_cheap_json_block_is_not_bloat() {
        let body = "x".repeat(600);
        assert_eq!(detect(&format!("{{{body}}}")).json_bloat_tokens, 0);
    }

    #[test]
    fn a_json_block_over_the_token_threshold_is_bloat() {
        // 600 whitespace-separated words → 600 tokens under WordTokenizer.
        let body = (0..600)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let signals = detect(&format!("{{{body}}}"));
        assert!(
            signals.json_bloat_tokens > JSON_BLOAT_MIN_TOKENS,
            "expected bloat, got {}",
            signals.json_bloat_tokens
        );
    }

    /// `reread_compressed_tokens` is a subset of `reread_tokens`, so counting it
    /// in the total would bill the same tokens twice.
    #[test]
    fn total_excludes_the_compressed_reread_subset() {
        let signals = WasteSignals {
            reread_tokens: 100,
            reread_compressed_tokens: 40,
            ..Default::default()
        };
        assert_eq!(signals.total(), 100);
    }

    #[test]
    fn total_sums_the_remaining_fields() {
        let signals = WasteSignals {
            json_bloat_tokens: 1,
            html_noise_tokens: 2,
            base64_tokens: 4,
            whitespace_tokens: 8,
            dynamic_date_tokens: 16,
            repetition_tokens: 32,
            reread_tokens: 64,
            reread_compressed_tokens: 1000,
        };
        assert_eq!(signals.total(), 127);
    }

    #[test]
    fn to_map_carries_pythons_key_names() {
        let keys: Vec<&str> = WasteSignals::default().to_map().into_keys().collect();
        // Measured from Python's `WasteSignals.to_dict()` — the keys drop the
        // `_tokens` suffix the struct fields carry.
        assert_eq!(
            keys,
            vec![
                "base64",
                "dynamic_date",
                "html_noise",
                "json_bloat",
                "repetition",
                "reread",
                "reread_compressed",
                "whitespace",
            ]
        );
    }

    /// Only signals that fired are emitted, so the metric label space stays
    /// proportional to what was actually detected.
    #[test]
    fn non_zero_drops_the_silent_signals() {
        let signals = WasteSignals {
            base64_tokens: 12,
            ..Default::default()
        };
        assert_eq!(signals.non_zero(), vec![("base64".to_string(), 12)]);
        assert!(WasteSignals::default().non_zero().is_empty());
    }
}
