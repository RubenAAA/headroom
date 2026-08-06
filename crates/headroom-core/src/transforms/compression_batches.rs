//! Bounded batching for small provider-extracted compression units.
//!
//! Rust port of `headroom/transforms/compression_batches.py`.
//!
//! A unit below the per-unit size floor is not worth a router round trip on its
//! own. This module groups compatible small units into one tagged envelope,
//! sends that envelope through a single compressor call, and splits the result
//! back apart — but only when the returned text is structurally intact.
//!
//! Two safety layers guard the split:
//!
//! * **Envelope tags.** Each entry is wrapped in
//!   `<headroom-batch-{nonce}-{entry_id}>…</headroom-batch-…>`, where `nonce` is
//!   a content hash of the batch. Parsing is strict and positional: the tags
//!   must appear in order, with nothing but whitespace between them and nothing
//!   trailing. Anything else and the whole batch falls back to passthrough.
//! * **CCR marker placeholders.** Retrieval markers are swapped for unique
//!   `[[HEADROOM_BATCH_CCR_…]]` tokens before compression so a compressor cannot
//!   fold two entries' markers together, and each placeholder must survive
//!   exactly once — in the entry it came from.
//!
//! Nothing is compressed partially: either every entry splits cleanly, or all of
//! them come back unchanged with a shared reason.
//!
//! # Divergence: no `ContentRouter` object
//!
//! Python takes a live `ContentRouter` instance and calls `router.compress(...)`
//! on it. Rust's content router is a set of free functions, so this port takes
//! the [`Compressor`] trait from [`compression_units`](super::compression_units)
//! instead — the same seam `compress_unit_with_router` uses. Two consequences:
//!
//! * Python's `target_ratio` parameter poked `router._runtime_target_ratio` for
//!   the duration of the call and restored it afterwards. There is no mutable
//!   per-call ratio on the trait, so that parameter has **no counterpart here**.
//!   A caller that needs a different target ratio configures it on the
//!   `Compressor` implementation it passes in.
//! * Python wraps the call in `try/except` and returns a `batch_router_error`
//!   passthrough on any exception. [`Compressor::compress`] is infallible, so
//!   that reason is unreachable from this function; it is still exposed as
//!   [`BATCH_ROUTER_ERROR`] for implementations that want to surface it.

use sha2::{Digest, Sha256};

use super::compression_units::{
    ccr_marker_re, is_structured_shell_output, lossy_unmarked_strategies, Compressor,
    RoutedCompressionUnit, TokenCounter, UnitCompressionResult,
};
use super::content_router::CompressionStrategy;
use super::tag_protector::{protect_tags, restore_tags};

/// Default ceiling on the combined UTF-8 byte size of one batch.
pub const DEFAULT_MAX_BATCH_BYTES: usize = 2048;

/// Default ceiling on the number of entries in one batch.
pub const DEFAULT_MAX_BATCH_UNITS: usize = 16;

/// Passthrough reason used when the compressor itself fails.
///
/// Python's `except` path. Unreachable through [`compress_batch_with_router`]
/// because [`Compressor::compress`] cannot fail; kept so callers that wrap a
/// fallible compressor can report the same reason string.
pub const BATCH_ROUTER_ERROR: &str = "batch_router_error";

/// Passthrough reason when the compressor returned the input unchanged.
pub const ROUTER_NO_CHANGE: &str = "router_no_change";

/// Passthrough reason when the returned envelope did not survive intact.
pub const BATCH_INVALID: &str = "batch_invalid";

/// One provider slot with a stable batch-local identifier.
#[derive(Debug, Clone)]
pub struct CompressionBatchEntry {
    /// Identifier unique within the batch; embedded in the envelope tag name.
    pub entry_id: String,
    /// The unit and its provider-owned slot reference.
    pub routed: RoutedCompressionUnit,
}

/// Compatible small units that share one future router invocation.
#[derive(Debug, Clone)]
pub struct CompressionBatch {
    /// Entries in envelope order.
    pub entries: Vec<CompressionBatchEntry>,
    /// Combined UTF-8 byte size of the entries' texts.
    pub text_bytes: usize,
}

/// Why [`build_compression_batches`] rejected its bounds.
///
/// Python raises `ValueError`; the messages match one-for-one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchBoundsError {
    /// `min_batch_bytes` was zero.
    MinBatchBytesNotPositive,
    /// `max_batch_bytes` was below `min_batch_bytes`.
    MaxBatchBytesBelowMin,
    /// `max_batch_units` was zero.
    MaxBatchUnitsNotPositive,
}

impl std::fmt::Display for BatchBoundsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchBoundsError::MinBatchBytesNotPositive => {
                write!(f, "min_batch_bytes must be positive")
            }
            BatchBoundsError::MaxBatchBytesBelowMin => {
                write!(f, "max_batch_bytes must be at least min_batch_bytes")
            }
            BatchBoundsError::MaxBatchUnitsNotPositive => {
                write!(f, "max_batch_units must be positive")
            }
        }
    }
}

impl std::error::Error for BatchBoundsError {}

/// Bounds for [`build_compression_batches`].
///
/// Python passes these as keyword-only arguments with defaults for the two
/// ceilings; [`BatchBounds::new`] is the equivalent entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchBounds {
    /// A group must reach this many bytes to become a batch. Also the
    /// per-unit floor: a unit at or above it is already worth its own
    /// compression pass and is never batched.
    pub min_batch_bytes: usize,
    /// Ceiling on a batch's combined bytes.
    pub max_batch_bytes: usize,
    /// Ceiling on a batch's entry count.
    pub max_batch_units: usize,
}

