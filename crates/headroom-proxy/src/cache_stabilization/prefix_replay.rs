//! Freeze-replay / prefix-cache tracker — byte-identical replay of the
//! previously-forwarded (compressed) prefix so provider prompt caches stay warm.
//!
//! # Why this exists
//!
//! Clients like Claude Code already manage prompt caching (Anthropic: up to 4
//! `cache_control` breakpoints on a growing prefix). When Headroom compresses a
//! message that sits inside the cached prefix it changes the bytes the provider
//! hashed for its cache key — replacing a 90% read discount with a 25% write
//! penalty for the whole suffix. Worse, on the *next* turn the freeze path emits
//! the agent's ORIGINAL bytes for a message the provider had cached in its
//! COMPRESSED form last turn; forwarding original then mismatches the cached
//! prefix and busts it from that point (a real SWE-bench run attributed 100% of
//! cache misses to this `prefix_change`, ~56% of all cache-writes bust-induced).
//!
//! # What it does
//!
//! This is the Rust port of `headroom/cache/prefix_tracker.py`. It records, per
//! session, the exact messages we FORWARDED last turn (their compressed bytes)
//! and, on the next turn, replays them byte-for-byte in place of whatever the
//! compression pipeline just produced for the same leading positions. Only the
//! newly-appended suffix (the "delta") is left as the fresh compressor output.
//! The forwarded prefix therefore stays byte-identical turn-over-turn and the
//! prompt cache keeps hitting.
//!
//! # Design (mirrors the drift detector's per-session store)
//!
//! - [`SessionReplayStore`] holds one [`PrefixReplayTracker`] per session in a
//!   1000-entry LRU (same capacity + `Arc<Mutex<LruCache>>` shape as
//!   [`crate::cache_stabilization::drift_detector::DriftState`]), keyed by the
//!   shared [`derive_session_key`](crate::cache_stabilization::drift_detector::derive_session_key).
//!   It also holds a small LRU of *pending turns* keyed by `request_id` so the
//!   response-side usage observer can feed cache-token counts back into the
//!   tracker once the stream completes (the request→response correlation the
//!   Python handler did inline).
//! - [`overlay_cached_prefix`] is the request-side replay: given this turn's
//!   optimized (compressed) messages plus the previous turn's original and
//!   forwarded messages, it replays the previously-forwarded prefix byte-identical
//!   when this turn append-only-extends the previous one, else returns the
//!   optimized messages unchanged (accept a possible bust over forwarding wrong
//!   content).
//! - [`extract_cache_stable_delta`] is the cache-mode sibling: returns
//!   `(previously_forwarded_prefix, appended_delta)` so the caller can compress
//!   ONLY the delta.
//! - [`normalize_message_cache_control`] keeps message-level `cache_control`
//!   breakpoints bounded (Anthropic hard-errors at >4) and stable across the
//!   replay so the overlay itself never busts.
//!
//! # How the three spec commits are honored
//!
//! - **#1850** (freeze must forward the cached/compressed prefix byte-identical):
//!   [`overlay_cached_prefix`] replays `previous_forwarded` verbatim, append-only
//!   guarded and idempotent, in place of the compressor's per-position output.
//! - **#1852** (keep `cache_control` bounded + stable): the append-only guard
//!   runs on **content only** — [`canonicalize_for_prefix_compare`] strips
//!   `cache_control` and other transport noise before comparing — and
//!   [`normalize_message_cache_control`] strips every message-level marker and
//!   re-places exactly one ephemeral breakpoint on the last block so replayed
//!   markers cannot accumulate past Anthropic's limit.
//! - **#1868** (provider-agnostic delta + cc-agnostic prefix comparison): the
//!   comparison key is the shared [`canonicalize_for_prefix_compare`] projection
//!   — representation-agnostic (string↔block content sugar), transport/annotation
//!   agnostic (`_NON_SEMANTIC_KEYS`), and opaque over user tool payloads
//!   (`_OPAQUE_PAYLOAD_KEYS`) — never a source to rebuild forwarded bytes.
//!
//! # Interaction with the drift detector and the J4 offload gate
//!
//! A drift/rebuild boundary (the drift detector saw the cache hot zone change)
//! means the prefix the provider had cached is gone, so the stored
//! previously-forwarded prefix is stale. [`PrefixReplayTracker::invalidate`] is
//! called on a rebuild boundary to drop the stored prefix; the next turn then
//! starts a fresh replay chain. This is deliberately the same boundary the J4
//! offload gate uses to allow frozen-history conversion, so the two stay
//! consistent: on a boundary the prefix is rebuilt, on a steady-state turn it is
//! replayed byte-identical.
//!
//! Gated behind `config.prefix_replay` (default off) so it is a safe rollout.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde_json::Value;

/// Production session capacity — matches the drift detector's 1000.
pub const REPLAY_STORE_CAPACITY: usize = 1000;

/// Pending-turn correlation capacity. In-flight requests awaiting their
/// response usage; bounded well above realistic concurrency.
pub const PENDING_CAPACITY: usize = 4096;

/// Minimum cached tokens before the tracker considers a prefix worth freezing,
/// mirroring `PrefixFreezeConfig.min_cached_tokens` in Python.
const MIN_CACHED_TOKENS: u64 = 1024;

/// Hard ceiling on prefixes kept per session beyond the most recent one.
///
/// Sized against real fan-out, not a guess: a `capture-beta` run puts 8 subagents
/// on one session, and each needs its own prefix or it busts the cache on every
/// turn it takes. 16 leaves room for that plus the parent and a few more
/// without the ceiling ever being the thing that bites. The count alone is not
/// the real bound — see [`MAX_ALTERNATE_MESSAGES`].
const MAX_ALTERNATE_PREFIXES: usize = 16;

/// The bound that actually matters: total messages held across a session's
/// alternate prefixes.
///
/// A count-only cap prices a 20-message subagent the same as a 500-message
/// main conversation, though they differ by more than an order of magnitude in
/// what they cost to hold — and each entry keeps original *and* forwarded
/// arrays, across a 1000-session store. Budgeting messages lets the common
/// case (many short subagent streams) keep all of them while a few very long
/// conversations are held to a couple, which is the right trade in both
/// directions.
///
/// Sized so the count ceiling is what governs the fan-out case rather than
/// this: ten subagents holding 400 messages each fit, which is a long run for
/// a subagent. A budget that trimmed them would silently reintroduce the bust
/// this store exists to prevent, and it would do so on exactly the busiest
/// sessions.
///
/// Counted in messages rather than bytes deliberately: sizing the values would
/// mean serialising them, and this runs while the request is in flight.
const MAX_ALTERNATE_MESSAGES: usize = 4_000;

/// Does `candidate` canonically lead `current`?
///
/// This is exactly the test [`overlay_cached_prefix_reported`] applies before
/// replaying, so selecting a candidate with it cannot widen what gets
/// forwarded: a prefix that passes here is one the overlay would have accepted
/// anyway. Content only — the shared canonicalizer strips `cache_control` and
/// the rest of the per-turn transport churn.
fn is_canonical_prefix(candidate: &[Value], current: &[Value]) -> bool {
    if candidate.is_empty() || current.len() < candidate.len() {
        return false;
    }
    canonicalize_slice(&current[..candidate.len()]) == canonicalize_slice(candidate)
}

/// Proactive expansion is deliberately a cache *tail*. When it becomes the
/// cache-control target, its first appearance makes Anthropic write the entire
/// segment we were trying to preserve. Keep the marker on the preceding block
/// instead, leaving the one-time expansion outside the cached prefix.
const PROACTIVE_EXPANSION_OPEN_TAG: &str = "<headroom_proactive_expansion>";

fn is_proactive_expansion_text(text: &str) -> bool {
    text.contains(PROACTIVE_EXPANSION_OPEN_TAG)
}

fn is_proactive_expansion_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_proactive_expansion_text)
}

/// A `<system-reminder>` the client attaches to the newest message and withdraws
/// on the following turn.
///
/// Same shape of problem as a proactive expansion, arrived at from the other
/// end. Measured 2026-08-09: the client hangs one of these off a `tool_result`
/// for exactly one turn, so the provider caches a prefix ending in a block that
/// will not be there next time. The prefix then breaks at that message and
/// everything after it is re-written — 95 turns, 4,353,443 tokens, an average of
/// 45,826 re-written for a few hundred bytes of reminder, 19% of the day's
/// input bill.
///
/// Nothing here removes or moves the block: the model still sees it, in place,
/// on the turn it arrives. It is only kept out of the *cached* region, which is
/// where its disappearance does the damage.
const SYSTEM_REMINDER_OPEN_TAG: &str = "<system-reminder>";

fn is_ephemeral_client_text(text: &str) -> bool {
    text.trim_start().starts_with(SYSTEM_REMINDER_OPEN_TAG)
}

fn is_ephemeral_client_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_ephemeral_client_text)
}

/// Anthropic refuses `cache_control` on a `thinking` block outright —
/// `messages.N.content.0.thinking.cache_control: Extra inputs are not
/// permitted`, a 400 for the whole turn. Extended thinking makes assistant
/// messages whose only block is a thinking block, so "the last block of the
/// message" lands on one often enough to refuse 8% of turns (measured
/// 2026-08-12). Such a block is not a legal target; the placement pass falls
/// back to an earlier block, or to an earlier message when there is none.
fn is_thinking_block(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking") | Some("redacted_thinking")
    )
}

/// Keys that carry NO semantic payload for the model — transport / caching-
/// directive / telemetry / client-routing annotations that clients attach and
/// vary turn-to-turn. Dropped from the cross-turn prefix-equality key ONLY,
/// never from the bytes we forward. Ported verbatim from Python
/// `_NON_SEMANTIC_KEYS`.
const NON_SEMANTIC_KEYS: &[&str] = &[
    // cache-breakpoint markers (moved to the newest block every turn)
    "cache_control", // Anthropic (per-block)
    "cachePoint",    // Bedrock (per-block content block)
    // litellm unified-message / tool annotations
    "caller",
    "provider_specific_fields",
    "reasoning_content",
    "reasoning_items",
    "annotations",
    // OpenAI response echoes that can ride on assistant messages
    "system_fingerprint",
    "service_tier",
    // Vercel AI SDK / opencode part transport
    "providerMetadata",
    "providerOptions",
    "callProviderMetadata",
    "state",
    "providerExecuted",
    "synthetic",
    "ignored",
    // streaming-assembly artifact
    "index",
];

/// Values under these keys are opaque semantic payloads (tool-call input,
/// OpenAI stringified arguments, Bedrock tool_result json). Compared VERBATIM —
/// we never recurse into them to strip "noise" keys, because arbitrary user
/// data there may legitimately contain keys that collide with
/// `NON_SEMANTIC_KEYS` (e.g. an `input` of `{"state": "CA", "index": 3}`).
const OPAQUE_PAYLOAD_KEYS: &[&str] = &["input", "arguments", "json"];

fn is_non_semantic(key: &str) -> bool {
    NON_SEMANTIC_KEYS.contains(&key)
}

fn is_opaque_payload(key: &str) -> bool {
    OPAQUE_PAYLOAD_KEYS.contains(&key)
}

