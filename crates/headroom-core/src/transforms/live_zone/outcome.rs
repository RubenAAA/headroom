//! Provider-agnostic result types: auth mode, per-block outcomes and
//! actions, exclusion reasons, the compression manifest, and errors.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

// ─── Public types ──────────────────────────────────────────────────────

/// Authentication mode of the originating request. Passed through to
/// the dispatcher so PR-F2 can vary policy without re-shaping the
/// public API. PR-B3 ignores the value (always treated as `Payg`).
///
/// Also reused by [`crate::transforms::recommendations`] (PR-B5) as the lookup
/// key prefix — keeping one canonical enum avoids drift between the
/// dispatcher's auth slice and the published recommendations'.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthMode {
    /// Pay-as-you-go API key. Most aggressive compression budget —
    /// every saved token is real money for the customer.
    Payg,
    /// OAuth-bearing client (e.g. Anthropic.com OAuth). Compression
    /// must not break the per-account routing the OAuth header pins;
    /// otherwise behaves like PAYG.
    OAuth,
    /// Subscription seat (e.g. Claude.ai usage). The provider
    /// already counts tokens against a fixed quota; aggressive
    /// compression is less compelling and may interact badly with
    /// rate-limit accounting.
    Subscription,
    /// Auth slice not yet detected. Matches the Python TOIN publish
    /// CLI's "unknown" default. Used by the recommendations loader
    /// (PR-B5) when an aggregation row didn't carry an auth tag.
    Unknown,
}

impl AuthMode {
    /// String form used as the recommendations-store lookup key.
    /// Mirrors the Python publish CLI tag values.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::Payg => "payg",
            AuthMode::OAuth => "oauth",
            AuthMode::Subscription => "subscription",
            AuthMode::Unknown => "unknown",
        }
    }
}

/// Map F1's classifier output (`crate::auth_mode::AuthMode`) to the
/// dispatcher-local enum. The two enums differ only by `Unknown` (the
/// dispatcher carries a sentinel for the case where a stored
/// recommendation row didn't include an auth tag); F1 always returns
/// one of the three real classes, so this `From` is total and
/// infallible.
impl From<crate::auth_mode::AuthMode> for AuthMode {
    fn from(mode: crate::auth_mode::AuthMode) -> Self {
        match mode {
            crate::auth_mode::AuthMode::Payg => AuthMode::Payg,
            crate::auth_mode::AuthMode::OAuth => AuthMode::OAuth,
            crate::auth_mode::AuthMode::Subscription => AuthMode::Subscription,
        }
    }
}

/// Per-block decision recorded for observability. Independent of
/// whether the body was actually rewritten.
#[derive(Debug, Clone)]
pub struct BlockOutcome {
    /// Index into the `messages` array.
    pub message_index: usize,
    /// Index into the message's `content` array. `None` when the
    /// content is a plain string (Anthropic accepts both shapes).
    pub block_index: Option<usize>,
    /// Block kind detected on this slot. `text`, `tool_result`,
    /// `tool_use`, `image`, ... or `string_content` for the
    /// string-shaped fallback.
    pub block_type: String,
    /// What the dispatcher decided.
    pub action: BlockAction,
}