impl BatchBounds {
    /// Bounds with Python's default ceilings.
    pub fn new(min_batch_bytes: usize) -> Self {
        Self {
            min_batch_bytes,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
            max_batch_units: DEFAULT_MAX_BATCH_UNITS,
        }
    }

    fn validate(&self) -> Result<(), BatchBoundsError> {
        if self.min_batch_bytes == 0 {
            return Err(BatchBoundsError::MinBatchBytesNotPositive);
        }
        if self.max_batch_bytes < self.min_batch_bytes {
            return Err(BatchBoundsError::MaxBatchBytesBelowMin);
        }
        if self.max_batch_units == 0 {
            return Err(BatchBoundsError::MaxBatchUnitsNotPositive);
        }
        Ok(())
    }
}

/// UTF-8 byte length of `text`.
///
/// Python encodes with `errors="replace"`; a Rust `str` is always valid UTF-8,
/// so the replacement path cannot trigger and `len()` is the same number.
fn text_bytes(text: &str) -> usize {
    text.len()
}

/// Fields that must match for two units to share one compressor call.
///
/// Mirrors Python's `_compatibility_key` tuple. `bias` is compared with `f64`
/// equality, matching Python's element-wise tuple comparison.
#[derive(Debug, Clone, PartialEq)]
struct CompatibilityKey {
    provider: String,
    endpoint: String,
    role: String,
    cache_zone: String,
    mutable: bool,
    context: String,
    question: Option<String>,
    bias: f64,
}

fn compatibility_key(entry: &CompressionBatchEntry) -> CompatibilityKey {
    let unit = &entry.routed.unit;
    CompatibilityKey {
        provider: unit.provider.clone(),
        endpoint: unit.endpoint.clone(),
        role: unit.role.clone(),
        cache_zone: unit.cache_zone.clone(),
        mutable: unit.mutable,
        context: unit.context.clone(),
        question: unit.question.clone(),
        bias: unit.bias,
    }
}

/// Greedily group compatible small units and skip under-floor tails.
///
/// Returns `(batches, skipped)`. Callers retain the skipped entries as normal
/// `size_floor` results. A unit larger than `max_batch_bytes` deliberately does
/// **not** become a singleton batch; those units belong to the existing
/// independent compression path.
///
/// The scan is single-pass and order-preserving: a group flushes when the next
/// entry is incompatible, when the entry count hits `max_batch_units`, or when
/// adding the entry would cross `max_batch_bytes`. A flushed group that never
/// reached `min_batch_bytes` is skipped rather than batched.
// The final `flush!()` resets `pending_bytes`/`pending_key` that nothing reads
// afterwards — the cost of keeping Python's flush body in one place.
#[allow(unused_assignments)]
pub fn build_compression_batches(
    entries: &[CompressionBatchEntry],
    bounds: BatchBounds,
) -> Result<(Vec<CompressionBatch>, Vec<CompressionBatchEntry>), BatchBoundsError> {
    bounds.validate()?;

    let mut batches: Vec<CompressionBatch> = Vec::new();
    let mut skipped: Vec<CompressionBatchEntry> = Vec::new();
    let mut pending: Vec<CompressionBatchEntry> = Vec::new();
    let mut pending_bytes: usize = 0;
    let mut pending_key: Option<CompatibilityKey> = None;

    // Closure-free flush: Rust's borrow checker dislikes Python's `nonlocal`
    // shape here, so the body is a macro over the same four locals.
    macro_rules! flush {
        () => {
            if !pending.is_empty() {
                if pending_bytes >= bounds.min_batch_bytes {
                    batches.push(CompressionBatch {
                        entries: std::mem::take(&mut pending),
                        text_bytes: pending_bytes,
                    });
                } else {
                    skipped.append(&mut pending);
                }
                pending_bytes = 0;
                pending_key = None;
            }
        };
    }

    for entry in entries {
        let entry_bytes = text_bytes(&entry.routed.unit.text);
        let entry_key = compatibility_key(entry);
        if entry_bytes >= bounds.min_batch_bytes || entry_bytes > bounds.max_batch_bytes {
            flush!();
            skipped.push(entry.clone());
            continue;
        }
        if !pending.is_empty()
            && (Some(&entry_key) != pending_key.as_ref()
                || pending.len() >= bounds.max_batch_units
                || pending_bytes + entry_bytes > bounds.max_batch_bytes)
        {
            flush!();
        }
        pending.push(entry.clone());
        pending_bytes += entry_bytes;
        pending_key = Some(entry_key);
        if pending.len() == bounds.max_batch_units || pending_bytes == bounds.max_batch_bytes {
            flush!();
        }
    }

    flush!();
    Ok((batches, skipped))
}

/// Content-derived tag nonce: first 12 hex chars of a SHA-256 over the entries.
///
/// Byte-for-byte identical to Python's `_batch_nonce` — each entry contributes
/// `entry_id`, a NUL, the unit text, and another NUL.
fn batch_nonce(batch: &CompressionBatch) -> String {
    let mut digest = Sha256::new();
    for entry in &batch.entries {
        digest.update(entry.entry_id.as_bytes());
        digest.update(b"\0");
        digest.update(entry.routed.unit.text.as_bytes());
        digest.update(b"\0");
    }
    hex::encode(digest.finalize())[..12].to_string()
}