/// Representation-agnostic canonical form for cross-turn prefix equality.
///
/// Providers accept several *equivalent* encodings for the same message and real
/// clients vary them turn-to-turn; a raw compare then fails spuriously and drops
/// cache mode to raw (uncompressed) forwarding. This normalizes ONLY
/// representation:
///   * drops non-semantic annotation / cache-directive / telemetry keys
///     ([`NON_SEMANTIC_KEYS`]) at any message/block level;
///   * wraps a bare string `content` into `[{"type":"text","text":...}]`
///     (Anthropic's string sugar, which litellm flips per turn);
///   * leaves tool `input` / `arguments` / `json` payloads verbatim
///     ([`OPAQUE_PAYLOAD_KEYS`]) so user data is never corrupted;
///   * KEEPS all real content (text, tool name/input, tool_result content,
///     reasoning signatures, ids) so two messages canonicalize-equal iff they
///     are semantically identical.
///
/// Used only as a comparison/storage-boundary key; the original, unmodified
/// messages are always what gets forwarded.
pub fn canonicalize_for_prefix_compare(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, val) in map {
                if is_non_semantic(key) {
                    continue;
                }
                if is_opaque_payload(key) {
                    out.insert(key.clone(), val.clone()); // verbatim — do not recurse
                } else if key == "content" && val.is_string() {
                    // Anthropic string sugar → canonical block form.
                    let text = val.as_str().unwrap_or_default();
                    out.insert(
                        key.clone(),
                        Value::Array(vec![serde_json::json!({"type": "text", "text": text})]),
                    );
                } else {
                    out.insert(key.clone(), canonicalize_for_prefix_compare(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            // Drop blocks that projected to {} — a pure cache-directive content
            // block (e.g. Bedrock {"cachePoint": {...}}) whose only key was
            // non-semantic. Left in place it would be an empty-dict entry, so a
            // directive block moving position across turns would spuriously fail
            // the length/order compare.
            let empty = Value::Object(serde_json::Map::new());
            Value::Array(
                items
                    .iter()
                    .map(canonicalize_for_prefix_compare)
                    .filter(|v| *v != empty)
                    // And drop the client's ephemeral scaffolding, for the same
                    // reason `cache_control` is dropped: it rides on a message
                    // for a turn or two and then leaves, and it is not what
                    // makes one message different from another.
                    //
                    // Without this, a turn that merely lost a
                    // `<system-reminder>` fails the append-only guard, forwards
                    // fresh bytes over a live cache, and — because the same
                    // comparison decides chain identity — is recorded as
                    // continuing nothing, so the churn disguises itself as a
                    // branch. Measured 2026-08-09: every large-write decline in
                    // the sample reported `chain_id = 0`, and three of four
                    // involved a reminder.
                    //
                    // Safe only because the forwarded bytes are stripped to
                    // match (see `relocate_ephemeral_blocks`). Ignoring a
                    // difference here while still forwarding it would replay
                    // bytes the provider never cached.
                    .filter(|v| !is_ephemeral_client_block(v))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// Canonicalize a whole slice of messages (helper for slice comparisons).
fn canonicalize_slice(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(canonicalize_for_prefix_compare)
        .collect()
}

/// Does a message have no content after applying the append-only guard's exact
/// canonicalization?
///
/// This deliberately reuses [`canonicalize_for_prefix_compare`] instead of
/// naming reminder or cache-directive shapes here. A trailing message that
/// projects to empty content is outside the provider-cached prefix, and keeping
/// it in replay state makes its replacement on the next turn look like a real
/// edit. Using the same projection as the guard makes the storage boundary and
/// replay predicate agree by construction.
fn has_empty_canonical_content(message: &Value) -> bool {
    canonicalize_for_prefix_compare(message)
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
}

/// Length of the replayable stored prefix after removing trailing messages
/// whose canonical content is empty.
fn replayable_stored_prefix_len(original_messages: &[Value]) -> usize {
    original_messages
        .iter()
        .rposition(|message| !has_empty_canonical_content(message))
        .map_or(0, |index| index + 1)
}

/// Return `(stable_forwarded_prefix, appended_delta_messages)` when the current
/// request append-only-extends the previous one, else `None`.
///
/// Provider-agnostic delta engine for cache mode (Python
/// `extract_cache_stable_delta`). "Append-only" is decided by comparing the
/// *canonicalized* prefix, so a moved cache marker or shape churn does not
/// spuriously collapse cache mode to raw forwarding. On a match the caller
/// replays the byte-identical previously-forwarded prefix and compresses ONLY
/// the appended delta.
///
/// This is a COMPARISON + slice only: the returned prefix is the
/// previously-forwarded bytes verbatim and the delta is the raw appended
/// messages — never a rebuild from the canonical projection.
pub fn extract_cache_stable_delta(
    current_messages: &[Value],
    previous_original_messages: Option<&[Value]>,
    previous_forwarded_messages: Option<&[Value]>,
) -> Option<(Vec<Value>, Vec<Value>)> {
    let prev_orig = previous_original_messages?;
    let prev_fwd = previous_forwarded_messages?;
    if prev_orig.is_empty() {
        return None;
    }
    let prefix_len = prev_orig.len();
    if current_messages.len() < prefix_len {
        return None;
    }
    if canonicalize_slice(&current_messages[..prefix_len]) != canonicalize_slice(prev_orig) {
        return None;
    }
    Some((prev_fwd.to_vec(), current_messages[prefix_len..].to_vec()))
}

/// Replay the previously-forwarded (cached, compressed) prefix byte-identical.
///
/// Provider-agnostic cache-safety guard for the freeze path (Python
/// `overlay_cached_prefix`). When a message is "frozen" the compression pipeline
/// may emit the agent's ORIGINAL bytes for it — but the provider cached whatever
/// we FORWARDED last turn (the compressed form). Forwarding the original then
/// mismatches the cached prefix and busts the prompt cache from that point. This
/// overlays the exact previously-forwarded prefix onto the corresponding leading
/// messages so the forwarded prefix stays byte-for-byte what the provider hashed.
///
/// Safe only when this turn append-only-extends the previous turn: the previous
/// ORIGINAL messages must be a canonical prefix of the current ORIGINAL messages
/// and there must be exactly one forwarded message per original. Otherwise we
/// return `optimized_messages` unchanged (accept a possible bust over forwarding
/// wrong content).
pub fn overlay_cached_prefix(
    optimized_messages: Vec<Value>,
    current_original_messages: &[Value],
    previous_original_messages: Option<&[Value]>,
    previous_forwarded_messages: Option<&[Value]>,
) -> Vec<Value> {
    overlay_cached_prefix_reported(
        optimized_messages,
        current_original_messages,
        previous_original_messages,
        previous_forwarded_messages,
    )
    .0
}

/// First structural path at which two canonicalized values differ.
///
/// Keys and array indices only — **never a value**. The point is to name the
/// field that churns (`content[0].text`, `content[2].source.data`) so a false
/// divergence can be told from a real edit in a single sample instead of a
/// distribution, and tool-result content must not reach the log to do it.
///
/// Returns `None` when the values agree.
pub fn first_structural_difference(a: &Value, b: &Value) -> Option<String> {
    fn walk(a: &Value, b: &Value, path: &mut String) -> bool {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                // A key present on one side and not the other is itself the
                // difference, and its name is the useful part.
                for k in x.keys().chain(y.keys()) {
                    match (x.get(k), y.get(k)) {
                        (Some(av), Some(bv)) => {
                            let mark = path.len();
                            path.push('.');
                            path.push_str(k);
                            if walk(av, bv, path) {
                                return true;
                            }
                            path.truncate(mark);
                        }
                        _ => {
                            path.push('.');
                            path.push_str(k);
                            return true;
                        }
                    }
                }
                false
            }
            (Value::Array(x), Value::Array(y)) => {
                if x.len() != y.len() {
                    path.push_str(&format!("[len {} vs {}]", x.len(), y.len()));
                    return true;
                }
                for (i, (av, bv)) in x.iter().zip(y).enumerate() {
                    let mark = path.len();
                    path.push_str(&format!("[{i}]"));
                    if walk(av, bv, path) {
                        return true;
                    }
                    path.truncate(mark);
                }
                false
            }
            _ => a != b,
        }
    }
    let mut path = String::new();
    if walk(a, b, &mut path) {
        Some(path.trim_start_matches('.').to_string())
    } else {
        None
    }
}

/// Name the field that made two leading messages disagree.
///
/// Computed by the caller only when logging a
/// [`ReplaySkip::PrefixContentDiverged`], so a turn that replays cleanly never
/// pays for it. Structure only — see [`first_structural_difference`].
pub fn describe_divergence(
    previous_originals: &[Value],
    current_originals: &[Value],
    index: usize,
) -> Option<String> {
    let prev = previous_originals.get(index)?;
    let cur = current_originals.get(index)?;
    first_structural_difference(
        &canonicalize_for_prefix_compare(prev),
        &canonicalize_for_prefix_compare(cur),
    )
}

/// What kind of `text` blocks a message carries, from a closed vocabulary.
///
/// `block_type_shape` says a `text` block appeared or vanished; it cannot say
/// what the block was, and that is the whole question when the same index
/// churns every turn. A client's ephemeral scaffolding and a real edit look
/// identical as `text`.
///
/// The vocabulary is fixed — `system-reminder`, `other-tag`, `plain` — so
/// nothing user-controlled reaches the log. Reporting the actual tag name would
/// break that, because a tag is as attacker- and user-controlled as the body.
pub fn text_block_kinds(message: &Value) -> String {
    let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .map(|b| {
            let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let trimmed = text.trim_start();
            if trimmed.starts_with("<system-reminder>") {
                "system-reminder"
            } else if trimmed.starts_with('<') {
                "other-tag"
            } else {
                "plain"
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The sequence of content-block `type` values in a message, e.g.
/// `"tool_result,text"`.
///
/// Types only — never the blocks' contents. A divergence reported as
/// `content[len 2 vs 1]` says a block vanished but not which kind, and that is
/// the difference between a tool result being collapsed by something in front
/// of the client and an ordinary message being edited. Safe to log for the
/// same reason [`first_structural_difference`] is: the vocabulary is a fixed
/// set of API type names, not user data.
pub fn block_type_shape(message: &Value) -> String {
    match message.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| b.get("type").and_then(Value::as_str).unwrap_or("?"))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::String(_)) => "string".to_string(),
        _ => String::new(),
    }
}

/// Why the store had no prefix to hand back for a session.
///
/// Splits the commonest replay-decline reason into causes that need different
/// responses: a first turn costs nothing, an idle gap past the TTL is already
/// lost (the provider's cache expires in 5 minutes, this store holds 10), and a
/// session that should have had a tracker but did not is a real defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixMiss {
    /// No tracker under this session key. Either the session's first turn, an
    /// LRU eviction, or — the case worth chasing — a session key that is not
    /// stable turn to turn.
    NoTrackerForSession,
    /// The tracker existed but had not been touched inside the session TTL.
    /// The provider's own cache expired long before this did, so the following
    /// re-cache is a TTL expiry rather than anything the proxy caused.
    IdlePastTtl,
    /// A tracker exists but no turn has completed on it yet, so there are no
    /// forwarded bytes to replay.
    NothingForwardedYet,
    /// The tracker mutex was poisoned by a panicking task.
    LockPoisoned,
}

impl PrefixMiss {
    /// Stable label for logs and dashboards.
    pub fn as_str(self) -> &'static str {
        match self {
            PrefixMiss::NoTrackerForSession => "no_tracker_for_session",
            PrefixMiss::IdlePastTtl => "idle_past_ttl",
            PrefixMiss::NothingForwardedYet => "nothing_forwarded_yet",
            PrefixMiss::LockPoisoned => "lock_poisoned",
        }
    }
}

/// Why a turn forwarded its own bytes instead of replaying the cached prefix.
///
/// The caller used to record only whether the overlay changed anything, which
/// collapses five very different situations into one boolean. That matters
/// because turns without a replay are where the money goes: measured over the
/// 2026-08-08/09 logs, requests that did not replay were 19% of traffic and
/// carried 97% of all booked re-cache waste — roughly 80,500 wasted tokens per
/// event against 2,300 for turns that did replay. Naming the reason is what
/// makes that 19% addressable instead of merely visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySkip {
    /// Nothing stored for this session yet: its first turn, a TTL expiry, or an
    /// eviction. Benign on a first turn and expensive on the others.
    NoPreviousTurn,
    /// The stored turn had a different number of forwarded and original
    /// messages, so the prefix cannot be mapped one-to-one.
    ForwardedCountMismatch,
    /// This turn is **shorter** than the stored prefix. A conversation only
    /// grows, so this is the fingerprint of two interleaved streams sharing one
    /// session key — the store holds one prefix per session, so the longer
    /// stream's prefix blocks the shorter one and vice versa (see item 11).
    ShorterThanStoredPrefix,
    /// The optimized output is shorter than the stored prefix — the pipeline
    /// dropped messages the prefix covers.
    OptimizedShorterThanPrefix,
    /// The leading messages changed under canonicalization, so replaying the
    /// stored bytes would forward content the client did not send.
    ///
    /// Refusing is correct. That was doubted for a while — one conversation
    /// declined on 38 consecutive turns while growing two messages at a time,
    /// which looked like churn the canonicalizer had failed to neutralise —
    /// but capture settled it on 2026-08-11. Of 71 events where the current
    /// message carried `thinking` blocks the stored one lacked, 64 compared a
    /// user-role `tool_result` against an assistant `thinking,tool_use` at the
    /// same index. Roles alternate, so the list had shifted: the client
    /// deletes whole messages from mid-history. Growth of two per turn hides
    /// it, because a deletion in the middle and two arrivals at the end net
    /// out to the same count.
    ///
    /// So the stored prefix holds a message the client removed, and replaying
    /// it to save the cache would re-send content the client threw out. There
    /// is nothing to win either way: measured over 311 diverged turns, median
    /// creation falls 55,524 -> 28,153 -> 18,009 as the edit moves from the
    /// first quarter of the prefix to the third, with no replay involved. The
    /// provider already reads up to the deletion and rebuilds only what
    /// follows.
    ///
    /// Carries the first message index that differs, as a diagnostic only.
    /// Replaying up to it was tried and measured worse.
    PrefixContentDiverged { first_diff_index: usize },
}

impl ReplaySkip {
    /// Stable label for logs and dashboards.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplaySkip::NoPreviousTurn => "no_previous_turn",
            ReplaySkip::ForwardedCountMismatch => "forwarded_count_mismatch",
            ReplaySkip::ShorterThanStoredPrefix => "shorter_than_stored_prefix",
            ReplaySkip::OptimizedShorterThanPrefix => "optimized_shorter_than_prefix",
            ReplaySkip::PrefixContentDiverged { .. } => "prefix_content_diverged",
        }
    }
}

/// [`overlay_cached_prefix`], but reporting why it declined.
///
/// Returns `(messages, None)` when the cached prefix was replayed, and
/// `(messages_unchanged, Some(reason))` otherwise. It is all-or-nothing on
/// purpose — see the comment on the divergence path.
pub fn overlay_cached_prefix_reported(
    optimized_messages: Vec<Value>,
    current_original_messages: &[Value],
    previous_original_messages: Option<&[Value]>,
    previous_forwarded_messages: Option<&[Value]>,
) -> (Vec<Value>, Option<ReplaySkip>) {
    let (prev_orig, prev_fwd) = match (previous_original_messages, previous_forwarded_messages) {
        (Some(o), Some(f)) if !o.is_empty() && !f.is_empty() => (o, f),
        _ => return (optimized_messages, Some(ReplaySkip::NoPreviousTurn)),
    };
    let n = prev_orig.len();
    // One forwarded message per original, and the frozen prefix must fit within
    // both the current originals and this turn's optimized output.
    if prev_fwd.len() != n {
        return (optimized_messages, Some(ReplaySkip::ForwardedCountMismatch));
    }
    if current_original_messages.len() < n {
        return (
            optimized_messages,
            Some(ReplaySkip::ShorterThanStoredPrefix),
        );
    }
    if optimized_messages.len() < n {
        return (
            optimized_messages,
            Some(ReplaySkip::OptimizedShorterThanPrefix),
        );
    }
    // Append-only guard on CONTENT ONLY (#1852): compare with the shared
    // canonicalizer so the guard is robust to ALL per-turn transport /
    // annotation churn — cache_control movement, litellm `caller`, streaming
    // `index`, string↔block content shape, etc.
    if canonicalize_slice(&current_original_messages[..n]) != canonicalize_slice(prev_orig) {
        // Locate the first disagreement. The whole-slice compare above already
        // told us there is one; this only runs on the decline path, so the
        // per-message walk never touches a turn that replays cleanly.
        let first_diff_index = (0..n)
            .find(|&i| {
                canonicalize_for_prefix_compare(&current_original_messages[i])
                    != canonicalize_for_prefix_compare(&prev_orig[i])
            })
            .unwrap_or(n);
        // Forward this turn's own bytes for the whole prefix.
        //
        // Replaying the leading run that still agrees looks obviously better
        // and was tried on 2026-08-09. It is not. Measured live on one
        // conversation, `prev_fwd[..k] ++ optimized[k..]` collapsed a turn from
        // 224,689 cache-read / 981 cache-write to 22,026 read / 204,768 write,
        // and the following turn recovered on its own.
        //
        // The premise was wrong. A declined replay is not a bust: compression
        // is deterministic, so this turn's own bytes for an unchanged prefix
        // reproduce the bytes the provider already cached. The same
        // conversation shows it — a `no_previous_turn` turn, replaying nothing
        // at all, still read 222,975 tokens from cache. Splicing a previous
        // turn's bytes onto this turn's tail creates a seam that matches
        // neither, which is the one way to actually lose the prefix.
        return (
            optimized_messages,
            Some(ReplaySkip::PrefixContentDiverged { first_diff_index }),
        );
    }
    // Replay the cached (compressed) prefix; keep this turn's tail.
    let mut out = prefix_without_ephemeral_blocks(prev_fwd);
    out.extend_from_slice(&optimized_messages[n..]);
    (out, None)
}

/// The frozen prefix with client reminders taken out.
///
/// [`relocate_ephemeral_blocks`] moves reminders onto the newest message. One
/// turn later that message is history, and replaying it verbatim puts the
/// reminder back into a conversation the client has already withdrawn it from
/// — while [`canonicalize_for_prefix_compare`] filters reminders, so the
/// append-only guard sees two equal prefixes and replays anyway. Each turn
/// leaves one more behind. Measured live on 2026-08-11, the forwarded body
/// reached 382% of the client body in sixteen turns, with the whole pile
/// sitting past the last breakpoint where it bills fresh at 1.0x.
///
/// Stripping here is what makes the bytes agree with the comparison: history
/// we forward stops carrying reminders in either direction, so the prefix is
/// the same every turn. Nothing cached is lost — the breakpoint goes on the
/// last non-ephemeral block, so the provider never cached these blocks to
/// begin with.
///
/// Message count is preserved, and a message that is nothing but reminders is
/// left alone: dropping it would trip [`ReplaySkip::ForwardedCountMismatch`]
/// on the next turn, and emptying its content is rejected by the API.
fn prefix_without_ephemeral_blocks(prefix: &[Value]) -> Vec<Value> {
    prefix
        .iter()
        .map(|msg| {
            let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
                return msg.clone();
            };
            if !blocks.iter().any(is_ephemeral_client_block) {
                return msg.clone();
            }
            let keep: Vec<Value> = blocks
                .iter()
                .filter(|b| !is_ephemeral_client_block(b))
                .cloned()
                .collect();
            if keep.is_empty() {
                return msg.clone();
            }
            let mut msg = msg.clone();
            if let Some(obj) = msg.as_object_mut() {
                obj.insert("content".to_string(), Value::Array(keep));
            }
            msg
        })
        .collect()
}

/// Move the client's `<system-reminder>` blocks out of history and onto the
/// newest message, leaving forwarded history independent of them.
///
/// The client hangs these off a message for a turn or two and then withdraws
/// them. While they sit in history they are inside the provider's cached
/// prefix, so their departure kills it from that message on — measured at 32%
/// of declined turns taking a large write.
///
/// Nothing is dropped. Every block the client sent is still sent, in the same
/// request, moved to the end where it sits outside the cached prefix (the
/// breakpoint goes on the last non-ephemeral block). History therefore stops
/// depending on them in either direction: a reminder arriving or leaving cannot
/// change a byte of it.
///
/// This is the counterpart to the filter in
/// [`canonicalize_for_prefix_compare`]. The two must move together — treating
/// reminders as invisible for comparison while still forwarding them in place
/// would replay bytes the provider never cached.
///
/// Declines to act, returning the input untouched, when:
/// - the newest message is not a `user` message with list content, so there is
///   nowhere safe to put them;
/// - stripping would empty a message's content, which the API rejects.
pub fn relocate_ephemeral_blocks(messages: Vec<Value>) -> Vec<Value> {
    let Some(last) = messages.len().checked_sub(1) else {
        return messages;
    };
    let tail_is_open = messages[last].get("role").and_then(Value::as_str) == Some("user")
        && messages[last]
            .get("content")
            .map(Value::is_array)
            .unwrap_or(false);
    if !tail_is_open {
        return messages;
    }

    let mut messages = messages;
    let mut collected: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let tail = messages.split_off(last);
    for mut msg in messages {
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            out.push(msg);
            continue;
        };
        if !blocks.iter().any(is_ephemeral_client_block) {
            out.push(msg);
            continue;
        }
        let (ephemeral, keep): (Vec<Value>, Vec<Value>) = blocks
            .iter()
            .cloned()
            .partition(|b| is_ephemeral_client_block(b));
        collected.extend(ephemeral);
        // A message that was nothing but scaffolding leaves with it. Emptying
        // its content instead would be rejected by the API, and keeping the
        // message is what let this churn survive block-level relocation: the
        // client drops the whole message a turn later, every index after it
        // shifts, and the prefix dies there. Dropping it is safe because the
        // client's own next request is that same sequence without it.
        if keep.is_empty() {
            continue;
        }
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(keep));
        }
        out.push(msg);
    }
    out.extend(tail);
    if collected.is_empty() {
        return out;
    }
    if let Some(blocks) = out
        .last_mut()
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
    {
        blocks.extend(collected);
    }
    out
}

