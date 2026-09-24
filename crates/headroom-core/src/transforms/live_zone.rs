//! Live-zone block dispatcher — Phase B.
//!
//! # The mental model
//!
//! After Phase B PR-B1 retired the message-dropping machinery, all
//! compression happens *within* messages, never *between* them. The
//! live-zone dispatcher walks the request body and identifies the
//! *live zone*: the blocks the model will emit a response *against*,
//! which are the only ones whose bytes can mutate without busting the
//! provider's prompt cache.
//!
//! # Provider scope
//!
//! Phase B ships ONE dispatcher entry point —
//! [`compress_anthropic_live_zone`] — that handles the Anthropic
//! Messages API shape (`/v1/messages`). Other providers (OpenAI
//! Chat Completions, OpenAI Responses, Google Gemini, Bedrock with
//! native payloads, …) need their own dispatchers because their
//! request shapes diverge in load-bearing ways:
//!
//! - OpenAI Chat Completions puts tool results in their own
//!   `role: "tool"` messages, not nested in user messages.
//! - OpenAI Responses uses `input` (not `messages`) with item types
//!   like `function_call_output` and `reasoning`.
//! - Gemini uses `contents`/`parts`/`function_response`.
//!
//! Phase C (`docs/notes/realignment/05-phase-C-rust-proxy.md`) introduces
//! `compress_openai_chat_live_zone`, `compress_openai_responses_live_zone`,
//! and friends. They share this module's provider-agnostic types
//! ([`LiveZoneOutcome`], [`BlockAction`], [`CompressionManifest`])
//! and the per-content-type compressor backend, but each owns its
//! own walker.
//!
//! For Anthropic `/v1/messages`, the live zone is bounded by:
//!
//! - **Floor:** `frozen_message_count` (computed by
//!   [`crate::compute_frozen_count`] from explicit `cache_control`
//!   markers; passed in here). Indices below the floor are in the
//!   prompt cache and MUST be byte-identical.
//! - **Ceiling:** the latest user message. The latest assistant
//!   message (if any) is part of the cache hot zone too — it's what
//!   the next response continues from. We never touch it.
//! - **Inside the latest user message:** every block is a candidate.
//!   The most common compressible block type is `tool_result`
//!   (because tool outputs dominate token budgets); `text` blocks
//!   are also eligible (e.g. user pastes a long log).
//!
//! # Phase B build-up
//!
//! - **PR-B2** shipped the dispatcher *skeleton*: identify live-zone
//!   blocks, route to no-op compressors, always return `NoChange`.
//! - **PR-B3** (this PR) wires per-content-type compressors:
//!   `JsonArray` → SmartCrusher; `BuildOutput` → LogCompressor;
//!   `SearchResults` → SearchCompressor; `GitDiff` → DiffCompressor;
//!   `SourceCode` → CodeCompressor; `PlainText` → Kompress (cache-only ML
//!   model, passthrough when not cached); `Html` → no-op (no compressor).
//! - **PR-B4** adds the tokenizer-validation gate (per-block
//!   `compressed.tokens >= original.tokens` → fall back) and the
//!   per-content-type byte threshold below which compression is
//!   skipped.
//! - **PR-B7** wires CCR retrieval-marker injection.
//!
//! # Cache safety invariant
//!
//! Bytes outside the live zone are NEVER touched. PR-B3 writes new
//! bodies via **byte-range surgery**: we locate each rewritten block
//! by pointer arithmetic on `serde_json::value::RawValue` borrowed
//! slices (which retain their offset into the original buffer), then
//! splice the replacement into the output. Concretely:
//!
//! ```text
//!     out = body[..block_start] || replacement || body[block_end..]
//! ```
//!
//! The bytes outside the rewritten ranges are *literally copied*
//! from the input, never re-serialized. This is how we guarantee
//! the SHA-256 of the prefix and suffix are byte-identical to the
//! input — Phase A's fixtures and B3's `byte_fidelity_outside_compressed_block`
//! test pin this in CI.
//!
//! Why byte-range surgery and not "deserialize → mutate → serialize"?
//! Re-serializing a JSON `Value` does not preserve original
//! whitespace, key order subtleties, or numeric formatting that the
//! provider may have already cached against. Byte-faithful copy of
//! everything we don't touch is the only way to guarantee
//! cache stability — see `project_compression_realignment_2026_05`.
//!
//! # AuthMode
//!
//! The `AuthMode` parameter is taken in B3 but unused — Phase F
//! PR-F2 wires the gate (PAYG/OAuth/Subscription each demand
//! different policies; see project memory
//! `project_auth_mode_compression_nuances.md`). Keeping the
//! parameter in the signature now means later PRs are pure
//! implementation swaps, not signature redesigns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;
use thiserror::Error;