/// Wrap each text in its per-entry tag and join with newlines.
fn batch_envelope(batch: &CompressionBatch, nonce: &str, texts: &[String]) -> String {
    batch
        .entries
        .iter()
        .zip(texts.iter())
        .map(|(entry, text)| {
            let tag = format!("headroom-batch-{nonce}-{}", entry.entry_id);
            format!("<{tag}>{text}</{tag}>")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A CCR marker lifted out of an entry: `(placeholder, entry_index, marker)`.
type MarkerBlock = (String, usize, String);

/// Replace retrieval markers with unique tokens before the single router call.
///
/// The placeholder carries the entry index, so a marker that migrates to the
/// wrong entry is detectable after the split. Returns the protected texts in
/// entry order and the marker blocks in discovery order (Python relies on dict
/// insertion order for the same thing).
fn protect_ccr_markers(batch: &CompressionBatch, nonce: &str) -> (Vec<String>, Vec<MarkerBlock>) {
    let mut protected_texts: Vec<String> = Vec::with_capacity(batch.entries.len());
    let mut marker_blocks: Vec<MarkerBlock> = Vec::new();
    let re = ccr_marker_re();

    for (entry_index, entry) in batch.entries.iter().enumerate() {
        let text = &entry.routed.unit.text;
        let mut marker_index = 0usize;
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        for m in re.find_iter(text) {
            let placeholder =
                format!("[[HEADROOM_BATCH_CCR_{nonce}_{entry_index}_{marker_index}]]");
            marker_index += 1;
            out.push_str(&text[last..m.start()]);
            out.push_str(&placeholder);
            last = m.end();
            marker_blocks.push((placeholder, entry_index, m.as_str().to_string()));
        }
        out.push_str(&text[last..]);
        protected_texts.push(out);
    }
    (protected_texts, marker_blocks)
}

/// Return ordered entry bodies only when every expected tag is intact.
///
/// Strictly positional: leading whitespace is tolerated before each opening
/// tag, the tags must appear in entry order, and only whitespace may follow the
/// last closing tag. Anything else returns `None` and the caller falls back to
/// passthrough for the whole batch.
///
/// Python compiles a `re.DOTALL` pattern and calls `Pattern.match(text, cursor)`
/// — an anchored, non-greedy match. This does the same with literal string
/// search: `starts_with` for the anchor, first `find` of the closing tag for the
/// non-greedy body.
fn parse_batch_envelope(text: &str, batch: &CompressionBatch, nonce: &str) -> Option<Vec<String>> {
    let mut cursor = 0usize;
    let mut values: Vec<String> = Vec::with_capacity(batch.entries.len());
    for entry in &batch.entries {
        // Python's `str.isspace()`; `char::is_whitespace` differs only on the
        // C0 separators \x1c–\x1f, which Python treats as space and Rust does
        // not. They cannot appear in a well-formed envelope.
        while let Some(c) = text[cursor..].chars().next() {
            if c.is_whitespace() {
                cursor += c.len_utf8();
            } else {
                break;
            }
        }
        let tag = format!("headroom-batch-{nonce}-{}", entry.entry_id);
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if !text[cursor..].starts_with(&open) {
            return None;
        }
        let body_start = cursor + open.len();
        let rel_end = text[body_start..].find(&close)?;
        values.push(text[body_start..body_start + rel_end].to_string());
        cursor = body_start + rel_end + close.len();
    }
    if !text[cursor..].trim().is_empty() {
        return None;
    }
    Some(values)
}

/// Every entry returned unchanged with a shared reason.
///
/// Note `reason_category` is set to the raw `reason`, not the bucketed value
/// from `categorize_reason` — this matches Python, which passes
/// `reason_category=reason` here.
fn passthrough_batch_results(
    batch: &CompressionBatch,
    tokenizer: &dyn TokenCounter,
    reason: &str,
    router_result: Option<&crate::transforms::content_router::RouterCompressionResult>,
) -> Vec<(serde_json::Value, UnitCompressionResult)> {
    let strategy = match router_result {
        Some(r) => r.strategy_used.as_str().to_string(),
        None => CompressionStrategy::Passthrough.as_str().to_string(),
    };
    batch
        .entries
        .iter()
        .map(|entry| {
            let text = &entry.routed.unit.text;
            let tokens = tokenizer.count_text(text);
            (
                entry.routed.slot.clone(),
                UnitCompressionResult {
                    original: text.clone(),
                    compressed: text.clone(),
                    modified: false,
                    tokens_before: tokens,
                    tokens_after: tokens,
                    tokens_saved: 0,
                    transforms_applied: Vec::new(),
                    strategy: strategy.clone(),
                    reason: Some(reason.to_string()),
                    router_result: router_result.cloned(),
                    text_bytes: text_bytes(text),
                    min_bytes: entry.routed.unit.min_bytes,
                    reason_category: reason.to_string(),
                },
            )
        })
        .collect()
}

/// Compress one tagged batch and split only structurally valid output.
///
/// Returns one `(slot, result)` pair per entry, in entry order. Every failure
/// mode — no change, a mangled envelope, a lost or duplicated placeholder —
/// returns the whole batch unmodified rather than a partial split.
///
/// See the module docs for the `target_ratio` and error-path divergences from
/// Python.
pub fn compress_batch_with_router(
    batch: &CompressionBatch,
    compressor: &dyn Compressor,
    tokenizer: &dyn TokenCounter,
) -> Vec<(serde_json::Value, UnitCompressionResult)> {
    let nonce = batch_nonce(batch);
    let (batch_texts, marker_blocks) = protect_ccr_markers(batch, &nonce);
    let envelope = batch_envelope(batch, &nonce, &batch_texts);
    let (protected, protected_blocks, _stats) = protect_tags(&envelope, true);

    let first = &batch.entries[0].routed.unit;
    let router_result = compressor.compress(
        &protected,
        &first.context,
        first.question.as_deref(),
        first.bias,
    );

    let compressed = router_result.compressed.clone();
    if compressed.is_empty() || compressed == protected {
        return passthrough_batch_results(batch, tokenizer, ROUTER_NO_CHANGE, Some(&router_result));
    }

    // Every protected placeholder — tag-protector blocks first, then CCR
    // markers — must survive exactly once, or the split cannot be trusted.
    let survives_once = |placeholder: &str| compressed.matches(placeholder).count() == 1;
    let all_intact = protected_blocks
        .iter()
        .map(|(placeholder, _)| placeholder.as_str())
        .chain(marker_blocks.iter().map(|(p, _, _)| p.as_str()))
        .all(survives_once);
    if !all_intact {
        return passthrough_batch_results(batch, tokenizer, BATCH_INVALID, Some(&router_result));
    }

    let restored = restore_tags(&compressed, &protected_blocks);
    let mut replacements = match parse_batch_envelope(&restored, batch, &nonce) {
        Some(values) => values,
        None => {
            return passthrough_batch_results(batch, tokenizer, BATCH_INVALID, Some(&router_result))
        }
    };

    // A placeholder must land back in the entry it came from, exactly once.
    let misplaced = marker_blocks.iter().any(|(placeholder, entry_index, _)| {
        replacements[*entry_index].matches(placeholder).count() != 1
    });
    if misplaced {
        return passthrough_batch_results(batch, tokenizer, BATCH_INVALID, Some(&router_result));
    }
    for (placeholder, entry_index, marker) in &marker_blocks {
        replacements[*entry_index] = replacements[*entry_index].replace(placeholder, marker);
    }

    let strategy = router_result.strategy_used.as_str().to_string();
    let lossy_unmarked = lossy_unmarked_strategies().contains(strategy.as_str());

    batch
        .entries
        .iter()
        .zip(replacements.into_iter())
        .map(|(entry, replacement)| {
            let unit = &entry.routed.unit;
            let tokens_before = tokenizer.count_text(&unit.text);
            let tokens_after = tokenizer.count_text(&replacement);

            // Shell output compressed by a lossy strategy that left no CCR
            // marker is unrecoverable — keep the bytes, report the attempt.
            let unrecoverable_tool_output = unit.role == "tool"
                && unit.item_type == "local_shell_call_output"
                && is_structured_shell_output(&unit.text)
                && lossy_unmarked
                && !ccr_marker_re().is_match(&replacement);

            let result = if unrecoverable_tool_output {
                UnitCompressionResult {
                    original: unit.text.clone(),
                    compressed: replacement,
                    modified: false,
                    tokens_before,
                    tokens_after,
                    tokens_saved: 0,
                    transforms_applied: Vec::new(),
                    strategy: strategy.clone(),
                    reason: Some("lossy_unrecoverable_tool_output".to_string()),
                    router_result: Some(router_result.clone()),
                    text_bytes: text_bytes(&unit.text),
                    min_bytes: unit.min_bytes,
                    reason_category: "other".to_string(),
                }
            } else if tokens_after >= tokens_before {
                UnitCompressionResult {
                    original: unit.text.clone(),
                    compressed: replacement,
                    modified: false,
                    tokens_before,
                    tokens_after,
                    tokens_saved: 0,
                    transforms_applied: Vec::new(),
                    strategy: strategy.clone(),
                    reason: Some("rejected_not_smaller".to_string()),
                    router_result: Some(router_result.clone()),
                    text_bytes: text_bytes(&unit.text),
                    min_bytes: unit.min_bytes,
                    reason_category: "rejected_not_smaller".to_string(),
                }
            } else {
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
                    strategy: strategy.clone(),
                    reason: None,
                    router_result: Some(router_result.clone()),
                    text_bytes: text_bytes(&unit.text),
                    min_bytes: unit.min_bytes,
                    reason_category: "applied".to_string(),
                }
            };
            (entry.routed.slot.clone(), result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transforms::compression_units::CompressionUnit;
    use crate::transforms::content_router::RouterCompressionResult;

    fn unit(text: &str) -> CompressionUnit {
        CompressionUnit {
            text: text.to_string(),
            provider: "openai".to_string(),
            endpoint: "responses".to_string(),
            role: "tool".to_string(),
            item_type: "function_call_output".to_string(),
            cache_zone: "live".to_string(),
            mutable: true,
            context: String::new(),
            question: None,
            bias: 1.0,
            min_bytes: 512,
            metadata: Default::default(),
        }
    }

    fn entry(entry_id: &str, text: &str) -> CompressionBatchEntry {
        CompressionBatchEntry {
            entry_id: entry_id.to_string(),
            routed: RoutedCompressionUnit {
                unit: unit(text),
                slot: serde_json::json!(entry_id),
            },
        }
    }

    fn entry_with(
        entry_id: &str,
        text: &str,
        mutate: impl FnOnce(&mut CompressionUnit),
    ) -> CompressionBatchEntry {
        let mut u = unit(text);
        mutate(&mut u);
        CompressionBatchEntry {
            entry_id: entry_id.to_string(),
            routed: RoutedCompressionUnit {
                unit: u,
                slot: serde_json::json!(entry_id),
            },
        }
    }

    fn batch_of(entries: Vec<CompressionBatchEntry>) -> CompressionBatch {
        let text_bytes = entries
            .iter()
            .map(|e| e.routed.unit.text.len())
            .sum::<usize>();
        CompressionBatch {
            entries,
            text_bytes,
        }
    }

    /// One token per whitespace-separated word — matches the Python stub used
    /// to measure the expected values below.
    struct WordTokenizer;

    impl TokenCounter for WordTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
    }

    /// A compressor that applies literal substitutions to whatever it is
    /// handed. The batch envelope reaches it with its tags already swapped for
    /// tag-protector placeholders, so substituting on the *unit text* is the
    /// only way to write a fixture that survives the placeholder checks.
    ///
    /// The Python harness used to measure the expected values below is the same
    /// shape: a fake router whose `compress` runs `str.replace` in order.
    struct ReplacingCompressor {
        replacements: Vec<(String, String)>,
        strategy: CompressionStrategy,
    }

    impl ReplacingCompressor {
        fn new(replacements: &[(&str, &str)]) -> Self {
            Self {
                replacements: replacements
                    .iter()
                    .map(|(a, b)| (a.to_string(), b.to_string()))
                    .collect(),
                strategy: CompressionStrategy::Text,
            }
        }

        fn with_strategy(mut self, strategy: CompressionStrategy) -> Self {
            self.strategy = strategy;
            self
        }
    }

    impl Compressor for ReplacingCompressor {
        fn compress(
            &self,
            content: &str,
            _context: &str,
            _question: Option<&str>,
            _bias: f64,
        ) -> RouterCompressionResult {
            let mut out = content.to_string();
            for (from, to) in &self.replacements {
                out = out.replace(from.as_str(), to.as_str());
            }
            RouterCompressionResult {
                compressed: out,
                original: content.to_string(),
                strategy_used: self.strategy,
                routing_log: Vec::new(),
                sections_processed: 1,
                strategy_chain: Vec::new(),
                cache_hit: false,
            }
        }
    }

    /// A compressor that ignores its input and returns a fixed string.
    struct ConstCompressor(&'static str);

    impl Compressor for ConstCompressor {
        fn compress(
            &self,
            content: &str,
            _context: &str,
            _question: Option<&str>,
            _bias: f64,
        ) -> RouterCompressionResult {
            RouterCompressionResult {
                compressed: self.0.to_string(),
                original: content.to_string(),
                strategy_used: CompressionStrategy::Text,
                routing_log: Vec::new(),
                sections_processed: 1,
                strategy_chain: Vec::new(),
                cache_hit: false,
            }
        }
    }

    // ── build_compression_batches ────────────────────────────────────────

    #[test]
    fn defaults_match_python() {
        assert_eq!(DEFAULT_MAX_BATCH_BYTES, 2048);
        assert_eq!(DEFAULT_MAX_BATCH_UNITS, 16);
    }

    #[test]
    fn rejects_invalid_bounds() {
        let bounds = BatchBounds {
            min_batch_bytes: 0,
            max_batch_bytes: 10,
            max_batch_units: 4,
        };
        assert_eq!(
            build_compression_batches(&[], bounds).unwrap_err(),
            BatchBoundsError::MinBatchBytesNotPositive
        );

        let bounds = BatchBounds {
            min_batch_bytes: 100,
            max_batch_bytes: 10,
            max_batch_units: 4,
        };
        assert_eq!(
            build_compression_batches(&[], bounds).unwrap_err(),
            BatchBoundsError::MaxBatchBytesBelowMin
        );

        let bounds = BatchBounds {
            min_batch_bytes: 10,
            max_batch_bytes: 100,
            max_batch_units: 0,
        };
        assert_eq!(
            build_compression_batches(&[], bounds).unwrap_err(),
            BatchBoundsError::MaxBatchUnitsNotPositive
        );
    }

    /// Measured against Python: three 10-byte units, floor 25 → one batch of
    /// all three (30 bytes), nothing skipped.
    #[test]
    fn groups_small_compatible_units_into_one_batch() {
        let entries = vec![
            entry("a", "0123456789"),
            entry("b", "abcdefghij"),
            entry("c", "ABCDEFGHIJ"),
        ];
        let bounds = BatchBounds {
            min_batch_bytes: 25,
            max_batch_bytes: 2048,
            max_batch_units: 16,
        };
        let (batches, skipped) = build_compression_batches(&entries, bounds).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 3);
        assert_eq!(batches[0].text_bytes, 30);
        assert!(skipped.is_empty());
    }

    /// Measured against Python: a group that never reaches the floor is
    /// skipped, not batched.
    #[test]
    fn under_floor_group_is_skipped() {
        let entries = vec![entry("a", "0123456789"), entry("b", "abcdefghij")];
        let bounds = BatchBounds {
            min_batch_bytes: 25,
            max_batch_bytes: 2048,
            max_batch_units: 16,
        };
        let (batches, skipped) = build_compression_batches(&entries, bounds).unwrap();
        assert!(batches.is_empty());
        assert_eq!(
            skipped
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    /// A unit at or above the floor never joins a batch; it flushes whatever
    /// was pending first. Measured against Python.
    #[test]
    fn at_floor_unit_is_skipped_and_flushes_pending() {
        let entries = vec![
            entry("a", "0123456789"),
            entry("big", &"x".repeat(30)),
            entry("b", "abcdefghij"),
            entry("c", "ABCDEFGHIJ"),
            entry("d", "klmnopqrst"),
        ];
        let bounds = BatchBounds {
            min_batch_bytes: 25,
            max_batch_bytes: 2048,
            max_batch_units: 16,
        };
        let (batches, skipped) = build_compression_batches(&entries, bounds).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0]
                .entries
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "d"]
        );
        assert_eq!(
            skipped
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "big"]
        );
    }

    /// Incompatible neighbours (different provider) break the group.
    /// Measured against Python: the first group is under the floor and skipped.
    #[test]
    fn incompatible_units_start_a_new_group() {
        let entries = vec![
            entry("a", "0123456789"),
            entry_with("b", "abcdefghij", |u| u.provider = "anthropic".to_string()),
            entry_with("c", "ABCDEFGHIJ", |u| u.provider = "anthropic".to_string()),
            entry_with("d", "klmnopqrst", |u| u.provider = "anthropic".to_string()),
        ];
        let bounds = BatchBounds {
            min_batch_bytes: 25,
            max_batch_bytes: 2048,
            max_batch_units: 16,
        };
        let (batches, skipped) = build_compression_batches(&entries, bounds).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0]
                .entries
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "d"]
        );
        assert_eq!(
            skipped
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    /// The unit ceiling flushes eagerly. Measured against Python: 5 units of
    /// 10 bytes with `max_batch_units=2` yields two 20-byte batches and one
    /// skipped tail.
    #[test]
    fn unit_ceiling_flushes_eagerly() {
        let entries = vec![
            entry("a", "0123456789"),
            entry("b", "abcdefghij"),
            entry("c", "ABCDEFGHIJ"),
            entry("d", "klmnopqrst"),
            entry("e", "KLMNOPQRST"),
        ];
        let bounds = BatchBounds {
            min_batch_bytes: 15,
            max_batch_bytes: 2048,
            max_batch_units: 2,
        };
        let (batches, skipped) = build_compression_batches(&entries, bounds).unwrap();
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|b| b.text_bytes == 20));
        assert_eq!(
            skipped
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["e"]
        );
    }

    /// Python's `entry_bytes > max_batch_bytes` arm is dead code under valid
    /// bounds: a unit above the ceiling is also above the floor, so the first
    /// half of the same condition already caught it. Bounds that would make it
    /// reachable (`max < min`) are rejected up front, in both languages.
    #[test]
    fn over_ceiling_arm_is_unreachable_under_valid_bounds() {
        let entries = vec![entry("big", &"x".repeat(40))];
        let bounds = BatchBounds {
            min_batch_bytes: 50,
            max_batch_bytes: 30,
            max_batch_units: 16,
        };
        // max < min is rejected up front, matching Python.
        assert_eq!(
            build_compression_batches(&entries, bounds).unwrap_err(),
            BatchBoundsError::MaxBatchBytesBelowMin
        );
    }

    /// Byte ceiling: adding the third 10-byte unit would exceed 25, so the
    /// group flushes at 20. Measured against Python.
    #[test]
    fn byte_ceiling_flushes_before_overflow() {
        let entries = vec![
            entry("a", "0123456789"),
            entry("b", "abcdefghij"),
            entry("c", "ABCDEFGHIJ"),
        ];
        let bounds = BatchBounds {
            min_batch_bytes: 15,
            max_batch_bytes: 25,
            max_batch_units: 16,
        };
        let (batches, skipped) = build_compression_batches(&entries, bounds).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].text_bytes, 20);
        assert_eq!(
            skipped
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
    }

    #[test]
    fn empty_input_produces_nothing() {
        let (batches, skipped) = build_compression_batches(&[], BatchBounds::new(100)).unwrap();
        assert!(batches.is_empty());
        assert!(skipped.is_empty());
    }

    // ── nonce + envelope ─────────────────────────────────────────────────

    /// Nonce measured from live Python `_batch_nonce`.
    #[test]
    fn nonce_matches_python() {
        let batch = batch_of(vec![entry("a", "hello"), entry("b", "world")]);
        assert_eq!(batch_nonce(&batch), "3ba54ee2e42e");
    }

    /// Envelope text measured from live Python `_batch_envelope`.
    #[test]
    fn envelope_matches_python() {
        let batch = batch_of(vec![entry("a", "hello"), entry("b", "world")]);
        let nonce = batch_nonce(&batch);
        let texts = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(
            batch_envelope(&batch, &nonce, &texts),
            "<headroom-batch-3ba54ee2e42e-a>hello</headroom-batch-3ba54ee2e42e-a>\n\
             <headroom-batch-3ba54ee2e42e-b>world</headroom-batch-3ba54ee2e42e-b>"
        );
    }

    /// Marker protection measured from live Python `_protect_ccr_markers`.
    #[test]
    fn ccr_markers_are_swapped_for_indexed_placeholders() {
        let batch = batch_of(vec![
            entry("a", "line one\nRetrieve more: hash=abc\nline three"),
            entry("b", "plain"),
        ]);
        let nonce = batch_nonce(&batch);
        let (texts, markers) = protect_ccr_markers(&batch, &nonce);
        assert_eq!(
            texts[0],
            format!("line one\n[[HEADROOM_BATCH_CCR_{nonce}_0_0]]\nline three")
        );
        assert_eq!(texts[1], "plain");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].1, 0);
        assert_eq!(markers[0].2, "Retrieve more: hash=abc");
    }

    // ── envelope parsing ─────────────────────────────────────────────────

    #[test]
    fn parses_a_well_formed_envelope() {
        let batch = batch_of(vec![entry("a", "hello"), entry("b", "world")]);
        let nonce = batch_nonce(&batch);
        let text = batch_envelope(&batch, &nonce, &["short".to_string(), "tiny".to_string()]);
        assert_eq!(
            parse_batch_envelope(&text, &batch, &nonce),
            Some(vec!["short".to_string(), "tiny".to_string()])
        );
    }

    #[test]
    fn rejects_a_missing_tag() {
        let batch = batch_of(vec![entry("a", "hello"), entry("b", "world")]);
        let nonce = batch_nonce(&batch);
        let text = format!("<headroom-batch-{nonce}-a>short</headroom-batch-{nonce}-a>");
        assert_eq!(parse_batch_envelope(&text, &batch, &nonce), None);
    }

    #[test]
    fn rejects_trailing_content() {
        let batch = batch_of(vec![entry("a", "hello")]);
        let nonce = batch_nonce(&batch);
        let text = format!("<headroom-batch-{nonce}-a>x</headroom-batch-{nonce}-a>trailing");
        assert_eq!(parse_batch_envelope(&text, &batch, &nonce), None);
    }

    #[test]
    fn rejects_out_of_order_tags() {
        let batch = batch_of(vec![entry("a", "hello"), entry("b", "world")]);
        let nonce = batch_nonce(&batch);
        let text = format!(
            "<headroom-batch-{nonce}-b>y</headroom-batch-{nonce}-b>\n\
             <headroom-batch-{nonce}-a>x</headroom-batch-{nonce}-a>"
        );
        assert_eq!(parse_batch_envelope(&text, &batch, &nonce), None);
    }
    // ── protect_tags interaction ─────────────────────────────────────────

    /// The compressor never sees the envelope tags — `protect_tags` swaps them
    /// for placeholders first. Measured against live Python `protect_tags`.
    #[test]
    fn envelope_tags_reach_the_compressor_as_placeholders() {
        let batch = batch_of(vec![entry("a", "hello"), entry("b", "world")]);
        let nonce = batch_nonce(&batch);
        let envelope = batch_envelope(&batch, &nonce, &["hello".to_string(), "world".to_string()]);
        let (protected, blocks, _stats) = protect_tags(&envelope, true);
        assert_eq!(
            protected,
            "{{HEADROOM_TAG_0}}hello{{HEADROOM_TAG_1}}\n{{HEADROOM_TAG_2}}world{{HEADROOM_TAG_3}}"
        );
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].1, format!("<headroom-batch-{nonce}-a>"));
    }

    // ── compress_batch_with_router ───────────────────────────────────────

    fn two_entry_batch() -> CompressionBatch {
        batch_of(vec![
            entry("a", "alpha beta gamma delta"),
            entry("b", "one two three four"),
        ])
    }

    /// Measured against live Python (`applied` scenario).
    #[test]
    fn splits_a_clean_envelope_and_marks_entries_applied() {
        let batch = two_entry_batch();
        let compressor = ReplacingCompressor::new(&[
            ("alpha beta gamma delta", "alpha"),
            ("one two three four", "one"),
        ]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        assert_eq!(results.len(), 2);
        let (slot, first) = &results[0];
        assert_eq!(slot, &serde_json::json!("a"));
        assert_eq!(first.compressed, "alpha");
        assert!(first.modified);
        assert_eq!(first.tokens_before, 4);
        assert_eq!(first.tokens_after, 1);
        assert_eq!(first.tokens_saved, 3);
        assert_eq!(first.strategy, "text");
        assert_eq!(first.reason, None);
        assert_eq!(first.reason_category, "applied");
        assert_eq!(first.text_bytes, 22);
        assert_eq!(first.min_bytes, 512);
        assert_eq!(
            first.transforms_applied,
            vec![
                "router:openai:responses:function_call_output:text".to_string(),
                "text".to_string(),
            ]
        );

        let (slot, second) = &results[1];
        assert_eq!(slot, &serde_json::json!("b"));
        assert_eq!(second.compressed, "one");
        assert!(second.modified);
        assert_eq!(second.tokens_saved, 3);
        assert_eq!(second.text_bytes, 18);
    }

    /// Measured against live Python (`nochange` scenario). Note the strategy
    /// comes from the router result, not `passthrough`, and `reason_category`
    /// is the raw reason rather than the `compressor_noop` bucket.
    #[test]
    fn unchanged_output_is_a_router_no_change_passthrough() {
        let batch = two_entry_batch();
        let compressor = ReplacingCompressor::new(&[]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        assert_eq!(results.len(), 2);
        for (i, expected) in ["alpha beta gamma delta", "one two three four"]
            .iter()
            .enumerate()
        {
            let r = &results[i].1;
            assert_eq!(r.compressed, *expected);
            assert!(!r.modified);
            assert_eq!(r.tokens_before, 4);
            assert_eq!(r.tokens_after, 4);
            assert_eq!(r.tokens_saved, 0);
            assert_eq!(r.strategy, "text");
            assert_eq!(r.reason.as_deref(), Some(ROUTER_NO_CHANGE));
            assert_eq!(r.reason_category, ROUTER_NO_CHANGE);
            assert!(r.transforms_applied.is_empty());
        }
    }

    #[test]
    fn empty_output_is_a_router_no_change_passthrough() {
        let batch = two_entry_batch();
        let compressor = ConstCompressor("");
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);
        assert!(results
            .iter()
            .all(|(_, r)| r.reason.as_deref() == Some(ROUTER_NO_CHANGE)));
    }

    /// Measured against live Python (`garbage` scenario): dropping one
    /// tag-protector placeholder invalidates the whole batch.
    #[test]
    fn a_lost_tag_placeholder_invalidates_the_batch() {
        let batch = two_entry_batch();
        let compressor = ReplacingCompressor::new(&[("{{HEADROOM_TAG_0}}", "ZZZ")]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1.compressed, "alpha beta gamma delta");
        assert_eq!(results[1].1.compressed, "one two three four");
        for (_, r) in &results {
            assert!(!r.modified);
            assert_eq!(r.reason.as_deref(), Some(BATCH_INVALID));
            assert_eq!(r.reason_category, BATCH_INVALID);
            assert_eq!(r.tokens_saved, 0);
        }
    }

    /// Measured against live Python (`const_garbage` scenario).
    #[test]
    fn unrelated_output_falls_back_for_the_whole_batch() {
        let batch = two_entry_batch();
        let compressor = ConstCompressor("totally unrelated text");
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, r)| r.reason.as_deref() == Some(BATCH_INVALID) && !r.modified));
        assert_eq!(results[0].1.compressed, "alpha beta gamma delta");
    }

    /// Measured against live Python (`not_smaller` scenario).
    #[test]
    fn output_that_is_not_smaller_is_rejected() {
        let batch = batch_of(vec![entry("a", "alpha")]);
        let compressor = ReplacingCompressor::new(&[("alpha", "alpha beta gamma")]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        let r = &results[0].1;
        assert_eq!(r.compressed, "alpha beta gamma");
        assert!(!r.modified);
        assert_eq!(r.tokens_before, 1);
        assert_eq!(r.tokens_after, 3);
        assert_eq!(r.tokens_saved, 0);
        assert_eq!(r.reason.as_deref(), Some("rejected_not_smaller"));
        assert_eq!(r.reason_category, "rejected_not_smaller");
        assert_eq!(r.text_bytes, 5);
    }

    fn ccr_batch() -> CompressionBatch {
        batch_of(vec![entry(
            "a",
            "head line\nRetrieve more: hash=abc\ntail line here",
        )])
    }

    /// Measured against live Python (`ccr_kept` scenario): a surviving
    /// placeholder is swapped back for the marker it replaced.
    #[test]
    fn a_surviving_ccr_placeholder_is_restored_to_its_marker() {
        let batch = ccr_batch();
        let compressor =
            ReplacingCompressor::new(&[("head line\n", "head\n"), ("\ntail line here", "")]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        let r = &results[0].1;
        assert_eq!(r.compressed, "head\nRetrieve more: hash=abc");
        assert!(r.modified);
        assert_eq!(r.tokens_before, 8);
        assert_eq!(r.tokens_after, 4);
        assert_eq!(r.tokens_saved, 4);
        assert_eq!(r.reason_category, "applied");
        assert_eq!(r.text_bytes, 48);
    }

    /// Measured against live Python (`ccr_dropped` scenario).
    #[test]
    fn a_dropped_ccr_placeholder_invalidates_the_batch() {
        let batch = ccr_batch();
        let nonce = batch_nonce(&batch);
        let placeholder = format!("[[HEADROOM_BATCH_CCR_{nonce}_0_0]]");
        let compressor = ReplacingCompressor::new(&[(placeholder.as_str(), "")]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        let r = &results[0].1;
        assert_eq!(
            r.compressed,
            "head line\nRetrieve more: hash=abc\ntail line here"
        );
        assert!(!r.modified);
        assert_eq!(r.reason.as_deref(), Some(BATCH_INVALID));
        assert_eq!(r.tokens_before, 8);
        assert_eq!(r.tokens_after, 8);
    }

    fn shell_batch() -> CompressionBatch {
        batch_of(vec![entry_with(
            "a",
            "line one\nline two\nline three",
            |u| {
                u.role = "tool".to_string();
                u.item_type = "local_shell_call_output".to_string();
            },
        )])
    }

    /// Measured against live Python (`shell_lossy` scenario): structured shell
    /// output compressed by a lossy strategy with no CCR marker left behind is
    /// reported but not applied.
    #[test]
    fn lossy_unmarked_shell_output_is_not_applied() {
        let batch = shell_batch();
        let compressor = ReplacingCompressor::new(&[("line one\nline two\nline three", "line")]);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        let r = &results[0].1;
        assert_eq!(r.compressed, "line");
        assert!(!r.modified);
        assert_eq!(r.tokens_before, 6);
        assert_eq!(r.tokens_after, 1);
        assert_eq!(r.tokens_saved, 0);
        assert_eq!(r.reason.as_deref(), Some("lossy_unrecoverable_tool_output"));
        assert_eq!(r.reason_category, "other");
        assert_eq!(r.text_bytes, 28);
    }

    /// Measured against live Python (`shell_search` scenario): the same output
    /// under a strategy outside `_LOSSY_UNMARKED_STRATEGIES` is applied.
    #[test]
    fn shell_output_under_a_non_lossy_strategy_is_applied() {
        let batch = shell_batch();
        let compressor = ReplacingCompressor::new(&[("line one\nline two\nline three", "line")])
            .with_strategy(CompressionStrategy::Search);
        let results = compress_batch_with_router(&batch, &compressor, &WordTokenizer);

        let r = &results[0].1;
        assert!(r.modified);
        assert_eq!(r.tokens_saved, 5);
        assert_eq!(r.strategy, "search");
        assert_eq!(r.reason_category, "applied");
        assert_eq!(
            r.transforms_applied,
            vec![
                "router:openai:responses:local_shell_call_output:search".to_string(),
                "search".to_string(),
            ]
        );
    }
}