/// Own message-level `cache_control` placement so breakpoints stay bounded.
///
/// Two forces pile up markers turn over turn: clients move the breakpoint to the
/// newest message each call, and [`overlay_cached_prefix`] replays the markers
/// that rode on each turn's then-newest message. Anthropic hard-errors at >4
/// `cache_control` blocks total. Fix (Python `normalize_message_cache_control`):
/// strip EVERY message-level marker and re-place a **single** ephemeral
/// breakpoint on the last block of the last block-style message. One breakpoint
/// caches the whole prefix, and — because the provider's cache key is message
/// CONTENT, not marker presence — stripping and re-placing never busts.
///
/// Every non-empty string message is converted to its equivalent one-text-block
/// form so it can carry `cache_control` — not only the one selected, which
/// would leave the shape depending on which turn it is. Empty strings and
/// proactive expansions stay byte-for-byte unchanged. Returns the input
/// unchanged when there is nothing to normalize.
pub fn normalize_message_cache_control(messages: Vec<Value>) -> Vec<Value> {
    place_tail_cache_breakpoints(messages, 1).0
}

/// [`normalize_message_cache_control`] with the number of tail breakpoints
/// chosen by the caller, and a count of how many it managed to place.
///
/// One breakpoint caches everything before it, so a second one further back is
/// a hedge: when the newest message changes — the client withdraws a reminder,
/// or a turn is retried — the older marker still names a prefix the provider
/// holds, and the read starts there instead of at nothing. Measured against
/// Anthropic's published multipliers, two tail slots beat one by roughly 5% of
/// the bill.
///
/// It is not free. Each marker the provider has not seen before is a write at
/// 1.25x, so a second slot pays only while the extra checkpoint is reused. That
/// is why the count comes back: the caller uses it to decide whether it may
/// also drop the client's own `system` breakpoints, which is unsafe with zero
/// message markers placed.
///
/// `tail_slots` is taken as given, `0` included — this function cannot see
/// `system` or `tools`, so it cannot know how many of Anthropic's four marker
/// slots are already spoken for. Ask [`tail_slots_within_budget`] first.
pub fn place_tail_cache_breakpoints(messages: Vec<Value>, tail_slots: usize) -> (Vec<Value>, usize) {
    #[derive(Clone, Copy)]
    struct CacheTarget {
        message_idx: usize,
        block_idx: usize,
    }

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    // The last cacheable target of each message, oldest first. Only the tail of
    // this list is used, but a message qualifies or not as it is walked.
    let mut cacheable_targets: Vec<CacheTarget> = Vec::new();
    // Set once an ephemeral block in the live tail is passed; nothing after it
    // may carry the breakpoint.
    //
    // The live tail is normally the final user message. On tool-use turns the
    // request can end with an assistant message while the transient reminder
    // still hangs off the latest user message. Treating only the literal final
    // message as live cached that reminder; the next request removed it and the
    // provider had to rebuild from there (observed 2026-08-13: 6,434 tokens).
    //
    // Older reminders are not seals: reminders sometimes persist deep in
    // history, and stranding every later message would cost more than the churn
    // this protects against.
    let mut sealed = false;
    let final_message = messages.len().saturating_sub(1);
    let latest_user_message = messages.iter().rposition(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
    });

    for (i, msg) in messages.into_iter().enumerate() {
        let is_block_content = msg.get("content").map(|c| c.is_array()).unwrap_or(false);
        if is_block_content {
            let content = msg.get("content").and_then(|c| c.as_array()).unwrap();
            let had = content
                .iter()
                .any(|b| b.is_object() && b.get("cache_control").is_some());
            if had {
                let stripped: Vec<Value> = content
                    .iter()
                    .map(|b| {
                        if let Value::Object(obj) = b {
                            let mut o = obj.clone();
                            o.remove("cache_control");
                            Value::Object(o)
                        } else {
                            b.clone()
                        }
                    })
                    .collect();
                let mut m = msg.as_object().unwrap().clone();
                m.insert("content".to_string(), Value::Array(stripped));
                out.push(Value::Object(m));
            } else {
                out.push(msg);
            }

            if let Some(blocks) = out
                .last()
                .and_then(|msg| msg.get("content"))
                .and_then(Value::as_array)
            {
                let mut last_in_message: Option<(usize, usize)> = None;
                for (block_idx, block) in blocks.iter().enumerate() {
                    // An ephemeral block seals the cacheable region: the
                    // breakpoint must land strictly BEFORE it, never merely
                    // skip over it. Anthropic caches up to and including the
                    // marked block, so a marker placed after this one would
                    // pull the ephemeral block into the cached prefix — the
                    // exact thing that costs 19% of the bill.
                    if (i == final_message || latest_user_message == Some(i))
                        && is_ephemeral_client_block(block)
                    {
                        sealed = true;
                    }
                    if sealed {
                        continue;
                    }
                    if block.is_object()
                        && !is_proactive_expansion_block(block)
                        && !is_thinking_block(block)
                    {
                        last_in_message = Some((i, block_idx));
                    }
                }
                if let Some(found) = last_in_message {
                    cacheable_targets.push(CacheTarget {
                        message_idx: found.0,
                        block_idx: found.1,
                    });
                }
            }
        } else if let Some(text) = msg.get("content").and_then(Value::as_str).map(str::to_owned) {
            // String sugar has no block on which to put `cache_control`, so an
            // eligible string is converted to its one-text-block equivalent —
            // every eligible message, not only the one selected this turn.
            //
            // Wrapping just the selection would make a message's forwarded
            // shape depend on the turn: block form while it holds the marker,
            // bare string again once the marker moves to a newer message. The
            // provider caches up to and including the marked message, so that
            // revert lands INSIDE the cached prefix and costs all of it. The
            // overlay does replay the wrapped shape, but only on turns it
            // replays at all — this keeps the shape a function of the message's
            // own content, so it is identical every turn either way.
            //
            // Eligibility is content-only for the same reason. Sealing stays
            // positional (a reminder is only withdrawn from the newest
            // message), so a reminder is wrapped like any other string and
            // simply never selected while it is the final message.
            let final_ephemeral = (i == final_message || latest_user_message == Some(i))
                && is_ephemeral_client_text(&text);
            let eligible = !text.trim().is_empty() && !is_proactive_expansion_text(&text);
            if eligible {
                let mut m = msg.as_object().cloned().unwrap_or_default();
                m.insert(
                    "content".to_string(),
                    serde_json::json!([{"type": "text", "text": text}]),
                );
                out.push(Value::Object(m));
            } else {
                out.push(msg);
            }
            if final_ephemeral {
                sealed = true;
            }
            if !sealed && eligible {
                cacheable_targets.push(CacheTarget {
                    message_idx: i,
                    block_idx: 0,
                });
            }
        } else {
            out.push(msg);
        }
    }

    // Re-place the breakpoints on the latest ordinary blocks, newest first. A
    // proactive expansion is a one-time tail and must never become the cache
    // target: doing so converts its first appearance into a cache write.
    let mut placed = 0usize;
    for target in cacheable_targets.iter().rev().take(tail_slots) {
        let CacheTarget {
            message_idx,
            block_idx,
        } = *target;
        if let Some(content) = out[message_idx]
            .get_mut("content")
            .and_then(|c| c.as_array_mut())
        {
            if let Some(Value::Object(block)) = content.get_mut(block_idx) {
                block.insert(
                    "cache_control".to_string(),
                    serde_json::json!({"type": "ephemeral"}),
                );
                placed += 1;
            }
        }
    }
    (out, placed)
}

/// Remove every `cache_control` marker the client put on `system`.
///
/// Claude Code sends two of them, both asking for the 1h TTL, which bills at
/// 2.0x against 1.25x for the 5m one. They buy nothing here: a breakpoint caches
/// the whole prefix before it, and the system prompt sits in front of every
/// message, so the tail marker already covers it. Dropping them also frees two
/// of Anthropic's four marker slots.
///
/// Only call this once a message breakpoint is in place. With none, these are
/// the only markers on the request and removing them turns caching off outright.
///
/// Returns how many it removed; `0` means the field was a plain string, absent,
/// or already clean, and nothing was touched.
/// Anthropic refuses a request carrying more than this many `cache_control`
/// blocks, counted across `system`, `tools` and `messages` together.
pub const ANTHROPIC_CACHE_CONTROL_LIMIT: usize = 4;

/// How many tail breakpoints may be placed without breaking that limit, and how
/// many slots `system` and `tools` have already taken.
///
/// The limit spans three fields and this proxy sets markers in only one of them,
/// so the sum is nobody's business by default and the request is refused whole
/// when it goes over. Claude Code sends 2 on `system`; PR-E3 adds one to
/// `tools` on PAYG. Asking for 2 message slots on top of both is 5.
///
/// Message markers are not counted: [`place_tail_cache_breakpoints`] strips
/// every one it finds before placing its own, so whatever the client put there
/// is gone by the time these are added.
pub fn tail_slots_within_budget(body: &Value, requested: usize) -> (usize, usize) {
    let reserved = count_field_markers(body, "system") + count_field_markers(body, "tools");
    let allowed = ANTHROPIC_CACHE_CONTROL_LIMIT.saturating_sub(reserved);
    (requested.min(allowed), reserved)
}

/// `cache_control` markers on the objects of a top-level array field. `system`
/// blocks and `tools` entries both carry theirs as a direct key, and a `system`
/// sent as a plain string has none.
fn count_field_markers(body: &Value, field: &str) -> usize {
    body.get(field)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("cache_control").is_some())
                .count()
        })
        .unwrap_or(0)
}

pub fn strip_system_cache_control(body: &mut Value) -> usize {
    let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut removed = 0;
    for block in blocks.iter_mut() {
        if let Value::Object(obj) = block {
            if obj.remove("cache_control").is_some() {
                removed += 1;
            }
        }
    }
    removed
}

/// Fingerprints the first few messages exactly as they go on the wire, as
/// `"0:a1b2c3d4,1:...,2:..."`.
///
/// The drift detector cannot answer this question: it filters ephemeral blocks
/// before comparing and the provider does not, so it reports a stable prefix
/// while the provider re-creates one. This hashes the bytes themselves, with
/// nothing removed, so consecutive turns can be diffed offline to name the first
/// message that moved.
pub fn early_message_fingerprints(messages: &[Value], count: usize) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    messages
        .iter()
        .take(count)
        .enumerate()
        .map(|(idx, message)| {
            let mut hasher = DefaultHasher::new();
            message.to_string().hash(&mut hasher);
            format!("{idx}:{:08x}", hasher.finish() as u32)
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Rough per-message token estimate (chars / 3.5), mirroring Python
/// `_estimate_message_tokens`. Counts text, tool_result content, tool_use input
/// (Anthropic) and top-level `tool_calls`/`function_call` (OpenAI).
fn estimate_message_tokens(messages: &[Value]) -> Vec<u64> {
    messages
        .iter()
        .map(|msg| {
            let mut chars: usize = 0;
            match msg.get("content") {
                Some(Value::String(s)) => chars += s.len(),
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                chars += block
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .map_or(0, str::len)
                            }
                            "tool_result" => match block.get("content") {
                                Some(Value::String(s)) => chars += s.len(),
                                Some(Value::Array(inner)) => {
                                    for b in inner {
                                        chars += b
                                            .get("text")
                                            .and_then(|t| t.as_str())
                                            .map_or(0, str::len);
                                    }
                                }
                                _ => {}
                            },
                            "tool_use" => match block.get("input") {
                                Some(Value::String(s)) => chars += s.len(),
                                Some(v @ Value::Object(_)) => {
                                    chars += serde_json::to_string(v).map_or(0, |s| s.len())
                                }
                                _ => {}
                            },
                            _ => {
                                chars += block
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .map_or(0, str::len)
                            }
                        }
                    }
                }
                _ => {}
            }
            // OpenAI function-calling: command lives in top-level `tool_calls`
            // (or legacy `function_call`), not `content`.
            if let Some(Value::Array(tcs)) = msg.get("tool_calls") {
                for tc in tcs {
                    if let Some(fnc) = tc.get("function") {
                        chars += fnc.get("name").and_then(|n| n.as_str()).map_or(0, str::len);
                        chars += fnc
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .map_or(0, str::len);
                    }
                }
            }
            if let Some(fc) = msg.get("function_call") {
                chars += fc.get("name").and_then(|n| n.as_str()).map_or(0, str::len);
                chars += fc
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .map_or(0, str::len);
            }
            chars += 20; // role/structure overhead
            std::cmp::max(1, (chars as f64 / 3.5) as u64)
        })
        .collect()
}

/// Per-session freeze-replay state across turns (Python `PrefixCacheTracker`).
#[derive(Clone, Debug)]
pub struct PrefixReplayTracker {
    cached_token_count: u64,
    cached_message_count: usize,
    turn_number: u64,
    last_activity: Instant,
    last_original_messages: Vec<Value>,
    last_forwarded_messages: Vec<Value>,
    /// Prefixes belonging to OTHER streams interleaved on this session.
    ///
    /// One session key carries several streams — a subagent inheriting its
    /// parent's context, a resumed transcript — which item 11 proved by message
    /// counts that run backwards under a single key. With one slot per session,
    /// stream B's turn is tested against stream A's stored prefix, fails the
    /// append-only guard, and forwards freshly compressed bytes for content the
    /// provider already had cached. That is a bust the proxy causes itself.
    ///
    /// Bounded and ordered most-recent-first. Each carries the id of the chain
    /// it belongs to.
    alternates: Vec<(u64, Vec<Value>, Vec<Value>)>,
    /// Which chain the primary prefix belongs to. 0 before the first turn.
    ///
    /// A *chain* is a run of turns that each continue the previous one. It is
    /// the identity that every other key in this codebase only approximates:
    /// `session_key` is per-client, `conversation_key` hashes `system` plus the
    /// first message, and neither can tell two branches of one conversation
    /// apart. Message counts cannot either — compaction, a retry and a genuine
    /// second stream all make the count stop rising, and on 2026-08-09 three
    /// separate conclusions were drawn from that ambiguity and all three were
    /// wrong (item 25).
    ///
    /// The store already computes the answer to decide what to replay. This
    /// only gives it a name so it can be logged and grouped by.
    primary_chain_id: u64,
    next_chain_id: u64,
}

impl Default for PrefixReplayTracker {
    fn default() -> Self {
        Self {
            cached_token_count: 0,
            cached_message_count: 0,
            turn_number: 0,
            last_activity: Instant::now(),
            last_original_messages: Vec::new(),
            last_forwarded_messages: Vec::new(),
            alternates: Vec::new(),
            primary_chain_id: 0,
            next_chain_id: 1,
        }
    }
}

impl PrefixReplayTracker {
    /// How many leading messages to skip compression on the next turn. Returns 0
    /// on the cold-start turn or when the cached prefix is below the min-token
    /// threshold.
    pub fn frozen_message_count(&self) -> usize {
        if self.turn_number == 0 {
            return 0;
        }
        if self.cached_token_count < MIN_CACHED_TOKENS {
            return 0;
        }
        self.cached_message_count
    }

    /// The previous turn's original (pre-compression) messages.
    pub fn last_original_messages(&self) -> &[Value] {
        &self.last_original_messages
    }

    /// The previous turn's forwarded (post-compression, byte-exact) messages.
    pub fn last_forwarded_messages(&self) -> &[Value] {
        &self.last_forwarded_messages
    }