use super::code_compressor::{CodeAwareCompressor, CodeCompressorConfig};
use super::content_detector::{detect_content_type, ContentType};
use super::content_router::detect_content_native;
use super::diff_compressor::{DiffCompressor, DiffCompressorConfig};
#[cfg(feature = "ml")]
use super::kompress::{Kompress, KompressConfig};
use super::log_compressor::{LogCompressor, LogCompressorConfig};
use super::read_protection::{
    is_read_command, read_output_should_be_protected, read_protection_enabled,
    tool_call_command_text,
};
use super::search_compressor::{SearchCompressor, SearchCompressorConfig};
use super::smart_crusher::{SmartCrusher, SmartCrusherConfig};
use crate::ccr::{compute_key, marker_for, CcrStore};
use crate::tokenizer::get_tokenizer;
use crate::tool_exclusion::{
    is_byte_exact_excluded, is_ccr_retrieve_tool, is_tool_excluded, is_verbatim_excluded,
};

// ─── Tunable constants (no magic numbers in the dispatch logic) ────────

/// Strategy tag emitted when SmartCrusher rewrote a JSON-array block.
const STRATEGY_SMART_CRUSHER: &str = "smart_crusher";
/// Strategy tag emitted when LogCompressor rewrote a build-output / log block.
const STRATEGY_LOG_COMPRESSOR: &str = "log_compressor";
/// Strategy tag emitted when SearchCompressor rewrote a grep / ripgrep block.
const STRATEGY_SEARCH_COMPRESSOR: &str = "search_compressor";
/// Strategy tag emitted when DiffCompressor rewrote a unified-diff block.
const STRATEGY_DIFF_COMPRESSOR: &str = "diff_compressor";
/// Strategy tag emitted when CodeCompressor rewrote a source-code block.
const STRATEGY_CODE_COMPRESSOR: &str = "code_aware_compressor";
/// Strategy tag emitted when Kompress rewrote a plain-text block.
const STRATEGY_KOMPRESS: &str = "kompress";
/// Tier 1 of Python's `config_compressor`: the self-verified reversible fold.
/// The comment-elision tier (which mints a CCR marker) and the schema fold are
/// NOT ported — `headroom/transforms/config_compressor.py` still owns those.
const STRATEGY_CONFIG_LOSSLESS: &str = "config_lossless";
/// Strategy tag emitted when a block belonging to an `--exclude-tools` tool was
/// folded losslessly instead of compressed. Distinct from
/// [`STRATEGY_CONFIG_LOSSLESS`] so the manifest shows *why* the cheaper fold
/// ran.
const STRATEGY_EXCLUDED_TOOL_LOSSLESS: &str = "excluded_tool_lossless";

/// Marker prefix written by ctx-offload when it replaces a `tool_result`
/// block with a digest. Offload runs before this pass, so a block carrying
/// this has already been shrunk and its original stored elsewhere.
///
/// Defined here rather than in the proxy crate because this pass is the one
/// that has to *recognise* it, and a marker the writer and the reader spell
/// differently is a silent failure — the guard would simply stop matching.
/// `compression::ctx_offload` imports this constant instead of its own copy.
pub const CTX_OFFLOAD_MARKER_PREFIX: &str = "<<ctx:";

/// Empty query context passed to compressors that take a relevance
/// query string. PR-B3 dispatcher does not yet plumb the user's last
/// prompt through; PR-F3 will.
const EMPTY_QUERY: &str = "";
/// Default relevance bias passed to scoring-aware compressors.
///
/// This multiplies the knee index that `adaptive_sizer::compute_optimal_k`
/// found, so 1.0 is "keep what the knee said" — matching Python's `moderate`
/// profile, whose scale runs 0.7 aggressive to 1.5 conservative.
///
/// It was 0.0, described as "no bias". Zero is not the neutral value of a
/// multiplier: `knee * 0.0` is 0, so `k` fell back to `min_k` every time and
/// adaptive sizing never chose anything. Every array big enough to reach the
/// knee path was cut to the floor of 3-5 items no matter how much distinct
/// content it held.
const DEFAULT_BIAS: f64 = 1.0;