/// Disposition of one block.
#[derive(Debug, Clone)]
pub enum BlockAction {
    /// Content type was inspected, no compressor was applicable.
    /// Examples: `PlainText` when the Kompress model isn't cached,
    /// `Html` (no compressor), `Image` (binary), unknown shapes.
    NoCompressionApplied {
        /// String form of the detected content type — `"text"`,
        /// `"source_code"`, `"html"`, `"image"`, `"unknown"`, etc.
        content_type: String,
        /// The compressor that ran and declined because its output was not
        /// smaller, when that is what happened. `None` covers the ordinary
        /// case: no compressor was applicable in the first place.
        ///
        /// The two look identical from the outside and are not the same
        /// thing. Without this the size gate would silently absorb the work
        /// it declines — `proxy_compression_rejected_by_token_check_total`
        /// drops, and there is no way to tell "we stopped wasting runs" from
        /// "we stopped attempting compression that paid".
        declined_by: Option<String>,
    },
    /// A compressor ran and produced a smaller output (in tokens, as
    /// counted by the model's tokenizer) that was spliced into the
    /// body. Both byte and token counts are reported so the proxy
    /// can log the savings ratio in either currency.
    Compressed {
        /// Identifier of the compressor (`"smart_crusher"`,
        /// `"log_compressor"`, ...). Static so the manifest is
        /// allocation-light.
        strategy: &'static str,
        /// Bytes of the original block content (the JSON string
        /// value, after unescaping).
        original_bytes: usize,
        /// Bytes of the replacement block content.
        compressed_bytes: usize,
        /// Tokens in the original block content (per the model's
        /// tokenizer).
        original_tokens: usize,
        /// Tokens in the replacement block content. Always strictly
        /// less than `original_tokens` for this variant — the
        /// tokenizer-validated rejection gate (PR-B4) maps the
        /// `>=` case to `RejectedNotSmaller`.
        compressed_tokens: usize,
    },
    /// A compressor was tried but failed loudly. Per project memory
    /// `feedback_no_silent_fallbacks.md`: surface the error in the
    /// manifest; the proxy logs warn-level and forwards the original
    /// bytes for that block (other blocks in the same body still get
    /// compressed normally).
    CompressorError {
        /// Identifier of the compressor that failed.
        strategy: &'static str,
        /// Human-readable error string (from `Display`).
        error: String,
    },
    /// A compressor ran but produced output that did not shrink the
    /// token count. Cache safety + "don't make it worse" → keep the
    /// original. PR-B4 wired the tokenizer-validated check; both
    /// byte and token counts are reported for observability.
    RejectedNotSmaller {
        /// Identifier of the compressor that was rejected.
        strategy: &'static str,
        /// Original block-content size, bytes.
        original_bytes: usize,
        /// Would-be compressed-block-content size, bytes.
        compressed_bytes: usize,
        /// Original block-content size, tokens.
        original_tokens: usize,
        /// Would-be compressed-block-content size, tokens. Always
        /// `>= original_tokens` (otherwise this would be
        /// `Compressed`).
        compressed_tokens: usize,
    },
    /// The block content was below the per-content-type byte
    /// threshold; no compressor was invoked. The dispatcher does
    /// not even spin up the tokenizer for these — they're below the
    /// per-call overhead so the marginal savings are negative.
    BelowByteThreshold {
        /// Detected content type — string tag matches
        /// `ContentType::as_str`.
        content_type: &'static str,
        /// Bytes in the block content.
        byte_count: usize,
        /// Threshold (in bytes) the content failed to clear.
        threshold_bytes: usize,
    },
    /// Block type is intentionally outside the live zone (e.g.
    /// `tool_use` → cache hot zone) and is excluded from dispatch.
    Excluded { reason: ExclusionReason },
}

/// Why a block was not eligible for compression.
#[derive(Debug, Clone, Copy)]
pub enum ExclusionReason {
    /// Block is in a message at index `< frozen_message_count`.
    BelowFrozenFloor,
    /// Block belongs to a message above the latest user message
    /// boundary (e.g. an older assistant turn).
    AboveLiveZone,
    /// Block type is on the cache-hot list (e.g. `tool_use`,
    /// `thinking`, `redacted_thinking`).
    HotZoneBlockType,
    /// Block is the result of a `headroom_retrieve` call — the bytes
    /// the model just asked to have restored from the CCR store.
    /// Compressing them again writes a fresh `<<ccr:hash>>` marker
    /// nobody can redeem.
    CcrRetrieveResult,
    /// Block was already replaced by a ctx-offload digest (`<<ctx:hash>>`).
    /// Offload runs before this pass and has already stored the original
    /// under that hash; the digest left behind is a preview plus a retrieval
    /// pointer. Compressing it again buys almost nothing — the block is
    /// already ~3KB against a 50KB offload floor — and writes a second
    /// `<<ccr:hash>>` marker whose hash resolves to the digest rather than
    /// the original, so a model following the inner pointer gets a lossy
    /// copy back and the true bytes become unreachable.
    CtxOffloadDigest,
    /// Block is the result of a tool the operator named in
    /// `--exclude-tools`. Lossy compression is off for it; the block
    /// still gets a byte-reversible fold when its shape supports one,
    /// so this reason means "nothing shrank it", not "never touched".
    ExcludedTool,
    /// Block is the output of a file read (`cat`, `sed -n`, `head`) whose
    /// content is not confidently data, and `HEADROOM_PROTECT_READS` is on.
    /// The agent is about to patch these bytes and needs them exactly.
    ProtectedRead,
}

/// Aggregated per-request manifest. Always populated, regardless of
/// whether any bytes were written.
#[derive(Debug, Clone)]
pub struct CompressionManifest {
    /// Total messages in the input array. Matches
    /// `body.messages.len()`.
    pub messages_total: usize,
    /// Messages with index `< frozen_message_count`. Untouched.
    pub messages_below_frozen_floor: usize,
    /// Index of the latest user message in the live zone, if any.
    pub latest_user_message_index: Option<usize>,
    /// Per-block outcomes for the latest user message. Empty when
    /// the live zone has no eligible blocks (or the body has no
    /// messages).
    pub block_outcomes: Vec<BlockOutcome>,
}