    /// Record what we forwarded this turn and how many tokens the provider
    /// cached, computing the frozen-message boundary for the next turn (Python
    /// `update_from_response`).
    ///
    /// `original_messages` is this turn's pre-compression input; `forwarded` is
    /// exactly the bytes we sent upstream. When `original_messages` is `None`,
    /// `forwarded` is used for both (parity with Python).
    pub fn update_from_response(
        &mut self,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        forwarded: &[Value],
        original_messages: Option<&[Value]>,
    ) {
        self.last_activity = Instant::now();
        self.turn_number += 1;
        let mut incoming_original = original_messages.unwrap_or(forwarded).to_vec();
        // The last message can consist entirely of client-ephemeral blocks.
        // `place_tail_cache_breakpoints` seals before that message, so the
        // provider never cached it. Keeping it here would nevertheless make a
        // replacement at the same index fail the append-only guard next turn.
        //
        // Derive the boundary from the ORIGINAL slice. Relocation can attach a
        // reminder to a forwarded message whose original counterpart had none,
        // so inspecting the two tails independently can produce different
        // lengths and trip `ForwardedCountMismatch`. Apply the one original
        // index to both stored slices instead.
        let stored_prefix_len = replayable_stored_prefix_len(&incoming_original);
        incoming_original.truncate(stored_prefix_len);
        let mut incoming_forwarded = forwarded.to_vec();
        incoming_forwarded.truncate(stored_prefix_len);
        // If this turn does not continue the prefix we are currently holding,
        // the two belong to different streams sharing this session. Keep the
        // displaced one instead of dropping it, so the stream it belongs to can
        // still replay on its next turn rather than busting.
        if !self.last_forwarded_messages.is_empty()
            && !is_canonical_prefix(&self.last_original_messages, &incoming_original)
        {
            let displaced = (
                self.primary_chain_id,
                std::mem::take(&mut self.last_original_messages),
                std::mem::take(&mut self.last_forwarded_messages),
            );
            self.primary_chain_id = 0;
            self.alternates.retain(|(_, o, _)| o != &displaced.1);
            self.alternates.insert(0, displaced);
            self.alternates.truncate(MAX_ALTERNATE_PREFIXES);
            // Then trim to the message budget, dropping the
            // least-recently-displaced first. A stream that keeps taking turns
            // is promoted back to primary on each one, so what falls off the
            // end is what has actually gone quiet.
            let mut held = 0usize;
            let mut keep = 0usize;
            for (_, orig, _) in &self.alternates {
                let next = held.saturating_add(orig.len());
                if keep > 0 && next > MAX_ALTERNATE_MESSAGES {
                    break;
                }
                held = next;
                keep += 1;
            }
            self.alternates.truncate(keep);
        }
        // Whichever chain this turn continues, it inherits that chain's id;
        // continuing nothing starts a new one. This is the only place a chain
        // is born, so an id names one unbroken run of turns for as long as the
        // tracker lives.
        if self.primary_chain_id == 0 {
            self.primary_chain_id = self
                .alternates
                .iter()
                .find(|(_, o, _)| is_canonical_prefix(o, &incoming_original))
                .map(|(id, _, _)| *id)
                .unwrap_or_else(|| {
                    let id = self.next_chain_id;
                    self.next_chain_id += 1;
                    id
                });
        }
        // This turn continues one of the alternates? Then it is no longer an
        // alternate — it is the live prefix, and holding it twice would let a
        // stale copy win a later match.
        self.alternates
            .retain(|(_, o, _)| !is_canonical_prefix(o, &incoming_original));
        self.last_original_messages = incoming_original;
        self.last_forwarded_messages = incoming_forwarded;

        let total_cached = cache_read_tokens + cache_write_tokens;
        if total_cached == 0 {
            self.cached_token_count = 0;
            self.cached_message_count = 0;
            return;
        }

        // Estimate positions over the complete upstream request, not the
        // replay-state slice. The reported cached total stops accumulation
        // before the uncached ephemeral tail; only storage is truncated above.
        let counts = estimate_message_tokens(forwarded);
        let mut accumulated: u64 = 0;
        let mut frozen_count = 0usize;
        for (i, tok) in counts.iter().enumerate() {
            accumulated += *tok;
            if accumulated <= total_cached {
                frozen_count = i + 1;
            } else {
                break;
            }
        }
        self.cached_token_count = total_cached;
        self.cached_message_count = frozen_count;
    }

    /// Drop the stored prefix. Called on a drift/rebuild boundary: the bytes the
    /// provider cached are gone, so replaying the stored prefix would be stale.
    /// The next turn starts a fresh replay chain.
    pub fn invalidate(&mut self) {
        self.cached_token_count = 0;
        self.cached_message_count = 0;
        self.last_original_messages.clear();
        self.last_forwarded_messages.clear();
        // Alternates are prefixes for the same dead cache; leaving them would
        // let an invalidated stream replay after the boundary that killed it.
        self.alternates.clear();
        // The chain is broken here by definition, so the next turn starts a new
        // one rather than silently extending the id across the boundary.
        self.primary_chain_id = 0;
    }

    pub fn turn_number(&self) -> u64 {
        self.turn_number
    }
}

/// One in-flight turn awaiting its response usage, so the response side can feed
/// cache tokens back into the right session's tracker.
#[derive(Clone, Debug)]
struct PendingTurn {
    session_key: String,
    original_messages: Vec<Value>,
    forwarded_messages: Vec<Value>,
}

/// Per-session freeze-replay store. Cloneable `Arc<Mutex<…>>` handle like
/// [`crate::cache_stabilization::drift_detector::DriftState`].
#[derive(Clone)]
pub struct SessionReplayStore {
    trackers: Arc<Mutex<LruCache<String, PrefixReplayTracker>>>,
    pending: Arc<Mutex<LruCache<String, PendingTurn>>>,
    session_ttl: Duration,
}

impl std::fmt::Debug for SessionReplayStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionReplayStore")
            .field("capacity", &REPLAY_STORE_CAPACITY)
            .finish_non_exhaustive()
    }
}

