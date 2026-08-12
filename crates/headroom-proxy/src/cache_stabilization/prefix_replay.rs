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
/// Used ONLY as a comparison key; the original, unmodified messages are always
/// what gets forwarded.
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
    let (prev_orig, prev_fwd) = match (previous_original_messages, previous_forwarded_messages) {
        (Some(o), Some(f)) if !o.is_empty() && !f.is_empty() => (o, f),
        _ => return optimized_messages,
    };
    let n = prev_orig.len();
    // One forwarded message per original, and the frozen prefix must fit within
    // both the current originals and this turn's optimized output.
    if prev_fwd.len() != n {
        return optimized_messages;
    }
    if current_original_messages.len() < n || optimized_messages.len() < n {
        return optimized_messages;
    }
    // Append-only guard on CONTENT ONLY (#1852): compare with the shared
    // canonicalizer so the guard is robust to ALL per-turn transport /
    // annotation churn — cache_control movement, litellm `caller`, streaming
    // `index`, string↔block content shape, etc.
    if canonicalize_slice(&current_original_messages[..n]) != canonicalize_slice(prev_orig) {
        return optimized_messages;
    }
    // Replay the cached (compressed) prefix byte-identical; keep this turn's tail.
    let mut out = prev_fwd.to_vec();
    out.extend_from_slice(&optimized_messages[n..]);
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
/// Only block-style (list) content can carry `cache_control`; string content is
/// left as-is. Returns the input unchanged when there is nothing to normalize.
pub fn normalize_message_cache_control(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut last_block_idx: Option<usize> = None;

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
                let non_empty_last = stripped.last().map(|b| b.is_object()).unwrap_or(false);
                m.insert("content".to_string(), Value::Array(stripped));
                out.push(Value::Object(m));
                if non_empty_last {
                    last_block_idx = Some(i);
                }
            } else {
                let non_empty_last = content.last().map(|b| b.is_object()).unwrap_or(false);
                out.push(msg);
                if non_empty_last {
                    last_block_idx = Some(i);
                }
            }
        } else {
            out.push(msg);
        }
    }

    // Re-place exactly one breakpoint on the last block-style message.
    if let Some(idx) = last_block_idx {
        if let Some(content) = out[idx].get_mut("content").and_then(|c| c.as_array_mut()) {
            if let Some(Value::Object(last)) = content.last_mut() {
                last.insert(
                    "cache_control".to_string(),
                    serde_json::json!({"type": "ephemeral"}),
                );
            }
        }
    }

    out
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
        self.last_original_messages = original_messages.unwrap_or(forwarded).to_vec();
        self.last_forwarded_messages = forwarded.to_vec();

        let total_cached = cache_read_tokens + cache_write_tokens;
        if total_cached == 0 {
            self.cached_token_count = 0;
            self.cached_message_count = 0;
            return;
        }

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
        let mut guard = match self.trackers.lock() {
            Ok(g) => g,
            Err(_) => return None,
        };
        let tracker = guard.get(session_key)?;
        if tracker.last_activity.elapsed() > self.session_ttl {
            guard.pop(session_key);
            return None;
        }
        if tracker.last_forwarded_messages.is_empty() {
            return None;
        }
        Some((
            tracker.last_original_messages.clone(),
            tracker.last_forwarded_messages.clone(),
        ))
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
    fn normalize_string_content_untouched() {
        let msgs = vec![json!({"role": "user", "content": "plain"})];
        let out = normalize_message_cache_control(msgs.clone());
        assert_eq!(out, msgs);
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
        // Drive the real store + overlay over multiple append-only turns against
        // a simulated provider prefix cache and assert the forwarded prefix stays
        // byte-identical turn-over-turn. Load-bearing: without the overlay the
        // freeze path would forward ORIGINAL bytes over the cached COMPRESSED
        // prefix and this assertion fails.
        let store = SessionReplayStore::new(8);
        let session = "sess";

        // Simulated provider cache: the bytes it hashed for the prefix.
        let mut provider_cached_prefix: Option<Vec<Value>> = None;

        // Conversation grows one user+assistant pair per turn. The "original"
        // first user message is large; the compressor shrinks it to a fixed
        // compressed form every turn.
        let big = "x".repeat(7000);
        let compressed_first = text_msg("user", "FIRST-COMPRESSED");

        let mut originals: Vec<Value> = Vec::new();

        for turn in 0..4 {
            // Append this turn's new messages (original bytes).
            if turn == 0 {
                originals.push(text_msg("user", &big));
            } else {
                originals.push(text_msg("assistant", &format!("reply {turn}")));
                originals.push(text_msg("user", &format!("followup {turn}")));
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
            let forwarded = if turn == 0 {
                let mut f = forwarded;
                f[0] = compressed_first.clone();
                f
            } else {
                forwarded
            };

            // Assert: whatever the provider cached last turn is still an exact
            // prefix of what we forward this turn.
            if let Some(ref cached) = provider_cached_prefix {
                assert_eq!(
                    &forwarded[..cached.len()],
                    cached.as_slice(),
                    "turn {turn}: forwarded prefix diverged from provider-cached bytes"
                );
            }

            // The first forwarded message must always be the compressed form,
            // never the big original — that is the #1850 invariant.
            assert_eq!(
                forwarded[0], compressed_first,
                "turn {turn}: frozen message forwarded original instead of cached compressed bytes"
            );

            // Provider caches what we forwarded; record + feed the tracker.
            provider_cached_prefix = Some(forwarded.clone());
            let rid = format!("req-{turn}");
            store.begin_request(&rid, session, originals.clone(), forwarded.clone());
            // Claim a healthy cache read so the tracker keeps a live prefix.
            store.complete(&rid, 6000, 500);
        }
    }
}