/// Default model name handed to the tokenizer registry when the proxy
/// could not extract `body["model"]`. Matches the most-common
/// production Claude model — chars-per-token estimator for `claude-*`
/// is calibrated to 3.5 cpt; using a non-Claude model here would
/// silently pick a different estimator density. PR-F3 will plumb the
/// actual model from `body["model"]`; PR-B4 just establishes the
/// signature.
pub const DEFAULT_MODEL: &str = "claude-3-5-sonnet-20241022";

// ─── Per-content-type byte thresholds ──────────────────────────────────
//
// Below these byte sizes the dispatcher does not even attempt
// compression — the per-block overhead (tokenizer count, dispatcher
// bookkeeping, log lines) costs more than the marginal token savings,
// and tiny inputs almost never compress at all.
//
// Sourced from the spec (`docs/notes/realignment/04-phase-B-live-zone.md::PR-B4`).
// Pinned as `const` rather than a hard-coded `match` so the values are
// grep-able and reviewable in one place.

/// JSON-array tool_results below this size route to no-op.
const THRESHOLD_JSON_ARRAY: usize = 512;
/// Build / log output below this size routes to no-op (512 B). Logs
/// are the most repetitive content type so the threshold is the
/// lowest of the bunch.
const THRESHOLD_BUILD_OUTPUT: usize = 512;
/// Search-result blocks below this size route to no-op.
const THRESHOLD_SEARCH_RESULTS: usize = 512;
/// Git-diff blocks below this size route to no-op.
const THRESHOLD_GIT_DIFF: usize = 512;
/// Source-code blocks below this size route to no-op. Pinned
/// for the future Rust code-compressor port — currently unused
/// because `ContentType::SourceCode` short-circuits to no-op above
/// the dispatch (see `dispatch_compressor`).
const THRESHOLD_SOURCE_CODE: usize = 512;
/// Plain-text blocks below this size route to no-op. Pinned
/// for the future Kompress wiring (PR-B7 follow-up); currently unused.
const THRESHOLD_PLAIN_TEXT: usize = 512;
/// HTML blocks have no compressor; threshold matches plain text so
/// when an HTML compressor lands the value is already pinned.
const THRESHOLD_HTML: usize = 512;

/// Map a content type to its byte threshold. Returning `usize` rather
/// than an `Option` because every variant has a sensible default;
/// `Html` is a no-op anyway so the threshold check never fires.
fn threshold_for(content_type: ContentType) -> usize {
    match content_type {
        ContentType::JsonArray => THRESHOLD_JSON_ARRAY,
        ContentType::BuildOutput => THRESHOLD_BUILD_OUTPUT,
        ContentType::SearchResults => THRESHOLD_SEARCH_RESULTS,
        ContentType::GitDiff => THRESHOLD_GIT_DIFF,
        ContentType::SourceCode => THRESHOLD_SOURCE_CODE,
        ContentType::PlainText => THRESHOLD_PLAIN_TEXT,
        ContentType::Html => THRESHOLD_HTML,
        ContentType::Tabular => THRESHOLD_PLAIN_TEXT,
        // Config payloads are prose-shaped enough that the plain-text
        // threshold is the right floor; the `config` lossless fold does the
        // work once the block is big enough to bother with.
        ContentType::StructuredConfig => THRESHOLD_PLAIN_TEXT,
    }
}

/// Block types the live-zone dispatcher considers "in the cache hot
/// zone" even when they appear inside a live-zone message. Listed
/// explicitly (no string-prefix matching) so the cache-safety
/// surface is grep-able.
const HOT_ZONE_BLOCK_TYPES: &[&str] = &[
    "tool_use",
    "thinking",
    "redacted_thinking",
    // Anthropic compaction items — once injected they're sticky to
    // the cache as much as `tool_use` is.
    "compaction",
];

mod anthropic;
mod compressors;
mod dispatch;
mod openai_chat;
mod openai_responses;
mod outcome;
mod planner;
#[cfg(test)]
mod tests;

// Globs cap each item at its own visibility: `pub` items stay public
// API, the rest stay in-crate. Modules with no `pub` item would
// otherwise warn that they re-export nothing.
#[allow(unused_imports)]
pub use self::{
    anthropic::*, compressors::*, dispatch::*, openai_chat::*, openai_responses::*, outcome::*,
    planner::*,
};

#[cfg(test)]
mod live_zone_size_gate_tests;