impl SessionReplayStore {
    /// Build a store bounded to `capacity` sessions. Production uses
    /// [`REPLAY_STORE_CAPACITY`]; tests pass small values.
    ///
    /// # Panics
    /// If `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("SessionReplayStore capacity must be > 0");
        let pending_cap =
            NonZeroUsize::new(PENDING_CAPACITY).expect("PENDING_CAPACITY must be > 0");
        Self {
            trackers: Arc::new(Mutex::new(LruCache::new(cap))),
            pending: Arc::new(Mutex::new(LruCache::new(pending_cap))),
            session_ttl: Duration::from_secs(600),
        }
    }

    /// Snapshot of the previous turn's `(original, forwarded)` messages for a
    /// session, or `None` if there is no live prefix to replay (cold start, or
    /// idle beyond the session TTL). Used to build the overlay inputs.
    pub fn previous_turn(&self, session_key: &str) -> Option<(Vec<Value>, Vec<Value>)> {
        self.previous_turn_detailed(session_key).ok()
    }

    /// The stored prefix that `current_originals` actually continues.
    ///
    /// One session key carries several streams (item 11), so "the last turn on
    /// this session" is the wrong question — it may belong to a different
    /// stream, in which case the append-only guard rejects it and the turn
    /// forwards fresh bytes over content the provider had cached. This asks the
    /// right question instead: of the prefixes held for this session, which one
    /// does this turn extend? Longest match wins, so a stream replays as much
    /// of its own history as it can.
    ///
    /// Falls back to the most recent prefix when nothing matches, leaving the
    /// overlay to decline exactly as before — this can only turn a decline into
    /// a replay, never a replay into a wrong one.
    ///
    /// The third element is the id of the chain this turn continues, or `0`
    /// when it continues none of them (the fallback below). That is the only
    /// trustworthy grouping key the proxy has — see [`PrefixReplayTracker`].
    pub fn previous_turn_for(
        &self,
        session_key: &str,
        current_originals: &[Value],
    ) -> Result<(Vec<Value>, Vec<Value>, u64), PrefixMiss> {
        let mut guard = match self.trackers.lock() {
            Ok(g) => g,
            Err(_) => return Err(PrefixMiss::LockPoisoned),
        };
        let Some(tracker) = guard.get(session_key) else {
            return Err(PrefixMiss::NoTrackerForSession);
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            guard.pop(session_key);
            return Err(PrefixMiss::IdlePastTtl);
        }
        if tracker.last_forwarded_messages.is_empty() && tracker.alternates.is_empty() {
            return Err(PrefixMiss::NothingForwardedYet);
        }
        let best = std::iter::once((
            tracker.primary_chain_id,
            &tracker.last_original_messages,
            &tracker.last_forwarded_messages,
        ))
        .chain(tracker.alternates.iter().map(|(id, o, f)| (*id, o, f)))
        .filter(|(_, o, f)| !f.is_empty() && is_canonical_prefix(o, current_originals))
        .max_by_key(|(_, o, _)| o.len());
        match best {
            Some((chain_id, o, f)) => {
                // Did a stream other than the session's most recent one win?
                // That is this store's whole reason to exist, and the only
                // direct evidence that interleaved streams were costing busts:
                // under one slot per session this turn would have declined and
                // forwarded fresh bytes over cached content.
                let primary_len = tracker.last_original_messages.len();
                let matched_alternate =
                    o.len() != primary_len || o != &tracker.last_original_messages;
                if matched_alternate {
                    tracing::info!(
                        event = "prefix_replay_matched_alternate",
                        alternates_held = tracker.alternates.len(),
                        matched_prefix_msgs = o.len(),
                        most_recent_prefix_msgs = primary_len,
                        current_msgs = current_originals.len(),
                        chain_id = chain_id,
                        "replayed a stream's own prefix instead of the session's \
                         most recent one; one slot per session would have declined here"
                    );
                }
                Ok((o.clone(), f.clone(), chain_id))
            }
            None if tracker.last_forwarded_messages.is_empty() => {
                Err(PrefixMiss::NothingForwardedYet)
            }
            // Nothing held leads this turn. The prefix goes back for the
            // overlay to report against, but the chain id is 0: this turn
            // continues none of them, and saying otherwise would put two
            // unrelated runs of turns under one name.
            None => Ok((
                tracker.last_original_messages.clone(),
                tracker.last_forwarded_messages.clone(),
                0,
            )),
        }
    }

    /// [`Self::previous_turn`], but saying why there was nothing to replay.
    ///
    /// `no_previous_turn` is the commonest reason a turn declines to replay,
    /// and on its own it is not actionable: a genuine first turn is free, a
    /// session key that changed between turns is a bug worth chasing, and an
    /// idle gap past the TTL is neither — the provider's cache (5 minutes) died
    /// long before this store's 10. They need different responses, so they get
    /// different names.
    pub fn previous_turn_detailed(
        &self,
        session_key: &str,
    ) -> Result<(Vec<Value>, Vec<Value>), PrefixMiss> {
        let mut guard = match self.trackers.lock() {
            Ok(g) => g,
            Err(_) => return Err(PrefixMiss::LockPoisoned),
        };
        let Some(tracker) = guard.get(session_key) else {
            return Err(PrefixMiss::NoTrackerForSession);
        };
        if tracker.last_activity.elapsed() > self.session_ttl {
            guard.pop(session_key);
            return Err(PrefixMiss::IdlePastTtl);
        }
        if tracker.last_forwarded_messages.is_empty() {
            return Err(PrefixMiss::NothingForwardedYet);
        }
        Ok((
            tracker.last_original_messages.clone(),
            tracker.last_forwarded_messages.clone(),
        ))
    }

    /// Shorten the session TTL so a test can reach the idle path without
    /// sleeping for the production ten minutes.
    #[cfg(test)]
    fn set_session_ttl_for_test(&mut self, ttl: Duration) {
        self.session_ttl = ttl;
    }

    /// Invalidate a session's stored prefix (drift/rebuild boundary).
    pub fn invalidate(&self, session_key: &str) {
        if let Ok(mut guard) = self.trackers.lock() {
            if let Some(t) = guard.get_mut(session_key) {
                t.invalidate();
            }
        }
    }

    /// Park this turn's original + forwarded messages under `request_id` so
    /// [`complete`](Self::complete) can attribute the response's cache tokens.
    pub fn begin_request(
        &self,
        request_id: &str,
        session_key: &str,
        original_messages: Vec<Value>,
        forwarded_messages: Vec<Value>,
    ) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.put(
                request_id.to_string(),
                PendingTurn {
                    session_key: session_key.to_string(),
                    original_messages,
                    forwarded_messages,
                },
            );
        }
    }

    /// Feed the completed turn's cache tokens back into the session tracker.
    /// Called from the response side once the stream completes cleanly. No-op
    /// when the request was never parked (e.g. non-Anthropic, or feature off).
    pub fn complete(&self, request_id: &str, cache_read_tokens: u64, cache_write_tokens: u64) {
        let pending = match self.pending.lock() {
            Ok(mut g) => g.pop(request_id),
            Err(_) => None,
        };
        let Some(pending) = pending else {
            return;
        };
        if let Ok(mut guard) = self.trackers.lock() {
            let tracker = if let Some(t) = guard.get_mut(&pending.session_key) {
                t
            } else {
                guard.put(pending.session_key.clone(), PrefixReplayTracker::default());
                guard.get_mut(&pending.session_key).unwrap()
            };
            tracker.update_from_response(
                cache_read_tokens,
                cache_write_tokens,
                &pending.forwarded_messages,
                Some(&pending.original_messages),
            );
        }
    }

    #[cfg(test)]
    fn active_sessions(&self) -> usize {
        self.trackers.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}]})
    }

    // ── canonicalizer ───────────────────────────────────────────────────

    #[test]
    fn canonicalize_strips_cache_control() {
        let a = json!({"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]});
        let b = text_msg("user", "hi");
        assert_eq!(
            canonicalize_for_prefix_compare(&a),
            canonicalize_for_prefix_compare(&b)
        );
    }

    #[test]
    fn canonicalize_string_content_sugar_equals_block() {
        let string_form = json!({"role": "user", "content": "hi"});
        let block_form = text_msg("user", "hi");
        assert_eq!(
            canonicalize_for_prefix_compare(&string_form),
            canonicalize_for_prefix_compare(&block_form)
        );
    }

    #[test]
    fn canonicalize_does_not_recurse_into_tool_input() {
        // A user payload whose keys collide with NON_SEMANTIC_KEYS must survive.
        let a = json!({"type": "tool_use", "name": "f", "input": {"state": "CA", "index": 3}});
        let canon = canonicalize_for_prefix_compare(&a);
        assert_eq!(canon["input"], json!({"state": "CA", "index": 3}));
    }

    #[test]
    fn canonicalize_semantic_change_detected() {
        let a = text_msg("user", "hello");
        let b = text_msg("user", "goodbye");
        assert_ne!(
            canonicalize_for_prefix_compare(&a),
            canonicalize_for_prefix_compare(&b)
        );
    }

    #[test]
    fn canonicalize_drops_pure_directive_block() {
        // A Bedrock cachePoint block projects to {} and is dropped, so moving it
        // does not fail the compare.
        let with_cp = json!([{"type": "text", "text": "a"}, {"cachePoint": {"type": "default"}}]);
        let without = json!([{"type": "text", "text": "a"}]);
        assert_eq!(
            canonicalize_for_prefix_compare(&with_cp),
            canonicalize_for_prefix_compare(&without)
        );
    }

    #[test]
    fn empty_canonical_content_uses_the_same_projection_as_prefix_compare() {
        let reminder_only = json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>x</system-reminder>"}
        ]});
        let directive_only = json!({"role": "user", "content": [
            {"cachePoint": {"type": "default"}}
        ]});
        let real_content = json!({"role": "user", "content": [
            {"type": "text", "text": "keep me"},
            {"type": "text", "text": "<system-reminder>x</system-reminder>"}
        ]});

        assert!(has_empty_canonical_content(&reminder_only));
        assert!(has_empty_canonical_content(&directive_only));
        assert!(!has_empty_canonical_content(&real_content));
    }

    // ── overlay_cached_prefix ────────────────────────────────────────────

    #[test]
    fn overlay_replays_forwarded_prefix_byte_identical() {
        // prev turn: original m0, forwarded compressed(m0)
        let orig0 = text_msg("user", "big original message");
        let fwd0 = text_msg("user", "compressed");
        let prev_orig = vec![orig0.clone()];
        let prev_fwd = vec![fwd0.clone()];

        // this turn: originals = [m0, m1]; optimized emits original m0 again
        let m1 = text_msg("assistant", "reply");
        let current_orig = vec![orig0.clone(), m1.clone()];
        let optimized = vec![orig0.clone(), m1.clone()];

        let out =
            overlay_cached_prefix(optimized, &current_orig, Some(&prev_orig), Some(&prev_fwd));
        // Position 0 must be the compressed forwarded bytes, not the original.
        assert_eq!(out[0], fwd0);
        assert_eq!(out[1], m1);
    }

    #[test]
    fn overlay_survives_moved_cache_control_marker() {
        // #1852: a cache_control marker landing in the frozen prefix must NOT
        // fail the append-only guard (content-only comparison).
        let orig0 = text_msg("user", "hello");
        let fwd0 = text_msg("user", "hello-compressed");
        let prev_orig = vec![orig0.clone()];
        let prev_fwd = vec![fwd0.clone()];

        // current original has a moved cache_control marker on the same content.
        let orig0_marked = json!({"role": "user", "content": [{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]});
        let m1 = text_msg("assistant", "reply");
        let current_orig = vec![orig0_marked.clone(), m1.clone()];
        let optimized = vec![orig0_marked, m1.clone()];

        let out =
            overlay_cached_prefix(optimized, &current_orig, Some(&prev_orig), Some(&prev_fwd));
        assert_eq!(out[0], fwd0, "replay must fire despite moved marker");
    }

    #[test]
    fn overlay_noop_when_prefix_diverges() {
        let prev_orig = vec![text_msg("user", "hello")];
        let prev_fwd = vec![text_msg("user", "hello-c")];
        // current prefix changed content → not append-only.
        let current_orig = vec![text_msg("user", "DIFFERENT"), text_msg("assistant", "x")];
        let optimized = current_orig.clone();
        let out = overlay_cached_prefix(
            optimized.clone(),
            &current_orig,
            Some(&prev_orig),
            Some(&prev_fwd),
        );
        assert_eq!(
            out, optimized,
            "diverged prefix must return input unchanged"
        );
    }

    /// A divergence costs the whole prefix, and that is deliberate.
    ///
    /// Replaying the leading run that still agrees was implemented and measured
    /// live on 2026-08-09: it turned a 224,689-read / 981-write turn into a
    /// 22,026-read / 204,768-write one. Compression is deterministic, so this
    /// turn's own bytes for an unchanged prefix already reproduce what the
    /// provider cached; a splice between two turns' bytes matches neither.
    #[test]
    fn overlay_declines_whole_prefix_when_a_later_message_diverges() {
        let prev_orig = vec![
            text_msg("user", "a"),
            text_msg("assistant", "b"),
            text_msg("user", "c"),
        ];
        let prev_fwd = vec![
            text_msg("user", "a-c"),
            text_msg("assistant", "b-c"),
            text_msg("user", "c-c"),
        ];
        let mut current_orig = prev_orig.clone();
        current_orig[2]["content"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "text", "text": "appended"}));
        current_orig.push(text_msg("assistant", "d"));
        let optimized = current_orig.clone();

        let (out, skip) = overlay_cached_prefix_reported(
            optimized.clone(),
            &current_orig,
            Some(&prev_orig),
            Some(&prev_fwd),
        );
        assert_eq!(
            skip,
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index: 2
            }),
            "the index is still reported, as a diagnostic"
        );
        assert_eq!(
            out, optimized,
            "no partial splice: the turn forwards its own bytes throughout"
        );
    }

    #[test]
    fn overlay_noop_on_cold_start() {
        let current_orig = vec![text_msg("user", "hi")];
        let optimized = current_orig.clone();
        let out = overlay_cached_prefix(optimized.clone(), &current_orig, None, None);
        assert_eq!(out, optimized);
    }

    #[test]
    fn overlay_is_idempotent() {
        let orig0 = text_msg("user", "big");
        let fwd0 = text_msg("user", "small");
        let prev_orig = vec![orig0.clone()];
        let prev_fwd = vec![fwd0.clone()];
        let m1 = text_msg("assistant", "r");
        let current_orig = vec![orig0.clone(), m1.clone()];
        let optimized = vec![orig0.clone(), m1.clone()];

        let once =
            overlay_cached_prefix(optimized, &current_orig, Some(&prev_orig), Some(&prev_fwd));
        // Re-applying against the already-overlaid output: the prefix already
        // equals the forwarded bytes, and current_orig still canonical-matches.
        let twice = overlay_cached_prefix(
            once.clone(),
            &current_orig,
            Some(&prev_orig),
            Some(&prev_fwd),
        );
        assert_eq!(once, twice);
    }

    // ── extract_cache_stable_delta ───────────────────────────────────────

    #[test]
    fn delta_splits_prefix_and_appended_suffix() {
        let m0 = text_msg("user", "q1");
        let fwd0 = text_msg("user", "q1-compressed");
        let prev_orig = vec![m0.clone()];
        let prev_fwd = vec![fwd0.clone()];
        let m1 = text_msg("assistant", "a1");
        let m2 = text_msg("user", "q2");
        let current = vec![m0.clone(), m1.clone(), m2.clone()];

        let (prefix, delta) =
            extract_cache_stable_delta(&current, Some(&prev_orig), Some(&prev_fwd)).unwrap();
        assert_eq!(prefix, prev_fwd, "prefix is previously-forwarded bytes");
        assert_eq!(delta, vec![m1, m2], "delta is only the appended suffix");
    }

    #[test]
    fn delta_none_when_not_append_only() {
        let prev_orig = vec![text_msg("user", "q1")];
        let prev_fwd = vec![text_msg("user", "q1-c")];
        let current = vec![text_msg("user", "CHANGED")];
        assert!(extract_cache_stable_delta(&current, Some(&prev_orig), Some(&prev_fwd)).is_none());
    }

    // ── normalize_message_cache_control ──────────────────────────────────

    #[test]
    fn normalize_leaves_single_bounded_breakpoint() {
        // Markers on several messages accumulate; normalize collapses to one.
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}}]}),
        ];
        let out = normalize_message_cache_control(msgs);
        let total: usize = out
            .iter()
            .map(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("cache_control").is_some())
                            .count()
                    })
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(total, 1, "exactly one breakpoint after normalize");
        // And it lands on the last block of the last block-style message.
        let last = out.last().unwrap();
        assert!(last["content"].as_array().unwrap().last().unwrap()["cache_control"].is_object());
    }

    #[test]
    fn normalize_wraps_every_eligible_bare_string_and_marks_the_selected_one() {
        let msgs = vec![
            json!({"role": "assistant", "content": "wrap this one too"}),
            json!({"role": "user", "content": "mark this"}),
        ];
        let out = normalize_message_cache_control(msgs.clone());

        assert_eq!(
            out[0]["content"],
            json!([{"type": "text", "text": "wrap this one too"}]),
            "an unselected string is wrapped all the same — shape must not \
             depend on which message holds the marker this turn"
        );
        assert_eq!(
            out[1]["content"],
            json!([{
                "type": "text",
                "text": "mark this",
                "cache_control": {"type": "ephemeral"}
            }])
        );
        assert_eq!(
            canonicalize_for_prefix_compare(&out[1]),
            canonicalize_for_prefix_compare(&msgs[1]),
            "string sugar and the marked block form must share the append-only key"
        );
        assert!(out[1]["content"][0]["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn empty_bare_strings_are_not_wrapped_or_marked() {
        let msgs = vec![
            json!({"role": "user", "content": ""}),
            json!({"role": "assistant", "content": "  \n\t"}),
        ];
        let (out, placed) = place_tail_cache_breakpoints(msgs.clone(), 2);

        assert_eq!(placed, 0);
        assert_eq!(out, msgs);
    }

    #[test]
    fn a_final_bare_string_reminder_seals_instead_of_becoming_a_target() {
        let reminder = json!({
            "role": "user",
            "content": " \n<system-reminder>temporary</system-reminder>"
        });
        let msgs = vec![text_msg("assistant", "stable history"), reminder.clone()];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 2);

        assert_eq!(placed, 1);
        assert_eq!(marked_messages(&out), vec![0]);
        assert!(
            out[1]["content"][0]["cache_control"].is_null(),
            "the reminder is wrapped like any other string, but sealing must \
             keep it from ever taking the marker"
        );
    }

    /// Why the wrap covers every eligible string, not just the selected one.
    ///
    /// Two placement passes with nothing between them — the overlay skips a
    /// turn now and then, and when it does there is nothing to restore a
    /// message's earlier shape. The provider cached up to and including the
    /// message that held the marker last turn, so if that message goes out in
    /// a different shape now, the divergence lands inside the cached prefix and
    /// costs all of it.
    #[test]
    fn a_wrapped_string_keeps_its_shape_after_the_marker_moves_on() {
        let strip = |m: &Value| {
            let mut m = m.clone();
            for block in m["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
            m
        };

        let turn1 = normalize_message_cache_control(vec![json!({
            "role": "user", "content": "first"
        })]);
        let turn2 = normalize_message_cache_control(vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "reply"}),
            json!({"role": "user", "content": "second"}),
        ]);

        assert!(
            turn1[0]["content"][0]["cache_control"].is_object(),
            "turn 1 marks the only message there is"
        );
        assert!(
            turn2[0]["content"][0]["cache_control"].is_null(),
            "turn 2 moves the marker to the newest message"
        );
        assert_eq!(
            strip(&turn1[0]),
            strip(&turn2[0]),
            "message 0 changed shape when the marker moved off it"
        );
    }

    #[test]
    fn a_bare_string_proactive_expansion_is_left_outside_the_cache_target() {
        let expansion = json!({
            "role": "user",
            "content": "prefix <headroom_proactive_expansion>temporary</headroom_proactive_expansion>"
        });
        let msgs = vec![text_msg("assistant", "stable history"), expansion.clone()];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 2);

        assert_eq!(placed, 1);
        assert_eq!(marked_messages(&out), vec![0]);
        assert_eq!(out[1], expansion, "the expansion must remain bare and unmarked");
    }

    #[test]
    fn normalize_keeps_proactive_expansion_after_the_cache_breakpoint() {
        let msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "current question"},
                {"type": "text", "text": "<headroom_proactive_expansion>\\nold context\\n</headroom_proactive_expansion>"}
            ]
        })];

        let out = normalize_message_cache_control(msgs);
        let blocks = out[0]["content"].as_array().unwrap();
        assert!(blocks[0]["cache_control"].is_object());
        assert!(blocks[1].get("cache_control").is_none());
    }

    // ── place_tail_cache_breakpoints (2 slots) + system stripping ─────────

    /// Which messages carry a breakpoint, by index.
    fn marked_messages(messages: &[Value]) -> Vec<usize> {
        messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn two_slots_mark_the_last_two_messages() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "c"}]}),
        ];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 2);
        assert_eq!(placed, 2);
        assert_eq!(marked_messages(&out), vec![1, 2]);
    }

    /// The hedge, and the reason for it. The newest message is a reminder the
    /// client will withdraw, so it is sealed and takes no marker. One slot would
    /// checkpoint message 1 and nothing else; two reach back to message 0, which
    /// still names a prefix the provider holds after the reminder goes.
    #[test]
    fn a_sealed_newest_message_hands_both_slots_to_history() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>r</system-reminder>"}
            ]}),
        ];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 2);
        assert_eq!(placed, 2);
        assert_eq!(marked_messages(&out), vec![0, 1]);
    }

    #[test]
    fn slots_beyond_the_cacheable_messages_place_what_there_is() {
        let msgs = vec![
            json!({"role": "user", "content": "plain"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
        ];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 3);
        assert_eq!(placed, 2);
        assert_eq!(marked_messages(&out), vec![0, 1]);
    }

    /// Claude Code's own shape: two markers on `system`, none on `tools`. Two
    /// message slots fit exactly, and the guard must not take one away.
    #[test]
    fn two_system_markers_still_leave_room_for_two_message_slots() {
        let body = json!({
            "system": [
                {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ],
            "tools": [{"name": "t"}],
        });
        assert_eq!(tail_slots_within_budget(&body, 2), (2, 2));
    }

    /// PR-E3 marks `tools[last]` on PAYG. That is the third slot, so the second
    /// message slot is the one that goes — a refused request saves nothing.
    #[test]
    fn a_tool_marker_costs_the_second_message_slot() {
        let body = json!({
            "system": [
                {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral"}}
            ],
            "tools": [{"name": "t", "cache_control": {"type": "ephemeral"}}],
        });
        assert_eq!(tail_slots_within_budget(&body, 2), (1, 3));
    }

    #[test]
    fn a_full_budget_places_no_message_markers_at_all() {
        let body = json!({
            "system": (0..4)
                .map(|i| json!({"type": "text", "text": i.to_string(),
                                "cache_control": {"type": "ephemeral"}}))
                .collect::<Vec<_>>(),
        });
        assert_eq!(tail_slots_within_budget(&body, 2), (0, 4));

        // And zero slots really means none placed, not one.
        let msgs = vec![json!({"role": "user", "content": [{"type": "text", "text": "a"}]})];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 0);
        assert_eq!(placed, 0);
        assert!(marked_messages(&out).is_empty());
    }

    #[test]
    fn a_string_system_reserves_nothing() {
        let body = json!({"system": "one prompt", "messages": []});
        assert_eq!(tail_slots_within_budget(&body, 2), (2, 0));
    }

    #[test]
    fn strip_system_removes_every_client_marker() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ],
            "messages": []
        });
        assert_eq!(strip_system_cache_control(&mut body), 2);
        let blocks = body["system"].as_array().unwrap();
        assert!(blocks.iter().all(|b| b.get("cache_control").is_none()));
        // The prompt itself stays; only the marker goes.
        assert_eq!(blocks[0]["text"], json!("s1"));
    }

    #[test]
    fn strip_system_has_nothing_to_do_on_a_string_system() {
        let mut body = json!({"system": "one prompt", "messages": []});
        assert_eq!(strip_system_cache_control(&mut body), 0);
        assert_eq!(body["system"], json!("one prompt"));
    }

    // ── tracker + store ──────────────────────────────────────────────────

    #[test]
    fn tracker_cold_start_freezes_nothing() {
        let t = PrefixReplayTracker::default();
        assert_eq!(t.frozen_message_count(), 0);
    }

    #[test]
    fn tracker_computes_frozen_boundary_from_cache_tokens() {
        let mut t = PrefixReplayTracker::default();
        // Two big messages; claim enough cached tokens to cover the first only.
        let big = "x".repeat(7000); // ~2000 tokens
        let fwd = vec![text_msg("user", &big), text_msg("assistant", &big)];
        let first_tokens = estimate_message_tokens(&fwd)[0];
        t.update_from_response(first_tokens, 0, &fwd, None);
        assert_eq!(t.frozen_message_count(), 1);
    }

    #[test]
    fn tracker_drops_a_canonical_empty_tail_and_replays_its_replacement() {
        let store = SessionReplayStore::new(8);
        let stable_original = text_msg("user", &"x".repeat(7000));
        let stable_forwarded = text_msg("user", "compressed stable prefix");
        let reminder_only = json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>temporary</system-reminder>"}
        ]});

        let originals = vec![stable_original.clone(), reminder_only];
        // Relocation may leave the forwarded tail semantically non-empty even
        // though the corresponding original tail is reminder-only. The cut
        // must therefore come from `originals`, then apply at the same index.
        let forwarded = vec![
            stable_forwarded.clone(),
            json!({"role": "user", "content": [
                {"type": "text", "text": "forwarded tail"},
                {"type": "text", "text": "<system-reminder>relocated</system-reminder>"}
            ]}),
        ];
        store.begin_request("reminder-tail", "S", originals, forwarded);
        store.complete("reminder-tail", 5_000, 0);

        // On the next turn the client replaces the reminder-only tail with a
        // real bare-string message. Only the stable prefix is compared, so the
        // replay succeeds instead of reporting text -> string divergence.
        let replacement = json!({"role": "assistant", "content": "real next message"});
        let current = vec![stable_original, replacement];
        let (stored_originals, stored_forwarded, _) = store
            .previous_turn_for("S", &current)
            .expect("the stable prefix remains replayable");
        assert_eq!(stored_originals.as_slice(), &current[..1]);
        assert_eq!(stored_forwarded, vec![stable_forwarded.clone()]);

        let (out, skip) = overlay_cached_prefix_reported(
            current.clone(),
            &current,
            Some(&stored_originals),
            Some(&stored_forwarded),
        );
        assert_eq!(skip, None);
        assert_eq!(out[0], stable_forwarded);
    }

    #[test]
    fn tracker_drops_a_directive_only_tail_from_stored_replay_state() {
        let mut tracker = PrefixReplayTracker::default();
        let stable = text_msg("user", "stable");
        let directive_only = json!({"role": "user", "content": [
            {"cachePoint": {"type": "default"}}
        ]});
        let messages = vec![stable.clone(), directive_only];

        tracker.update_from_response(5_000, 0, &messages, Some(&messages));

        assert_eq!(tracker.last_original_messages(), &[stable.clone()]);
        assert_eq!(tracker.last_forwarded_messages(), &[stable]);
    }

    #[test]
    fn frozen_boundary_estimate_uses_the_untruncated_forwarded_slice() {
        let mut tracker = PrefixReplayTracker::default();
        let stable = text_msg("user", &"x".repeat(7_000));
        let reminder_only = json!({"role": "user", "content": [{
            "type": "text",
            "text": format!("<system-reminder>{}</system-reminder>", "y".repeat(7_000))
        }]});
        let messages = vec![stable, reminder_only];
        let all_forwarded_tokens = estimate_message_tokens(&messages).iter().sum();

        tracker.update_from_response(
            all_forwarded_tokens,
            0,
            &messages,
            Some(&messages),
        );

        assert_eq!(tracker.last_forwarded_messages().len(), 1);
        assert_eq!(
            tracker.frozen_message_count(),
            2,
            "the provider-reported boundary is estimated over every forwarded message"
        );
    }

    #[test]
    fn an_all_ephemeral_turn_leaves_no_replayable_prefix() {
        let store = SessionReplayStore::new(8);
        let reminder_only = json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>temporary</system-reminder>"}
        ]});
        store.begin_request(
            "reminder-only",
            "S",
            vec![reminder_only.clone()],
            vec![reminder_only],
        );
        store.complete("reminder-only", 5_000, 0);

        assert_eq!(
            store.previous_turn_detailed("S"),
            Err(PrefixMiss::NothingForwardedYet),
            "an empty stored prefix is deliberately treated as a cold replay"
        );
    }

    #[test]
    fn tracker_invalidate_clears_prefix() {
        let mut t = PrefixReplayTracker::default();
        let big = "x".repeat(7000);
        let fwd = vec![text_msg("user", &big)];
        t.update_from_response(5000, 0, &fwd, None);
        t.invalidate();
        assert!(t.last_forwarded_messages().is_empty());
        assert_eq!(t.frozen_message_count(), 0);
    }

    #[test]
    fn store_roundtrip_begin_complete_previous_turn() {
        let store = SessionReplayStore::new(8);
        let big = "x".repeat(7000);
        let orig = vec![text_msg("user", &big)];
        let fwd = vec![text_msg("user", "compressed")];
        store.begin_request("req-1", "sess-A", orig.clone(), fwd.clone());
        // no prefix until complete
        assert!(store.previous_turn("sess-A").is_none());
        store.complete("req-1", 5000, 0);
        let (po, pf) = store.previous_turn("sess-A").expect("prefix now present");
        assert_eq!(po, orig);
        assert_eq!(pf, fwd);
    }

    #[test]
    fn store_invalidate_drops_prefix() {
        let store = SessionReplayStore::new(8);
        let orig = vec![text_msg("user", &"x".repeat(7000))];
        let fwd = vec![text_msg("user", "c")];
        store.begin_request("r", "S", orig, fwd);
        store.complete("r", 5000, 0);
        assert!(store.previous_turn("S").is_some());
        store.invalidate("S");
        assert!(store.previous_turn("S").is_none());
    }

    #[test]
    fn store_lru_evicts_at_capacity() {
        let store = SessionReplayStore::new(2);
        for i in 0..3 {
            let sk = format!("S{i}");
            let rid = format!("r{i}");
            store.begin_request(
                &rid,
                &sk,
                vec![text_msg("user", "x")],
                vec![text_msg("user", "x")],
            );
            store.complete(&rid, 5000, 0);
        }
        assert_eq!(store.active_sessions(), 2, "LRU bounded to capacity");
    }

    // ── the invariant that was missing (#1850) ───────────────────────────

    #[test]
    fn cross_turn_forwarded_prefix_stays_byte_identical() {
        // Drive the real store + overlay + marker placement over multiple
        // append-only turns against a simulated provider prefix cache and
        // assert the forwarded prefix stays byte-identical turn-over-turn.
        // Load-bearing twice over: without overlay the freeze path would send
        // ORIGINAL bytes over the cached COMPRESSED prefix; without replaying
        // the wrapped string shape, a message whose marker moved on would fall
        // back to bare-string form and change the provider's prefix key.
        let store = SessionReplayStore::new(8);
        let session = "sess";

        // Cache directives choose the boundary but are not part of the content
        // key. Remove only that field, deliberately preserving string-vs-block
        // shape so this catches a wrapped message reverting to a bare string.
        let provider_key = |messages: &[Value]| -> Vec<Value> {
            messages
                .iter()
                .map(|message| {
                    let mut message = message.clone();
                    if let Some(blocks) = message
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                    {
                        for block in blocks {
                            if let Some(block) = block.as_object_mut() {
                                block.remove("cache_control");
                            }
                        }
                    }
                    message
                })
                .collect()
        };

        // Simulated provider cache: the content bytes it hashed for the prefix.
        let mut provider_cached_prefix: Option<Vec<Value>> = None;

        // Conversation grows one user+assistant pair per turn. The "original"
        // first user message is large; the compressor shrinks it to a fixed
        // compressed form every turn.
        let big = "x".repeat(7000);
        let compressed_first = json!({"role": "user", "content": "FIRST-COMPRESSED"});

        let mut originals: Vec<Value> = Vec::new();

        for turn in 0..4 {
            // Append this turn's new messages (original bytes).
            if turn == 0 {
                originals.push(json!({"role": "user", "content": big.clone()}));
            } else {
                originals.push(json!({"role": "assistant", "content": format!("reply {turn}")}));
                originals.push(json!({"role": "user", "content": format!("followup {turn}")}));
            }

            // Compressor output for THIS turn: it re-emits the original first
            // message (the freeze path bug), everything else verbatim.
            let mut optimized = originals.clone();
            optimized[0] = text_msg("user", &big); // original bytes for frozen msg

            // Overlay replays the previously-forwarded prefix byte-identical.
            let (prev_orig, prev_fwd) = match store.previous_turn(session) {
                Some((o, f)) => (Some(o), Some(f)),
                None => (None, None),
            };
            let forwarded = overlay_cached_prefix(
                optimized,
                &originals,
                prev_orig.as_deref(),
                prev_fwd.as_deref(),
            );

            // On turn 0 there's no prefix to replay; simulate the compressor
            // producing the compressed first message as what we actually send.
            let overlaid = if turn == 0 {
                let mut f = forwarded;
                f[0] = compressed_first.clone();
                f
            } else {
                forwarded
            };
            // Production order is overlay first, placement second. On turn 0
            // this wraps the only bare string. On later turns overlay restores
            // that exact wrapped shape before the marker moves to the new tail.
            let (forwarded, placed) = place_tail_cache_breakpoints(overlaid, 1);
            assert_eq!(placed, 1, "turn {turn}: one tail marker must be placed");
            let current_provider_key = provider_key(&forwarded);

            // Assert: whatever the provider cached last turn is still an exact
            // prefix of what we forward this turn.
            if let Some(ref cached) = provider_cached_prefix {
                assert_eq!(
                    &current_provider_key[..cached.len()],
                    cached.as_slice(),
                    "turn {turn}: forwarded prefix diverged from provider-cached bytes"
                );
            }

            // The first forwarded message must always be the compressed form,
            // never the big original — that is the #1850 invariant.
            assert_eq!(
                forwarded[0]["content"][0]["text"], "FIRST-COMPRESSED",
                "turn {turn}: frozen message forwarded original instead of cached compressed bytes"
            );
            assert!(
                forwarded[0]["content"].is_array(),
                "turn {turn}: a formerly marked string reverted to bare-string shape"
            );

            // Provider caches what we forwarded; record + feed the tracker.
            provider_cached_prefix = Some(current_provider_key);
            let rid = format!("req-{turn}");
            store.begin_request(&rid, session, originals.clone(), forwarded.clone());
            // Claim a healthy cache read so the tracker keeps a live prefix.
            store.complete(&rid, 6000, 500);
        }
    }
}