impl CompressionManifest {
    pub(super) fn empty() -> Self {
        Self {
            messages_total: 0,
            messages_below_frozen_floor: 0,
            latest_user_message_index: None,
            block_outcomes: Vec::new(),
        }
    }

    /// True when at least one block was actually rewritten by a
    /// compressor (used to discriminate the `Modified` arm from
    /// `NoChange`).
    pub(super) fn has_compressed_block(&self) -> bool {
        self.block_outcomes
            .iter()
            .any(|b| matches!(b.action, BlockAction::Compressed { .. }))
    }

    /// Aggregate `original_tokens − compressed_tokens` across every
    /// `BlockAction::Compressed` outcome. Zero when no block was
    /// rewritten. Saturating subtraction guards against the
    /// theoretically-impossible case where a `Compressed` variant
    /// reports compressed > original (the dispatcher's
    /// `RejectedNotSmaller` gate should make this unreachable, but the
    /// saturating arithmetic keeps callers panic-free).
    pub fn tokens_saved(&self) -> usize {
        self.block_outcomes
            .iter()
            .filter_map(|b| match &b.action {
                BlockAction::Compressed {
                    original_tokens,
                    compressed_tokens,
                    ..
                } => Some(original_tokens.saturating_sub(*compressed_tokens)),
                _ => None,
            })
            .sum()
    }

    /// Distinct compressor strategies that actually produced rewritten
    /// output, in first-seen order. Mirrors what the proxy logs as
    /// `transforms_applied`. Empty when no block was rewritten.
    pub fn transforms_applied(&self) -> Vec<&'static str> {
        let mut seen: Vec<&'static str> = Vec::new();
        for b in &self.block_outcomes {
            if let BlockAction::Compressed { strategy, .. } = &b.action
                && !seen.contains(strategy)
            {
                seen.push(*strategy);
            }
        }
        seen
    }
}

/// Summarize why a Responses live-zone dispatch made no changes.
///
/// The proxy uses this to log stable, grep-able reasons instead of the
/// generic `rust_no_compression` bucket. The classification is
/// intentionally coarse: operators want to know whether the dispatcher
/// saw no eligible items, hit a size floor, rejected output as not
/// smaller, or encountered a compressor error.
pub fn summarize_openai_responses_no_change_reason(manifest: &CompressionManifest) -> &'static str {
    if manifest.block_outcomes.is_empty() {
        return "no_eligible_items";
    }

    let mut saw_no_compression_applied = false;
    let mut saw_excluded = false;
    let mut saw_below_output_floor = false;
    let mut saw_below_plain_text_floor = false;
    let mut saw_rejected_not_smaller = false;
    let mut saw_compressor_error = false;

    for outcome in &manifest.block_outcomes {
        match &outcome.action {
            BlockAction::CompressorError { .. } => saw_compressor_error = true,
            BlockAction::RejectedNotSmaller { .. } => saw_rejected_not_smaller = true,
            BlockAction::BelowByteThreshold { content_type, .. } => {
                if *content_type == "output_item" {
                    saw_below_output_floor = true;
                } else {
                    saw_below_plain_text_floor = true;
                }
            }
            BlockAction::NoCompressionApplied { .. } => saw_no_compression_applied = true,
            BlockAction::Excluded { .. } => saw_excluded = true,
            BlockAction::Compressed { .. } => {}
        }
    }

    if saw_compressor_error {
        "compressor_error"
    } else if saw_rejected_not_smaller {
        "rejected_not_smaller"
    } else if saw_below_output_floor {
        "below_output_floor"
    } else if saw_below_plain_text_floor {
        "below_plain_text_floor"
    } else if saw_excluded {
        "excluded_live_zone"
    } else if saw_no_compression_applied {
        "no_compressible_content"
    } else {
        "no_change"
    }
}

/// Outcome of dispatching the live zone.
#[derive(Debug)]
pub enum LiveZoneOutcome {
    /// No bytes were rewritten. The caller must forward the original
    /// buffered request body byte-for-byte.
    NoChange { manifest: CompressionManifest },
    /// The dispatcher rewrote at least one block and emitted a fresh
    /// body. The caller forwards `new_body` upstream.
    Modified {
        new_body: Box<RawValue>,
        manifest: CompressionManifest,
    },
}

/// Dispatcher errors. Every variant is recoverable by the caller —
/// the proxy turns each into a structured warn-level log and
/// falls back to forwarding the original bytes.
#[derive(Debug, Error)]
pub enum LiveZoneError {
    /// The request body is not valid JSON.
    #[error("request body is not valid JSON: {0}")]
    BodyNotJson(serde_json::Error),
    /// `messages` field is missing or not a JSON array.
    #[error("body has no `messages` array")]
    NoMessagesArray,
}