/// Why a turn declined to replay its cached prefix. Non-replaying turns were
/// 19% of measured traffic and carried 97% of booked re-cache waste, and the
/// five reasons need opposite responses — so each one has to be nameable.
#[cfg(test)]
mod skip_reason_tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}]})
    }

    fn skip(
        optimized: Vec<Value>,
        current: &[Value],
        prev_orig: Option<&[Value]>,
        prev_fwd: Option<&[Value]>,
    ) -> Option<ReplaySkip> {
        overlay_cached_prefix_reported(optimized, current, prev_orig, prev_fwd).1
    }

    #[test]
    fn a_replayed_turn_reports_no_reason() {
        let prev_orig = vec![msg("user", "one")];
        let prev_fwd = vec![msg("user", "compressed-one")];
        let current = vec![msg("user", "one"), msg("assistant", "two")];
        let (out, reason) = overlay_cached_prefix_reported(
            current.clone(),
            &current,
            Some(&prev_orig),
            Some(&prev_fwd),
        );
        assert_eq!(reason, None, "an append-only turn must replay");
        assert_eq!(out[0], prev_fwd[0], "the cached bytes must be forwarded");
    }

    #[test]
    fn nothing_stored_is_named_rather_than_guessed() {
        let current = vec![msg("user", "one")];
        assert_eq!(
            skip(current.clone(), &current, None, None),
            Some(ReplaySkip::NoPreviousTurn)
        );
        // An empty stored prefix is the same situation, not a different one.
        assert_eq!(
            skip(current.clone(), &current, Some(&[]), Some(&[])),
            Some(ReplaySkip::NoPreviousTurn)
        );
    }

    /// The interleaved-stream fingerprint. One session slot holds one prefix, so
    /// when a shorter stream's turn arrives after a longer stream's, it cannot
    /// replay — and a conversation never shrinks on its own. This reason
    /// appearing in production is what would confirm item 11's merge is costing
    /// real tokens here, not just mis-reporting them.
    #[test]
    fn a_turn_shorter_than_the_stored_prefix_is_named_as_such() {
        let prev_orig = vec![
            msg("user", "one"),
            msg("assistant", "two"),
            msg("user", "three"),
        ];
        let prev_fwd = prev_orig.clone();
        let current = vec![msg("user", "one")];
        assert_eq!(
            skip(current.clone(), &current, Some(&prev_orig), Some(&prev_fwd)),
            Some(ReplaySkip::ShorterThanStoredPrefix)
        );
    }

    #[test]
    fn a_diverged_client_prefix_is_named_as_such() {
        let prev_orig = vec![msg("user", "one")];
        let prev_fwd = vec![msg("user", "compressed-one")];
        // Same length, different content: the client rewrote its own history.
        let current = vec![msg("user", "one-EDITED"), msg("assistant", "two")];
        assert_eq!(
            skip(current.clone(), &current, Some(&prev_orig), Some(&prev_fwd)),
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0
            })
        );
    }

    #[test]
    fn a_forwarded_count_mismatch_is_named_as_such() {
        let prev_orig = vec![msg("user", "one"), msg("assistant", "two")];
        let prev_fwd = vec![msg("user", "compressed-one")];
        let current = prev_orig.clone();
        assert_eq!(
            skip(current.clone(), &current, Some(&prev_orig), Some(&prev_fwd)),
            Some(ReplaySkip::ForwardedCountMismatch)
        );
    }

    #[test]
    fn a_short_optimized_output_is_named_as_such() {
        let prev_orig = vec![msg("user", "one"), msg("assistant", "two")];
        let prev_fwd = prev_orig.clone();
        let current = vec![
            msg("user", "one"),
            msg("assistant", "two"),
            msg("user", "3"),
        ];
        // The pipeline collapsed the prefix away, so there is nothing to overlay.
        let optimized = vec![msg("user", "one")];
        assert_eq!(
            skip(optimized, &current, Some(&prev_orig), Some(&prev_fwd)),
            Some(ReplaySkip::OptimizedShorterThanPrefix)
        );
    }

    /// The labels reach dashboards, so they must not drift silently.
    #[test]
    fn reason_labels_are_stable_and_distinct() {
        let all = [
            ReplaySkip::NoPreviousTurn,
            ReplaySkip::ForwardedCountMismatch,
            ReplaySkip::ShorterThanStoredPrefix,
            ReplaySkip::OptimizedShorterThanPrefix,
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0,
            },
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(labels.len(), all.len(), "labels must be distinct");
        assert_eq!(
            ReplaySkip::ShorterThanStoredPrefix.as_str(),
            "shorter_than_stored_prefix"
        );
    }
}

/// `no_previous_turn` is the commonest replay decline and on its own says
/// nothing actionable. These pin the split that makes it useful.
#[cfg(test)]
mod prefix_miss_tests {
    use super::*;
    use serde_json::json;

    fn msg(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn an_unknown_session_is_named_as_having_no_tracker() {
        let store = SessionReplayStore::new(8);
        assert_eq!(
            store.previous_turn_detailed("never-seen"),
            Err(PrefixMiss::NoTrackerForSession)
        );
    }

    #[test]
    fn parking_a_turn_does_not_yet_create_a_replayable_prefix() {
        let store = SessionReplayStore::new(8);
        store.begin_request("r1", "S", vec![msg("a")], vec![msg("a")]);
        // `begin_request` only fills the pending map; the tracker appears when
        // the response completes. Until then the session looks untracked.
        assert_eq!(
            store.previous_turn_detailed("S"),
            Err(PrefixMiss::NoTrackerForSession),
            "parking alone must not create a replayable prefix"
        );
    }

    #[test]
    fn a_completed_turn_yields_its_prefix() {
        let store = SessionReplayStore::new(8);
        let orig = vec![msg("a")];
        let fwd = vec![msg("compressed-a")];
        store.begin_request("r1", "S", orig.clone(), fwd.clone());
        store.complete("r1", 5_000, 0);
        assert_eq!(store.previous_turn_detailed("S"), Ok((orig, fwd)));
    }

    /// An idle gap past the TTL is not a proxy fault — the provider's cache
    /// (5 minutes) expired well before this store's 10 — so it must be
    /// distinguishable from a tracker that went missing on a live session.
    #[test]
    fn an_idle_session_is_named_as_ttl_rather_than_missing() {
        let mut store = SessionReplayStore::new(8);
        store.set_session_ttl_for_test(Duration::from_millis(1));
        store.begin_request("r1", "S", vec![msg("a")], vec![msg("a")]);
        store.complete("r1", 5_000, 0);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            store.previous_turn_detailed("S"),
            Err(PrefixMiss::IdlePastTtl)
        );
    }

    #[test]
    fn miss_labels_are_stable_and_distinct() {
        let all = [
            PrefixMiss::NoTrackerForSession,
            PrefixMiss::IdlePastTtl,
            PrefixMiss::NothingForwardedYet,
            PrefixMiss::LockPoisoned,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|m| m.as_str()).collect();
        assert_eq!(labels.len(), all.len());
    }
}

/// One session key carries several streams (item 11). Holding one prefix per
/// session makes every alternation forward fresh bytes over cached content —
/// a bust the proxy causes itself. These pin the multi-stream store.
#[cfg(test)]
mod interleaved_stream_tests {
    use super::*;
    use serde_json::json;

    fn msg(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    /// Build a conversation of `n` messages on branch `tag`, sharing an opener
    /// so it collides on the session key exactly as a subagent does.
    fn stream(tag: &str, n: usize) -> Vec<Value> {
        let mut v = vec![msg("shared opener")];
        for i in 1..n {
            v.push(msg(&format!("{tag}-{i}")));
        }
        v
    }

    fn turn(store: &SessionReplayStore, rid: &str, orig: &[Value]) {
        // Forward a marked copy so a replay is identifiable by content.
        let fwd: Vec<Value> = orig.iter().map(|_| msg("compressed")).collect();
        store.begin_request(rid, "S", orig.to_vec(), fwd);
        store.complete(rid, 5_000, 0);
    }

    /// The defect, stated directly: A, then B, then A again. Under one slot per
    /// session, A's second turn is tested against B's prefix and declines.
    #[test]
    fn alternating_streams_can_both_still_replay() {
        let store = SessionReplayStore::new(8);
        let a1 = stream("a", 3);
        let b1 = stream("b", 5);
        turn(&store, "a1", &a1);
        turn(&store, "b1", &b1);

        // A continues its own history, which B's stored prefix does not lead.
        let a2 = stream("a", 4);
        let (orig, fwd, _) = store
            .previous_turn_for("S", &a2)
            .expect("A's own prefix is still held");
        assert_eq!(orig, a1, "A must be matched against A, not against B");
        assert_eq!(fwd.len(), a1.len());

        // And the overlay accepts it, which is the whole point.
        let (_, skip) = overlay_cached_prefix_reported(a2.clone(), &a2, Some(&orig), Some(&fwd));
        assert_eq!(skip, None, "A's turn must replay rather than decline");
    }

    /// Two interleaved streams must get two chain ids, and each must keep its
    /// own as it grows.
    ///
    /// This is the whole point of the id. `conversation_key` hashes `system`
    /// plus the first message, which these two share; message counts cannot
    /// separate them either, because A at 4 messages following B at 5 looks
    /// exactly like a compaction. Three conclusions were drawn from that
    /// ambiguity on 2026-08-09 and all three were wrong.
    #[test]
    fn interleaved_streams_get_distinct_chain_ids() {
        let store = SessionReplayStore::new(8);
        let a1 = stream("a", 3);
        let b1 = stream("b", 5);
        turn(&store, "a1", &a1);
        turn(&store, "b1", &b1);

        let a2 = stream("a", 4);
        let b2 = stream("b", 6);
        let (_, _, a_id) = store.previous_turn_for("S", &a2).expect("A's prefix held");
        let (_, _, b_id) = store.previous_turn_for("S", &b2).expect("B's prefix held");
        assert_ne!(a_id, 0, "a matched chain must be named");
        assert_ne!(b_id, 0);
        assert_ne!(a_id, b_id, "two streams must not share a chain id");

        // And the id survives the stream growing.
        turn(&store, "a2", &a2);
        let (_, _, a_id_again) = store
            .previous_turn_for("S", &stream("a", 5))
            .expect("A's prefix still held");
        assert_eq!(a_id_again, a_id, "a chain keeps its id as it grows");
    }

    #[test]
    fn a_turn_continuing_nothing_reports_no_chain() {
        let store = SessionReplayStore::new(8);
        turn(&store, "a1", &stream("a", 3));
        let unrelated = stream("c", 6);
        let (_, _, id) = store
            .previous_turn_for("S", &unrelated)
            .expect("the fallback still returns a prefix to report against");
        assert_eq!(id, 0, "continuing nothing must not borrow another chain's id");
    }

    /// The safety property. A candidate is only ever returned when it
    /// canonically leads this turn, so a wrong stream's bytes cannot be
    /// forwarded even though several are held.
    #[test]
    fn a_stream_never_receives_another_streams_prefix() {
        let store = SessionReplayStore::new(8);
        turn(&store, "a1", &stream("a", 3));
        turn(&store, "b1", &stream("b", 5));

        // A conversation that continues neither branch.
        let c = stream("c", 6);
        let got = store.previous_turn_for("S", &c);
        if let Ok((orig, fwd, _)) = got {
            // The fallback may hand back the most recent prefix, but the
            // overlay must then refuse it — nothing wrong is forwarded.
            let (out, skip) =
                overlay_cached_prefix_reported(c.clone(), &c, Some(&orig), Some(&fwd));
            assert!(skip.is_some(), "an unrelated stream must not replay");
            assert_eq!(out, c, "the turn's own bytes must be forwarded untouched");
        }
    }

    /// Growth on one stream must not accumulate stale copies of itself, or an
    /// old short prefix could win a later match and replay less than it should.
    #[test]
    fn continuing_a_stream_does_not_hoard_stale_copies() {
        let store = SessionReplayStore::new(8);
        turn(&store, "a1", &stream("a", 3));
        turn(&store, "a2", &stream("a", 4));
        turn(&store, "a3", &stream("a", 5));
        let a4 = stream("a", 6);
        let (orig, _, _) = store.previous_turn_for("S", &a4).expect("prefix held");
        assert_eq!(orig.len(), 5, "the longest matching prefix must win");
    }

    /// The bound that matters: a few very long streams are held by total
    /// messages, not by count, so one session cannot pin an unbounded pile of
    /// full conversations.
    #[test]
    fn long_streams_are_bounded_by_messages_not_count() {
        let store = SessionReplayStore::new(8);
        // Each stream alone is a quarter of the budget, so far fewer than the
        // count ceiling may be held.
        let per = MAX_ALTERNATE_MESSAGES / 4;
        for i in 0..8 {
            turn(&store, &format!("r{i}"), &stream(&format!("s{i}"), per));
        }
        let guard = store.trackers.lock().unwrap();
        let t = guard.peek("S").expect("tracker");
        let held: usize = t.alternates.iter().map(|(_, o, _)| o.len()).sum();
        assert!(
            held <= MAX_ALTERNATE_MESSAGES,
            "held {held} messages, budget is {MAX_ALTERNATE_MESSAGES}"
        );
        assert!(
            t.alternates.len() < MAX_ALTERNATE_PREFIXES,
            "the message budget must bite before the count ceiling here"
        );
    }

    /// The other direction: many SHORT streams — the subagent fan-out case —
    /// must all be kept, since that is what the store exists for.
    #[test]
    fn many_short_streams_are_all_retained() {
        let store = SessionReplayStore::new(8);
        for i in 0..MAX_ALTERNATE_PREFIXES {
            turn(&store, &format!("r{i}"), &stream(&format!("s{i}"), 12));
        }
        let guard = store.trackers.lock().unwrap();
        let t = guard.peek("S").expect("tracker");
        // All but the newest (which is the primary) stay available.
        assert_eq!(
            t.alternates.len(),
            MAX_ALTERNATE_PREFIXES - 1,
            "short subagent streams must not evict each other"
        );
    }

    /// The sizing requirement, stated as a test: a `capture-beta` fan-out puts 8
    /// subagents on one session key. Every one of them must still be able to
    /// replay its own prefix after all the others have taken turns, or the
    /// busiest sessions are exactly the ones that bust on every turn.
    #[test]
    fn eight_concurrent_subagents_can_all_still_replay() {
        let store = SessionReplayStore::new(8);
        let agents: Vec<Vec<Value>> = (0..8).map(|i| stream(&format!("agent{i}"), 30)).collect();
        // Every agent takes a turn, round-robin, twice over.
        for round in 0..2 {
            for (i, a) in agents.iter().enumerate() {
                let mut convo = a.clone();
                for r in 0..round {
                    convo.push(msg(&format!("agent{i}-round{r}")));
                }
                turn(&store, &format!("r{round}-{i}"), &convo);
            }
        }
        // Now each agent extends its own history and must find its own prefix.
        for (i, a) in agents.iter().enumerate() {
            let mut next = a.clone();
            next.push(msg(&format!("agent{i}-round0")));
            next.push(msg(&format!("agent{i}-next")));
            let (orig, fwd, _) = store
                .previous_turn_for("S", &next)
                .unwrap_or_else(|e| panic!("agent{i} lost its prefix: {e:?}"));
            let (_, skip) =
                overlay_cached_prefix_reported(next.clone(), &next, Some(&orig), Some(&fwd));
            assert_eq!(skip, None, "agent{i} was forced to decline and would bust");
        }
    }

    /// Bounded memory: a session that keeps spawning streams must not grow.
    #[test]
    fn alternates_are_capped() {
        let store = SessionReplayStore::new(8);
        for i in 0..(MAX_ALTERNATE_PREFIXES + 3) {
            turn(&store, &format!("r{i}"), &stream(&format!("s{i}"), 3 + i));
        }
        let guard = store.trackers.lock().unwrap();
        let t = guard.peek("S").expect("tracker");
        assert!(
            t.alternates.len() <= MAX_ALTERNATE_PREFIXES,
            "alternates grew to {}",
            t.alternates.len()
        );
    }

    /// A rebuild boundary kills the provider's cache, so every prefix held for
    /// that session is dead — including the alternates.
    #[test]
    fn invalidate_clears_alternates_too() {
        let store = SessionReplayStore::new(8);
        turn(&store, "a1", &stream("a", 3));
        turn(&store, "b1", &stream("b", 5));
        store.invalidate("S");
        assert_eq!(
            store.previous_turn_for("S", &stream("a", 4)),
            Err(PrefixMiss::NothingForwardedYet),
            "no stream may replay across an invalidation"
        );
    }
}

/// A conversation that declines on every turn while growing normally is not
/// being edited by its client. These pin the index that says where the churn is.
#[cfg(test)]
mod divergence_index_tests {
    use super::*;
    use serde_json::json;

    fn msg(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    fn diverge_at(prev: &[Value], current: &[Value]) -> Option<usize> {
        match overlay_cached_prefix_reported(
            current.to_vec(),
            current,
            Some(prev),
            Some(&prev.to_vec()),
        )
        .1
        {
            Some(ReplaySkip::PrefixContentDiverged { first_diff_index }) => Some(first_diff_index),
            _ => None,
        }
    }

    /// The live shape: something in the opener churns per request, so every
    /// turn declines at index 0 however long the conversation grows.
    #[test]
    fn churn_in_the_opener_is_reported_at_index_zero() {
        let prev = vec![msg("opener @ 10:00"), msg("a"), msg("b")];
        let current = vec![msg("opener @ 10:05"), msg("a"), msg("b"), msg("c")];
        assert_eq!(diverge_at(&prev, &current), Some(0));
    }

    /// A real edit deeper in the history must be reported where it happened,
    /// because that calls for the opposite response — refusing is right.
    #[test]
    fn an_edit_deeper_in_the_history_reports_its_own_index() {
        let prev = vec![msg("a"), msg("b"), msg("c"), msg("d")];
        let current = vec![msg("a"), msg("b"), msg("EDITED"), msg("d"), msg("e")];
        assert_eq!(diverge_at(&prev, &current), Some(2));
    }

    /// Transport churn the canonicalizer already neutralises must not be
    /// reported as a divergence at all — otherwise the index would point at
    /// noise and send the reader chasing the wrong thing.
    #[test]
    fn annotation_churn_is_not_a_divergence() {
        let prev = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]
        })];
        let current = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            msg("b"),
        ];
        assert_eq!(
            diverge_at(&prev, &current),
            None,
            "a moved cache_control marker is not a content change"
        );
    }
}

/// The path locator must name the churning field without ever printing a value
/// — it runs on live traffic carrying user content.
#[cfg(test)]
mod divergence_path_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_the_field_that_changed() {
        let a = json!({"role": "user", "content": [{"type": "text", "text": "at 10:00"}]});
        let b = json!({"role": "user", "content": [{"type": "text", "text": "at 10:05"}]});
        assert_eq!(
            first_structural_difference(&a, &b),
            Some("content[0].text".to_string())
        );
    }

    /// The whole point of the field: it must be safe to log. A secret in the
    /// value must not appear in the path.
    #[test]
    fn never_reveals_a_value() {
        let a = json!({"content": [{"text": "sk-ant-SECRET-TOKEN-abc123"}]});
        let b = json!({"content": [{"text": "sk-ant-DIFFERENT-xyz789"}]});
        let path = first_structural_difference(&a, &b).expect("differs");
        assert_eq!(path, "content[0].text");
        assert!(!path.contains("SECRET"), "path leaked a value: {path}");
        assert!(!path.contains("sk-ant"), "path leaked a value: {path}");
    }

    #[test]
    fn reports_a_length_change_without_contents() {
        let a = json!({"content": [{"text": "one"}]});
        let b = json!({"content": [{"text": "one"}, {"text": "two"}]});
        let path = first_structural_difference(&a, &b).expect("differs");
        assert!(path.starts_with("content[len "), "got {path}");
        assert!(!path.contains("two"), "path leaked a value: {path}");
    }

    #[test]
    fn a_key_present_on_only_one_side_is_named() {
        let a = json!({"role": "user"});
        let b = json!({"role": "user", "name": "x"});
        assert_eq!(
            first_structural_difference(&a, &b),
            Some("name".to_string())
        );
    }

    #[test]
    fn identical_values_have_no_difference() {
        let a = json!({"content": [{"text": "same"}]});
        assert_eq!(first_structural_difference(&a, &a.clone()), None);
    }

    /// End to end through the helper the proxy calls, including the
    /// canonicalization step: transport churn must not produce a path.
    #[test]
    fn describe_ignores_what_the_canonicalizer_strips() {
        let prev = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]
        })];
        let cur = vec![json!({"role": "user", "content": [{"type": "text", "text": "a"}]})];
        assert_eq!(describe_divergence(&prev, &cur, 0), None);
    }
}

/// The invariant the whole store rests on: `original` is what the CLIENT sent.
///
/// Capturing it after our own CTX stage rewrote the body made the append-only
/// guard compare our output against our output. Our offload decisions moved,
/// the guard saw a difference we had introduced, declined the replay, and bust
/// the cache it exists to protect — while the log named the client.
#[cfg(test)]
mod originals_are_the_clients_tests {
    use super::*;
    use serde_json::json;

    fn client_msg(text: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "content": [{"type": "text", "text": text}]}
        ]})
    }

    /// What `ctx_offload` does to a message: collapse the block it holds.
    fn as_offloaded(m: &Value) -> Value {
        let mut out = m.clone();
        out["content"] = json!([{"type": "text", "text": "<ctx-ref/>"}]);
        out
    }

    /// The bug, reproduced. Feed the guard OUR rewritten bodies and it declines
    /// an append-only turn — the client never changed a thing.
    #[test]
    fn comparing_our_own_rewrites_wrongly_declines() {
        let client_turn1 = vec![client_msg("a")];
        // Last turn we offloaded it; this turn we did not (or vice versa).
        let ours_prev = vec![as_offloaded(&client_turn1[0])];
        let ours_now = vec![client_msg("a"), client_msg("b")];
        let (_, skip) = overlay_cached_prefix_reported(
            ours_now.clone(),
            &ours_now,
            Some(&ours_prev),
            Some(&ours_prev),
        );
        assert!(
            matches!(skip, Some(ReplaySkip::PrefixContentDiverged { .. })),
            "this is the defect: our own offload churn reads as a client edit"
        );
    }

    /// The fix. Hand the guard the CLIENT's messages on both sides and the same
    /// turn replays cleanly, however we chose to rewrite the wire bytes.
    #[test]
    fn comparing_the_clients_own_messages_replays_cleanly() {
        let client_prev = vec![client_msg("a")];
        let client_now = vec![client_msg("a"), client_msg("b")];
        // Forwarded bytes may be anything — offloaded, compressed, whatever.
        let forwarded_prev = vec![as_offloaded(&client_prev[0])];
        let (out, skip) = overlay_cached_prefix_reported(
            client_now.clone(),
            &client_now,
            Some(&client_prev),
            Some(&forwarded_prev),
        );
        assert_eq!(skip, None, "an append-only client turn must replay");
        assert_eq!(
            out[0], forwarded_prev[0],
            "and it must replay the bytes we actually forwarded last turn"
        );
    }

    /// The guard must still catch a real client edit, or the fix has simply
    /// traded a false decline for forwarding content the client did not send.
    #[test]
    fn a_real_client_edit_is_still_caught() {
        let client_prev = vec![client_msg("a")];
        let client_now = vec![client_msg("EDITED"), client_msg("b")];
        let forwarded_prev = vec![as_offloaded(&client_prev[0])];
        let (out, skip) = overlay_cached_prefix_reported(
            client_now.clone(),
            &client_now,
            Some(&client_prev),
            Some(&forwarded_prev),
        );
        assert!(skip.is_some(), "a genuine edit must still decline");
        assert_eq!(out, client_now, "and forward the client's own bytes");
    }
}

/// `content[len 2 vs 1]` says a block vanished but not which kind. These pin
/// the shape field that names it without logging any block's contents.
#[cfg(test)]
mod block_shape_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_the_block_kinds_in_order() {
        let m = json!({"content": [
            {"type": "tool_result", "content": "big output"},
            {"type": "text", "text": "after"}
        ]});
        assert_eq!(block_type_shape(&m), "tool_result,text");
    }

    /// The signature we are hunting: a tool result collapsed away.
    #[test]
    fn a_collapsed_tool_result_is_visible_in_the_shape_change() {
        let before = json!({"content": [
            {"type": "tool_result", "content": "big output"},
            {"type": "text", "text": "after"}
        ]});
        let after = json!({"content": [{"type": "text", "text": "<ref/>"}]});
        assert_eq!(block_type_shape(&before), "tool_result,text");
        assert_eq!(block_type_shape(&after), "text");
    }

    /// Same safety rule as the path locator: contents must never appear.
    #[test]
    fn never_reveals_block_contents() {
        let m = json!({"content": [
            {"type": "text", "text": "sk-ant-SECRET-abc123"},
            {"type": "tool_result", "content": "password hunter2"}
        ]});
        let shape = block_type_shape(&m);
        assert_eq!(shape, "text,tool_result");
        for leak in ["SECRET", "sk-ant", "hunter2", "password"] {
            assert!(!shape.contains(leak), "shape leaked {leak}: {shape}");
        }
    }

    #[test]
    fn string_content_and_missing_content_are_distinguishable() {
        assert_eq!(block_type_shape(&json!({"content": "plain"})), "string");
        assert_eq!(block_type_shape(&json!({"role": "user"})), "");
    }

    // ── relocate_ephemeral_blocks ───────────────────────────────────────

    #[test]
    fn reminders_move_out_of_history_onto_the_newest_message() {
        let msgs = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>old</system-reminder>"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let out = relocate_ephemeral_blocks(msgs);
        assert_eq!(
            out[0]["content"].as_array().unwrap().len(),
            1,
            "history keeps only the tool_result"
        );
        let tail = out[2]["content"].as_array().unwrap();
        assert_eq!(tail.len(), 2, "the reminder rides on the newest message");
        assert!(tail[1]["text"].as_str().unwrap().contains("<system-reminder>"));
    }

    #[test]
    fn history_becomes_identical_whether_or_not_a_reminder_was_sent() {
        // The property the whole fix rests on: the client adding or withdrawing
        // a reminder must not change one byte of forwarded history.
        let with = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let without = vec![
            json!({"role": "user", "content": [{"type": "tool_result", "content": "out"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let a = relocate_ephemeral_blocks(with);
        let b = relocate_ephemeral_blocks(without);
        assert_eq!(a[0], b[0], "history must not depend on the reminder");
    }

    /// A message that is nothing but scaffolding leaves with it.
    ///
    /// This is the dominant case: 12 of 18 surviving divergences were a
    /// reminder-only message vanishing, which shifts every index after it.
    /// Emptying its content instead would be rejected by the API, and keeping
    /// it is what let the churn survive block-level relocation.
    #[test]
    fn a_reminder_only_message_is_dropped_not_emptied() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "opener"}]}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>only</system-reminder>"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let out = relocate_ephemeral_blocks(msgs);
        assert_eq!(out.len(), 2, "the scaffolding-only message is gone");
        assert_eq!(out[0]["content"][0]["text"], "opener");
        let tail = out[1]["content"].as_array().unwrap();
        assert_eq!(tail.len(), 2, "its reminder rides on the newest message");
        assert!(tail[1]["text"].as_str().unwrap().contains("<system-reminder>"));
    }

    /// The property that makes the whole thing work, at message level: history
    /// is identical whether or not the client sent the standalone reminder.
    #[test]
    fn history_matches_whether_or_not_a_reminder_message_was_sent() {
        let with = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "opener"}]}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let without = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "opener"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let a = relocate_ephemeral_blocks(with);
        let b = relocate_ephemeral_blocks(without);
        assert_eq!(
            a[..a.len() - 1],
            b[..b.len() - 1],
            "the client withdrawing the message must not move a byte of history"
        );
    }

    #[test]
    fn nothing_moves_when_the_newest_message_cannot_take_it() {
        // An assistant prefill, or string content, is no place for a user's
        // reminder — leave the request exactly as it came.
        let msgs = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "prefill"}]}),
        ];
        assert_eq!(relocate_ephemeral_blocks(msgs.clone()), msgs);
    }

    #[test]
    fn requests_without_reminders_are_returned_untouched() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "tool_result", "content": "out"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        assert_eq!(relocate_ephemeral_blocks(msgs.clone()), msgs);
    }

    #[test]
    fn no_block_is_lost_in_the_move() {
        let msgs = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "a"},
                {"type": "text", "text": "<system-reminder>1</system-reminder>"},
                {"type": "text", "text": "<system-reminder>2</system-reminder>"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ];
        let before: usize = msgs
            .iter()
            .map(|m| m["content"].as_array().unwrap().len())
            .sum();
        let out = relocate_ephemeral_blocks(msgs);
        let after: usize = out
            .iter()
            .map(|m| m["content"].as_array().unwrap().len())
            .sum();
        assert_eq!(before, after, "blocks are moved, never dropped");
    }

    /// The invariant the whole design rests on, across the turn boundary.
    ///
    /// On turn N a reminder rides on the newest message. On turn N+1 that
    /// message is history and is stripped, so its bytes change — which would
    /// kill the cache if the breakpoint had been inside the changed part. It is
    /// not: the marker goes on the last non-ephemeral block, so everything the
    /// provider actually cached is byte-identical across the two turns.
    #[test]
    fn the_cached_region_survives_the_newest_message_becoming_history() {
        let cached_region = |msgs: Vec<Value>| -> Vec<Value> {
            let out = normalize_message_cache_control(relocate_ephemeral_blocks(msgs));
            // Everything up to and including the marked block is what the
            // provider caches; the rest rides outside it.
            let mut region = Vec::new();
            for m in &out {
                let mut kept = Vec::new();
                let mut done = false;
                for b in m["content"].as_array().unwrap() {
                    let marked = b.get("cache_control").is_some();
                    let mut b = b.clone();
                    b.as_object_mut().unwrap().remove("cache_control");
                    kept.push(b);
                    if marked {
                        done = true;
                        break;
                    }
                }
                region.push(Value::Array(kept));
                if done {
                    break;
                }
            }
            region
        };

        let turn_n = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "opener"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
        ];
        // Next turn the client has withdrawn the reminder and added two messages.
        let mut turn_n1 = turn_n.clone();
        turn_n1[2] = json!({"role": "user", "content": [{"type": "tool_result", "content": "out"}]});
        turn_n1.push(json!({"role": "assistant", "content": [{"type": "text", "text": "more"}]}));
        turn_n1.push(json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}));

        let a = cached_region(turn_n);
        let b = cached_region(turn_n1);
        let shared = a.len().min(b.len());
        assert_eq!(
            a[..shared],
            b[..shared],
            "the bytes the provider cached must not move when the reminder leaves"
        );
    }

    #[test]
    fn comparison_ignores_a_reminder_that_came_or_went() {
        // The other half: the append-only guard must see these two as the same
        // message, or the turn declines and the chain looks like a branch.
        let with = json!({"role": "user", "content": [
            {"type": "tool_result", "content": "out"},
            {"type": "text", "text": "<system-reminder>x</system-reminder>"}]});
        let without = json!({"role": "user", "content": [
            {"type": "tool_result", "content": "out"}]});
        assert_eq!(
            canonicalize_for_prefix_compare(&with),
            canonicalize_for_prefix_compare(&without)
        );
    }

    #[test]
    fn comparison_still_sees_a_real_edit_beside_a_reminder() {
        let a = json!({"role": "user", "content": [
            {"type": "tool_result", "content": "out"},
            {"type": "text", "text": "<system-reminder>x</system-reminder>"}]});
        let b = json!({"role": "user", "content": [
            {"type": "tool_result", "content": "DIFFERENT"},
            {"type": "text", "text": "<system-reminder>x</system-reminder>"}]});
        assert_ne!(
            canonicalize_for_prefix_compare(&a),
            canonicalize_for_prefix_compare(&b)
        );
    }

    // ── ephemeral blocks stay outside the cached region ─────────────────

    fn reminder() -> Value {
        json!({"type": "text", "text": "<system-reminder>do the thing</system-reminder>"})
    }

    fn text_msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn breakpoint_lands_before_a_trailing_system_reminder() {
        // The shape measured live: a reminder hung off the newest tool_result.
        // The marker must sit on the tool_result, so the cached prefix ends
        // before the block that will vanish next turn.
        let msgs = vec![
            text_msg("user", "a"),
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "output"},
                reminder()]}),
        ];
        let out = normalize_message_cache_control(msgs);
        let blocks = out[1]["content"].as_array().unwrap();
        assert!(
            blocks[0].get("cache_control").is_some(),
            "breakpoint belongs on the tool_result"
        );
        assert!(
            blocks[1].get("cache_control").is_none(),
            "the reminder must stay outside the cached prefix"
        );
    }

    #[test]
    fn a_reminder_seals_everything_after_it() {
        // Even a later ordinary block must not take the marker: Anthropic
        // caches up to and including it, which would swallow the reminder.
        let msgs = vec![json!({"role": "user", "content": [
            {"type": "tool_result", "content": "output"},
            reminder(),
            {"type": "text", "text": "trailing"}]})];
        let out = normalize_message_cache_control(msgs);
        let blocks = out[0]["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_some());
        assert!(blocks[1].get("cache_control").is_none());
        assert!(
            blocks[2].get("cache_control").is_none(),
            "a block after the reminder must not be the cache target"
        );
    }

    #[test]
    fn latest_user_reminder_seals_an_assistant_ended_tool_turn() {
        // Live 2026-08-13 shape: the newest user message carried transient
        // scaffolding, then an assistant tool-use message ended the request.
        // Caching through the assistant also cached the reminder and caused a
        // 6,434-token rebuild as soon as the client withdrew it.
        let msgs = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "question"},
                reminder()]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tool-1", "name": "search", "input": {}}]}),
        ];
        let (out, placed) = place_tail_cache_breakpoints(msgs, 2);

        assert_eq!(placed, 1);
        assert!(out[0]["content"][0].get("cache_control").is_some());
        assert!(out[0]["content"][1].get("cache_control").is_none());
        assert!(out[1]["content"][0].get("cache_control").is_none());
    }

    /// Shipped as a regression on 2026-08-12: 400s on 8% of turns, every one
    /// `messages.61.content.0.thinking.cache_control: Extra inputs are not
    /// permitted`. Extended thinking produces assistant messages whose only
    /// block is a thinking block, and "the last block of the message" put the
    /// marker somewhere Anthropic refuses to accept it.
    #[test]
    fn a_thinking_block_never_takes_the_breakpoint() {
        let msgs = vec![
            text_msg("user", "question"),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reasoning", "signature": "sig"}]}),
        ];
        let out = normalize_message_cache_control(msgs);
        assert!(
            out[1]["content"][0].get("cache_control").is_none(),
            "Anthropic rejects the whole request over a marker here"
        );
        assert!(
            out[0]["content"][0].get("cache_control").is_some(),
            "the breakpoint must fall back to the last legal block, not vanish"
        );
    }

    /// The fallback stays inside the message when it has an ordinary block:
    /// skipping the whole message would strand its content outside the cache.
    #[test]
    fn a_message_that_opens_with_thinking_is_still_cacheable() {
        let msgs = vec![json!({"role": "assistant", "content": [
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "answer"}]})];
        let out = normalize_message_cache_control(msgs);
        let blocks = out[0]["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[1].get("cache_control").is_some());
    }

    /// The case that nearly shipped as a regression.
    ///
    /// Reminders do persist in history — seen live at message 168, present on
    /// both turns. A persisting one is part of the stable prefix. Sealing on it
    /// would strand every later message outside the cache and cost far more
    /// than the churn the seal exists to prevent.
    #[test]
    fn a_reminder_deep_in_history_does_not_seal_the_rest() {
        let msgs = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "content": "output"}, reminder()]}),
            text_msg("assistant", "reply"),
            text_msg("user", "newest"),
        ];
        let out = normalize_message_cache_control(msgs);
        assert!(
            out[2]["content"][0].get("cache_control").is_some(),
            "the breakpoint must still reach the newest message"
        );
        assert!(out[0]["content"][0].get("cache_control").is_none());
        assert!(out[1]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn conversations_without_reminders_are_unaffected() {
        let msgs = vec![text_msg("user", "a"), text_msg("assistant", "b")];
        let out = normalize_message_cache_control(msgs);
        assert!(out[0]["content"][0].get("cache_control").is_none());
        assert!(
            out[1]["content"][0].get("cache_control").is_some(),
            "the breakpoint still belongs on the newest block"
        );
    }

    #[test]
    fn the_reminder_itself_is_never_removed_or_moved() {
        // The whole point of doing it this way: the model still sees the
        // reminder, in place, on the turn it arrives.
        let msgs = vec![json!({"role": "user", "content": [
            {"type": "tool_result", "content": "output"}, reminder()]})];
        let out = normalize_message_cache_control(msgs.clone());
        assert_eq!(out[0]["content"][1]["text"], msgs[0]["content"][1]["text"]);
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
    }

    // ── text_block_kinds ────────────────────────────────────────────────

    #[test]
    fn text_kinds_name_the_clients_ephemeral_scaffolding() {
        let m = json!({"content": [
            {"type": "tool_result", "content": "…"},
            {"type": "text", "text": "<system-reminder>do the thing</system-reminder>"},
            {"type": "text", "text": "an ordinary sentence"}
        ]});
        assert_eq!(text_block_kinds(&m), "system-reminder,plain");
    }

    #[test]
    fn text_kinds_never_reveal_the_text() {
        // Including the tag name would defeat the point: a tag is as
        // user-controlled as the body it wraps.
        let m = json!({"content": [
            {"type": "text", "text": "<sk-ant-SECRET-abc123>hunter2</x>"},
            {"type": "text", "text": "password hunter2"}
        ]});
        let kinds = text_block_kinds(&m);
        assert_eq!(kinds, "other-tag,plain");
        for leak in ["SECRET", "sk-ant", "hunter2", "password"] {
            assert!(!kinds.contains(leak), "kinds leaked {leak}: {kinds}");
        }
    }

    #[test]
    fn text_kinds_ignore_non_text_blocks_and_string_content() {
        let m = json!({"content": [{"type": "tool_result", "content": "x"}]});
        assert_eq!(text_block_kinds(&m), "");
        assert_eq!(text_block_kinds(&json!({"content": "plain"})), "");
        assert_eq!(text_block_kinds(&json!({"role": "user"})), "");
    }

    #[test]
    fn text_kinds_tolerate_leading_whitespace() {
        let m = json!({"content": [
            {"type": "text", "text": "\n  <system-reminder>x</system-reminder>"}]});
        assert_eq!(text_block_kinds(&m), "system-reminder");
    }

    /// Reminders the client has withdrawn must not survive in what we forward.
    ///
    /// Relocation and replay pull in opposite directions. Relocation moves a
    /// reminder onto the newest message; the next turn that message is history,
    /// so the frozen prefix carries the reminder forward even though the client
    /// dropped it. [`canonicalize_for_prefix_compare`] filters reminders out, so
    /// the append-only guard cannot see the difference and replays anyway.
    ///
    /// Each turn leaves one more behind. Measured live on 2026-08-11: the
    /// forwarded body climbed from 82% of the client body to 382% over sixteen
    /// turns, with roughly 28k tokens per turn sitting past the last breakpoint
    /// where they bill fresh at 1.0x.
    #[test]
    fn replay_does_not_accumulate_withdrawn_reminders() {
        fn reminder_count(messages: &[Value]) -> usize {
            messages
                .iter()
                .filter_map(|m| m.get("content").and_then(Value::as_array))
                .flatten()
                .filter(|b| is_ephemeral_client_block(b))
                .count()
        }

        // The client keeps a reminder on its newest message only, withdrawing
        // the previous one — which is what Claude Code does.
        let mut client: Vec<Value> = Vec::new();
        let mut prev: Option<(Vec<Value>, Vec<Value>)> = None;
        for turn in 0..4 {
            if let Some(last) = client.last_mut() {
                let blocks = last.get_mut("content").and_then(Value::as_array_mut).unwrap();
                blocks.retain(|b| !is_ephemeral_client_block(b));
            }
            if turn > 0 {
                client.push(text_msg("assistant", &format!("reply {turn}")));
            }
            client.push(json!({"role": "user", "content": [
                {"type": "text", "text": format!("ask {turn}")},
                {"type": "text", "text": format!("<system-reminder>r{turn}</system-reminder>")},
            ]}));

            // What the proxy does to it: relocate, then overlay last turn's
            // forwarded prefix. Compression is the identity here so the test
            // measures the replay path and nothing else.
            let originals = relocate_ephemeral_blocks(client.clone());
            let (prev_orig, prev_fwd) = match &prev {
                Some((o, f)) => (Some(o.as_slice()), Some(f.as_slice())),
                None => (None, None),
            };
            let forwarded =
                overlay_cached_prefix(originals.clone(), &originals, prev_orig, prev_fwd);

            assert_eq!(
                reminder_count(&forwarded),
                1,
                "turn {turn}: the client is sending one reminder, so one is what \
                 goes upstream — found {} in {} messages",
                reminder_count(&forwarded),
                forwarded.len()
            );
            prev = Some((originals, forwarded));
        }
    }
}
