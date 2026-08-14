//! PR-E6: cache-bust drift detector.
//!
//! # What it does
//!
//! For every inbound request on a known LLM endpoint, compute a
//! [`StructuralHash`] over the **cache hot zone**:
//!
//! - `system` — SHA-256 of the canonical system-prompt bytes (Anthropic
//!   `body.system`; OpenAI Chat first `role=system` message;
//!   OpenAI Responses `body.instructions`).
//! - `tools` — SHA-256 of the canonical bytes of `body.tools`.
//! - `early_messages` — per-message SHA-256 of the first 3
//!   conversation messages (or all, if fewer than 3). Skips the
//!   live-zone tail where mutation is expected and benign.
//!
//! All axes are canonicalized before hashing ([`canonicalize_for_hash`]):
//! objects are rebuilt with sorted keys (whitespace- and key-order-
//! neutral — the workspace's `preserve_order` feature would otherwise
//! keep wire order in the hash) and `cache_control` members are
//! stripped outside opaque tool payloads — clients relocate cache
//! breakpoints to the newest block every turn, and moving a breakpoint
//! never invalidates a previously cached prefix, so markers are
//! placement metadata rather than structure.
//!
//! Track the previous fingerprint per session in a bounded LRU. When a
//! subsequent request on the same session disagrees on any dimension,
//! emit a `cache_drift_observed` log line listing the drifted
//! dimensions. The `early_messages` comparison is prefix-aware: a
//! conversation *growing into* the window (turn 2 appends messages
//! after turn 1's) is append-only and benign, while a settled early
//! message that changes or disappears busted the provider's prefix
//! cache and is drift. **Never mutates the request body** — the
//! detector is a pure observer and the proxy's "passthrough is sacred"
//! invariant (Phase A) is preserved by construction.
//!
//! Each drift event also carries `early_drift`, which breaks the
//! `early_messages` axis down to the slot and content block that moved
//! (`0:blocks 1->2`, `0:block[1]`, `1:dropped`). Naming the dimension
//! was never enough to find the component responsible; naming the block
//! separates an inserted block from a rewritten one, which is the
//! difference between an injector and a rewriter. See
//! [`early_drift_detail`] for the vocabulary.
//!
//! Known trade-off: without an explicit `x-headroom-session-id`, the
//! session identity is anchored on the conversation's first message
//! (see [`conversation_discriminator`]), so a client that *rewrites*
//! that message (history compaction, rolling-window truncation)
//! re-keys to a fresh session and the rewrite surfaces as
//! `cache_drift_first_request` rather than `cache_drift_observed` —
//! traded deliberately against the credential-keyed alternative, which
//! false-warned on every conversation switch. With the explicit header
//! the identity is pinned and rewrites are reported as drift.
//!
//! # Privacy
//!
//! The session key prefers the client's explicit
//! `x-headroom-session-id` header — the same opt-in the Python proxy
//! honors for all session-sticky state — and otherwise combines the
//! strongest available client identifier (`Authorization`, `x-api-key`,
//! client IP, finally `(client_ip, user_agent)`) with a fingerprint of
//! the conversation's first message, so concurrent conversations that
//! share one credential do not share one drift session. Bearer tokens,
//! API keys, and the session-id header value are **hashed before they
//! ever leave this module**; the raw secret is never logged and never
//! stored. The conversation fingerprint is likewise a truncated SHA-256
//! — no message content appears in the key. The log line itself only
//! includes a short prefix of the SHA-256 hex of the session key.
//!
//! # Cost
//!
//! - Up to six SHA-256 digests per request (system, tools, up to
//!   three early messages, and the session key's conversation
//!   fingerprint), each over a canonicalized clone (filtered,
//!   key-sorted rebuild) of the corresponding subtree. Still well
//!   under a millisecond on a typical agentic request; the detector
//!   stays log-only and off the forwarded-bytes path.
//! - One LRU lookup + insert. `lru = "0.12"` is O(1) amortised.
//! - One `tracing::info!` or `tracing::warn!`. No metric emission yet
//!   (left for Phase F PR-F* when the global Prometheus registry can
//!   accept session-scoped counters without a cardinality explosion).

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use lru::LruCache;
use sha2::{Digest, Sha256};

/// Which provider's body shape we're hashing. The walker is shaped
/// per provider because the cache hot zone lives in different fields:
/// Anthropic uses `body.system`/`body.tools`/`body.messages`, OpenAI
/// Chat threads `system` into the first message, and OpenAI Responses
/// uses `body.instructions`/`body.tools`/`body.input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    /// `POST /v1/messages` (Anthropic).
    Anthropic,
    /// `POST /v1/chat/completions` (OpenAI).
    OpenAiChat,
    /// `POST /v1/responses` (OpenAI Responses API).
    OpenAiResponses,
}

/// Three-axis structural fingerprint of the cache hot zone.
///
/// Each axis is the SHA-256 of canonical bytes at that position (see
/// [`canonicalize_for_hash`]): all three axes must be *stable* for
/// "no drift", and each drifting axis is named in the emitted event.
///
/// `early_messages` holds one hash per settled slot of the early
/// window (`None` = the conversation hasn't grown that far yet), so
/// the comparison can tell "grew into the window" (benign) apart from
/// "a settled message changed" (drift). Note that the derived `==` is
/// stricter than the drift predicate — growth compares unequal but is
/// not drift; use [`drift_dims`]'s emptiness for drift decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuralHash {
    pub system: [u8; 32],
    pub tools: [u8; 32],
    pub early_messages: [Option<[u8; 32]>; EARLY_MESSAGES_WINDOW],
    /// Block-level breakdown of the same early messages, kept so a
    /// drift event can say *what* changed inside a slot rather than
    /// only that the slot changed. Purely diagnostic — the drift
    /// decision reads [`StructuralHash::early_messages`].
    pub early_shapes: [Option<MessageShape>; EARLY_MESSAGES_WINDOW],
}

/// How many message-shaped items count as the "early" prefix that
/// feeds [`early_message_hashes`]. Anything past this is the live zone
/// (where mutation is expected; we deliberately ignore it).
pub const EARLY_MESSAGES_WINDOW: usize = 3;

/// How many content blocks of one early message get an individual
/// fingerprint. Eight covers every real first message observed from
/// Claude Code and the Codex CLI; a change past that still shows up as
/// drift, just without a block index attached.
pub const EARLY_BLOCKS_WINDOW: usize = 8;

/// Block-level fingerprint of one early message.
///
/// Answers the question a bare `early_messages` drift cannot: was a
/// block *inserted* (count moved), or was an existing one *rewritten*
/// (count held, one hash moved)? Those two point at different
/// components — an injector versus a rewriter — and telling them apart
/// is the whole reason this exists.
///
/// Block hashes are truncated to 8 bytes. They only ever have to
/// answer "same or different" within one session's log line, and 64
/// bits does that without making every cached fingerprint three times
/// larger. Blocks are canonicalized before hashing exactly as the
/// message-level hash is, so a relocated `cache_control` breakpoint
/// does not read as a rewritten block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageShape {
    /// Content blocks in the message, or `None` for a plain string
    /// body (which has no block structure to index into).
    pub blocks: Option<usize>,
    /// One hash per block, capped at [`EARLY_BLOCKS_WINDOW`].
    pub block_hashes: [Option<[u8; 8]>; EARLY_BLOCKS_WINDOW],
}

/// Compute a [`StructuralHash`] for the body shape implied by `kind`.
///
/// `body` is borrowed; **this function never mutates it**. The
/// `does_not_mutate_input` test in the module below pins this with a
/// clone-and-compare assertion.
pub fn compute_structural_hash(body: &serde_json::Value, kind: ApiKind) -> StructuralHash {
    let system = hash_value(&canonicalize_for_hash(&extract_system(body, kind), false));
    let tools = hash_value(&canonicalize_for_hash(&extract_tools(body), false));
    let (early_messages, early_shapes) = early_message_hashes(body, kind);
    StructuralHash {
        system,
        tools,
        early_messages,
        early_shapes,
    }
}

/// Keys whose values are opaque user payloads: tool-call arguments and
/// schemas. Never strip inside them — a user field that happens to be
/// named `cache_control` is data, and dropping it would make two
/// genuinely different payloads hash identically, masking real drift.
/// `input`/`arguments`/`json` mirror the Python canonicalizer's
/// `_OPAQUE_PAYLOAD_KEYS`; `input_schema` extends the same rule to
/// tool definitions, which the Python comparator never hashes but the
/// `tools` axis here does.
const OPAQUE_PAYLOAD_KEYS: [&str; 4] = ["input", "arguments", "json", "input_schema"];

/// Canonicalize a JSON tree for hashing: rebuild every object with
/// sorted keys, dropping `cache_control` members outside opaque
/// payloads.
///
/// Key sorting is what makes the hashes genuinely key-order neutral:
/// this workspace builds `serde_json` with `preserve_order`, so a
/// plain re-serialize would keep the client's wire order and a
/// serializer-side reordering would read as drift (and would rotate
/// the conversation fingerprint). Sorting is a pure reordering — no
/// information is lost, so distinct payloads never conflate.
///
/// Cache-breakpoint markers are placement metadata, not structure:
/// clients relocate them to the newest block every turn (observed live
/// from Claude Code), and moving a breakpoint never invalidates a
/// previously cached prefix. Hashing them would flag drift on every
/// relocation.
fn canonicalize_for_hash(value: &serde_json::Value, in_opaque: bool) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map
                .iter()
                .filter(|(k, _)| in_opaque || k.as_str() != "cache_control")
                .collect();
            entries.sort_by_key(|(k, _)| k.as_str());
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| {
                        let opaque = in_opaque || OPAQUE_PAYLOAD_KEYS.contains(&k.as_str());
                        (k.clone(), canonicalize_for_hash(v, opaque))
                    })
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| canonicalize_for_hash(v, in_opaque))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Extract the "system" axis as a `serde_json::Value`. Returns
/// `Value::Null` when the dimension is absent — Null still hashes to
/// a stable 32-byte digest so first-request comparisons are
/// well-defined.
fn extract_system(body: &serde_json::Value, kind: ApiKind) -> serde_json::Value {
    match kind {
        ApiKind::Anthropic => body
            .get("system")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        ApiKind::OpenAiChat => {
            // First message with `role == "system"` is the OpenAI
            // Chat hot-zone equivalent. There can be at most one in
            // practice (newer requests use a `developer` role; that's
            // not the system axis and we deliberately don't conflate).
            body.get("messages")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter().find(|m| {
                        m.get("role")
                            .and_then(|r| r.as_str())
                            .map(|s| s == "system")
                            .unwrap_or(false)
                    })
                })
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        }
        ApiKind::OpenAiResponses => body
            .get("instructions")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    }
}

/// Extract the "tools" axis as a `serde_json::Value`. The same
/// `tools` array key is used by all three providers in practice.
fn extract_tools(body: &serde_json::Value) -> serde_json::Value {
    body.get("tools")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Collect the conversation-scoped message items for `kind`: Anthropic
/// `messages`, OpenAI Chat `messages` minus `role:"system"` entries
/// (the system axis already hashes those separately), OpenAI Responses
/// `input`. A Responses string-form `input` counts as one item.
fn conversation_messages(body: &serde_json::Value, kind: ApiKind) -> Vec<&serde_json::Value> {
    let array_key = match kind {
        ApiKind::Anthropic => "messages",
        ApiKind::OpenAiChat => "messages",
        ApiKind::OpenAiResponses => "input",
    };
    let items = match (kind, body.get(array_key)) {
        (_, Some(serde_json::Value::Array(arr))) => arr.iter().collect::<Vec<_>>(),
        // `input` may be a bare string in the Responses API. The other
        // shapes only accept arrays; anything else is malformed and
        // will be rejected upstream, so contribute nothing here.
        (ApiKind::OpenAiResponses, Some(s @ serde_json::Value::String(_))) => vec![s],
        _ => return Vec::new(),
    };
    match kind {
        ApiKind::OpenAiChat => items
            .into_iter()
            .filter(|m| {
                m.get("role")
                    .and_then(|r| r.as_str())
                    .map(|s| s != "system")
                    .unwrap_or(true)
            })
            .collect(),
        _ => items,
    }
}

/// Hash each of the first [`EARLY_MESSAGES_WINDOW`] conversation
/// messages individually (canonicalized). Slots the conversation has
/// not grown into yet stay `None`.
fn early_message_hashes(
    body: &serde_json::Value,
    kind: ApiKind,
) -> (
    [Option<[u8; 32]>; EARLY_MESSAGES_WINDOW],
    [Option<MessageShape>; EARLY_MESSAGES_WINDOW],
) {
    let mut out = [None; EARLY_MESSAGES_WINDOW];
    let mut shapes = [None; EARLY_MESSAGES_WINDOW];
    for (slot, msg) in conversation_messages(body, kind)
        .into_iter()
        .take(EARLY_MESSAGES_WINDOW)
        .enumerate()
    {
        let msg = string_content_as_block(msg);
        out[slot] = Some(hash_value(&canonicalize_for_hash(&msg, false)));
        shapes[slot] = Some(message_shape(&msg));
    }
    (out, shapes)
}

/// A message whose string content is rewritten as the one-block form
/// every message with a breakpoint already uses.
///
/// Attaching a breakpoint requires blocks, so a message flips between
/// `"hi"` and `[{"type":"text","text":"hi","cache_control":…}]` as the
/// client moves its markers. Dropping `cache_control` in
/// [`canonicalize_for_hash`] is not enough — the two shapes still hash
/// differently, so the message reads as rewritten every time a marker
/// lands on it or leaves it, and [`shape_delta`] can only answer
/// `string` because one side has no blocks to index.
///
/// That noise is what made `early_drift` useless: of 41 events
/// attributed on 2026-08-11, 34 were this flip and 2 were the real
/// thing — a `<system-reminder>` block deleted from settled history.
/// Both shapes carry the same text and bill the same tokens, so they
/// are the same prefix.
///
/// Widening the string into a block rather than flattening the block
/// into a string is what keeps the block *count* meaningful. Collapse
/// the other way and a message that lost one of two blocks reports
/// `string` instead of `blocks 2->1`, which is the one case worth
/// naming.
fn string_content_as_block(msg: &serde_json::Value) -> Cow<'_, serde_json::Value> {
    let Some(text @ serde_json::Value::String(_)) = msg.get("content") else {
        return Cow::Borrowed(msg);
    };
    let mut widened = msg.clone();
    if let Some(obj) = widened.as_object_mut() {
        obj.insert(
            "content".to_string(),
            serde_json::json!([{"type": "text", "text": text}]),
        );
    }
    Cow::Owned(widened)
}

/// Fingerprint each content block of one message. Content that is not
/// a block list has nothing to index, so it records `blocks: None` and
/// no hashes — the message-level hash already covers whether it
/// changed. String content never lands here: [`string_content_as_block`]
/// widens it first, so the block count stays comparable across a
/// breakpoint move.
fn message_shape(msg: &serde_json::Value) -> MessageShape {
    let mut block_hashes = [None; EARLY_BLOCKS_WINDOW];
    let blocks = match msg.get("content") {
        Some(serde_json::Value::Array(arr)) => {
            for (i, block) in arr.iter().take(EARLY_BLOCKS_WINDOW).enumerate() {
                let full = hash_value(&canonicalize_for_hash(block, false));
                let mut short = [0u8; 8];
                short.copy_from_slice(&full[..8]);
                block_hashes[i] = Some(short);
            }
            Some(arr.len())
        }
        _ => None,
    };
    MessageShape {
        blocks,
        block_hashes,
    }
}

/// SHA-256 over `serde_json::to_vec(value)`. Re-serializing the
/// borrowed `Value` defends against trivial whitespace differences
/// from the wire — operators care about *semantic* drift, not
/// formatter drift.
fn hash_value(value: &serde_json::Value) -> [u8; 32] {
    // `serde_json::to_vec` on a `Value` cannot fail except on a
    // pathological recursion, which the upstream API would itself
    // reject; on the impossible failure path we hash the empty byte
    // string so the digest is still stable rather than panicking and
    // taking the request down.
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Bounded session → last-seen `StructuralHash` map. Wrapped in
/// `Arc<Mutex<…>>` so it can be cloned freely into `AppState` without
/// duplicating the underlying LRU.
#[derive(Clone)]
pub struct DriftState {
    cache: Arc<Mutex<LruCache<String, StructuralHash>>>,
}

impl DriftState {
    /// Build a new `DriftState` bounded to `capacity` sessions. The
    /// production capacity is 1000; tests pass small values so the
    /// LRU eviction path is exercised cheaply.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0`. The detector is meaningless without
    /// at least one slot — use `LruCache::new(NonZeroUsize::MIN)` if
    /// you need a "remember nothing" mode.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("DriftState capacity must be > 0");
        Self {
            cache: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }
}

impl std::fmt::Debug for DriftState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.cache.lock().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("DriftState").field("len", &len).finish()
    }
}

/// Compare `current` against the last-seen hash for `session_key` and
/// emit a structured `tracing` event accordingly. Always updates the
/// LRU to `current` before returning so the next call sees the most
/// recent fingerprint.
///
/// Logging contract:
///
/// - First time a session is seen → `tracing::info!(event =
///   "cache_drift_first_request", …)` with a 16-char prefix of the
///   SHA-256 hex of `session_key`.
/// - Subsequent requests with no drifted dimension (append-only
///   growth into the early window included) → no event.
/// - Subsequent requests with any dimension drifting →
///   `tracing::warn!(event = "cache_drift_observed", drift_dims =
///   "<comma-joined>", early_drift = "<slot:what>", previous_hash_prefix,
///   current_hash_prefix, …)`. `early_drift` is empty unless the early
///   window is one of the drifted dimensions.
pub fn observe_drift(
    state: &DriftState,
    session_key: &str,
    current: StructuralHash,
) -> Option<String> {
    let session_prefix = session_key_log_prefix(session_key);
    let mut cache = match state.cache.lock() {
        Ok(c) => c,
        Err(poisoned) => {
            // Mutex was poisoned by a panicking writer in another
            // task. Recover the inner data — the only thing we lose
            // is one stale entry, and continuing the request is
            // strictly preferable to failing closed.
            tracing::warn!(
                event = "cache_drift_state_mutex_poisoned",
                "drift detector mutex was poisoned by a panicking task; recovering"
            );
            poisoned.into_inner()
        }
    };
    match cache.get(session_key).copied() {
        None => {
            tracing::info!(
                event = "cache_drift_first_request",
                session_key_hash = %session_prefix,
                current_hash_prefix = %structural_hash_log_prefix(&current),
                "cache_drift detector observed a new session"
            );
            cache.put(session_key.to_string(), current);
            None
        }
        Some(previous) => {
            let dims = drift_dims(&previous, &current);
            if dims.is_empty() {
                // Stable (append-only growth included). No event.
                // Update LRU recency by reinserting.
                cache.put(session_key.to_string(), current);
                None
            } else {
                tracing::warn!(
                    event = "cache_drift_observed",
                    session_key_hash = %session_prefix,
                    drift_dims = %dims,
                    // Empty unless the early window moved; the other two
                    // axes are single values with nothing to break down.
                    early_drift = %early_drift_detail(&previous, &current),
                    previous_hash_prefix = %structural_hash_log_prefix(&previous),
                    current_hash_prefix = %structural_hash_log_prefix(&current),
                    "cache_drift detector observed structural change between turns of the same session"
                );
                cache.put(session_key.to_string(), current);
                Some(dims)
            }
        }
    }
}

/// 16-char hex prefix of SHA-256(session_key). Bounds the log line
/// width and never reveals the raw key (which may be a bearer token
/// or API key — see `derive_session_key`). `pub(crate)` so the
/// beta-sticky merge site logs the same session identity the same way.
pub(crate) fn session_key_log_prefix(session_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    let digest = hasher.finalize();
    hex_prefix(&digest, 16)
}

/// 24-char hex prefix (12 bytes) of a digest over the concatenated
/// axis hashes. Useful as a compact "did the prefix change" indicator
/// in logs without printing every axis digest in full.
pub fn structural_hash_log_prefix(hash: &StructuralHash) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash.system);
    hasher.update(hash.tools);
    for slot in &hash.early_messages {
        // Length-prefix the slots so `[Some(h), None]` and `[None,
        // Some(h)]` cannot collide.
        match slot {
            Some(h) => {
                hasher.update([1u8]);
                hasher.update(h);
            }
            None => hasher.update([0u8]),
        }
    }
    let digest = hasher.finalize();
    hex_prefix(&digest, 12)
}

/// Lowercase hex of the first `take` bytes of `bytes`. Allocates a
/// `String` once per call.
fn hex_prefix(bytes: &[u8], take: usize) -> String {
    let take = take.min(bytes.len());
    let mut out = String::with_capacity(take * 2);
    for b in &bytes[..take] {
        // Manual hex; avoids pulling `hex` for one call site.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Comma-joined list of which dimensions drifted between `prev` and
/// `curr`. The order is fixed (`system`, `tools`, `early_messages`)
/// so log queries can match deterministically.
fn drift_dims(prev: &StructuralHash, curr: &StructuralHash) -> String {
    let mut dims: Vec<&'static str> = Vec::with_capacity(3);
    if prev.system != curr.system {
        dims.push("system");
    }
    if prev.tools != curr.tools {
        dims.push("tools");
    }
    if early_window_drifted(&prev.early_messages, &curr.early_messages) {
        dims.push("early_messages");
    }
    dims.join(",")
}

/// Prefix-aware early-window comparison. A settled slot changing or
/// disappearing is drift (the previously sent prefix was rewritten —
/// the provider's cache is busted); the conversation growing into a
/// previously empty slot is append-only and benign.
fn early_window_drifted(
    prev: &[Option<[u8; 32]>; EARLY_MESSAGES_WINDOW],
    curr: &[Option<[u8; 32]>; EARLY_MESSAGES_WINDOW],
) -> bool {
    prev.iter().zip(curr.iter()).any(|slots| match slots {
        (Some(p), Some(c)) => p != c,
        (Some(_), None) => true,
        (None, _) => false,
    })
}

/// Name what changed inside the early window, slot by slot.
///
/// `early_messages` on its own says the front of the conversation was
/// rewritten but not by whom, which is where the investigation stalled:
/// 830 findings in a day, all reading `early_messages`, none pointing
/// at a component. This turns each drifted slot into one of:
///
/// - `1:blocks 3->4` — a block was inserted or removed. Someone is
///   adding to a settled message; the count is the tell.
/// - `1:block[0]` — block 0 was rewritten in place. Note *which*:
///   block 0 means whatever writes the head of the message, a later
///   index means a rewriter working further in.
/// - `1:block[>7]` — changed past [`EARLY_BLOCKS_WINDOW`], so no index.
/// - `1:string` — a plain string body changed; no blocks to index.
/// - `1:dropped` — the slot is gone; the conversation was truncated or
///   replayed shorter than it was sent.
///
/// Structure only. No message text reaches the log — the point is to
/// name the component, and the shape does that without carrying user
/// content into an operator's terminal.
fn early_drift_detail(prev: &StructuralHash, curr: &StructuralHash) -> String {
    let mut parts: Vec<String> = Vec::new();
    for slot in 0..EARLY_MESSAGES_WINDOW {
        let (Some(p), c) = (prev.early_messages[slot], curr.early_messages[slot]) else {
            // Growth into a previously empty slot is append-only.
            continue;
        };
        let Some(c) = c else {
            parts.push(format!("{slot}:dropped"));
            continue;
        };
        if p == c {
            continue;
        }
        parts.push(match (prev.early_shapes[slot], curr.early_shapes[slot]) {
            (Some(ps), Some(cs)) => format!("{slot}:{}", shape_delta(&ps, &cs)),
            // Shapes travel with the hashes, so this is unreachable in
            // practice; report the slot rather than inventing detail.
            _ => format!("{slot}:changed"),
        });
    }
    parts.join(",")
}

/// The block-level difference between two shapes of the same slot.
fn shape_delta(prev: &MessageShape, curr: &MessageShape) -> String {
    match (prev.blocks, curr.blocks) {
        (Some(p), Some(c)) if p != c => format!("blocks {p}->{c}"),
        (Some(_), Some(_)) => {
            let changed: Vec<String> = prev
                .block_hashes
                .iter()
                .zip(curr.block_hashes.iter())
                .enumerate()
                .filter(|(_, (p, c))| p != c)
                .map(|(i, _)| i.to_string())
                .collect();
            if changed.is_empty() {
                // Same count, every hashed block identical: the change
                // sits past the window we fingerprint.
                format!("block[>{}]", EARLY_BLOCKS_WINDOW - 1)
            } else {
                format!("block[{}]", changed.join(","))
            }
        }
        // One side had blocks and the other a bare string, or both are
        // strings — either way there is no index to give.
        _ => "string".to_string(),
    }
}

/// Derive a stable per-session key from the request headers, client
/// address, and body. Priority order:
///
/// 1. `x-headroom-session-id` header (hashed) — the explicit opt-in
///    the Python proxy already honors for every session-sticky
///    subsystem (prefix tracker, beta-header tracker). When the
///    client declares its session, believe it.
/// 2. `Authorization` header (hashed; never logged raw).
/// 3. `x-api-key` header (hashed; never logged raw).
/// 4. Client IP address.
/// 5. `(client_ip, user_agent)` synthetic tuple — the user-agent
///    bucketization gives us *some* discrimination when many
///    anonymous clients sit behind the same NAT.
///
/// Arms 2–5 identify a *tenant*, not a conversation: interactive
/// clients (Claude Code, Codex CLI) send the same bearer for every
/// concurrent conversation. Each of those arms therefore also folds in
/// [`conversation_discriminator`] — a fingerprint of the
/// conversation's first message — so parallel conversations do not
/// alternate over one LRU slot and log false `cache_drift_observed`
/// events on every switch.
///
/// The returned string is opaque; never log it directly. Callers
/// should pass it straight to [`observe_drift`], which logs only a
/// hashed prefix.
pub fn derive_session_key(
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    body: &serde_json::Value,
    kind: ApiKind,
) -> String {
    if let Some(sid) = headers
        .get("x-headroom-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        return format!("session:{}", hash_secret(sid));
    }
    let conv = conversation_discriminator(body, kind);
    if let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        return format!("auth:{}:{conv}", hash_secret(token));
    }
    // `x-api-key` is the Anthropic/OpenAI-Responses convention.
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return format!("apikey:{}:{conv}", hash_secret(key));
    }
    let ip = client_addr.ip().to_string();
    if let Some(ua) = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
    {
        // Hash the (ip, ua) tuple so the resulting key remains opaque
        // and does not leak full UA strings into downstream logs that
        // forget our "log only the prefix" contract.
        let mut h = DefaultHasher::new();
        ip.hash(&mut h);
        ua.hash(&mut h);
        return format!("ipua:{:016x}:{conv}", h.finish());
    }
    format!("ip:{ip}:{conv}")
}

/// 16-hex-char fingerprint of `(model, first conversation message)`,
/// the message canonicalized via [`canonicalize_for_hash`] so a
/// relocated `cache_control` marker does not rotate the conversation's
/// identity between turns (interactive clients resend the history each
/// turn with the opener otherwise byte-stable). `-` when the body
/// carries no conversation messages.
///
/// The model is folded in because provider prompt caches are
/// per-model: an auxiliary small-model call that reuses a
/// conversation's opener (title generation, summarization sidecars)
/// must not share the conversation's drift baseline, and a
/// mid-conversation model switch genuinely starts a new provider
/// cache lineage.
///
/// Deliberately excludes the system prompt and tools: those are the
/// *measured* axes, and agentic clients legitimately mutate them
/// mid-conversation. An identity built from mutating content would
/// rotate exactly when the detector should be reporting drift instead.
///
/// Known trade-offs: a client that *rewrites* its first message
/// (history compaction, rolling-window truncation, Responses chained
/// mode sending delta-only `input`) re-keys to a fresh session — the
/// rewrite surfaces as `cache_drift_first_request` on the new key
/// rather than `cache_drift_observed` against the old baseline. An
/// explicit `x-headroom-session-id` pins the identity and reports
/// those rewrites as drift. Conversations sharing one credential AND
/// a byte-identical opener on the same model still conflate.
fn conversation_discriminator(body: &serde_json::Value, kind: ApiKind) -> String {
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("");
    match conversation_messages(body, kind).first() {
        Some(first) => {
            let canonical = canonicalize_for_hash(first, false);
            let mut hasher = Sha256::new();
            hasher.update(model.as_bytes());
            // NUL separator: domain-separate the model from the
            // message bytes so no (model, message) pair can alias
            // another by shifting bytes across the boundary.
            hasher.update([0u8]);
            hasher.update(serde_json::to_vec(&canonical).unwrap_or_default());
            let digest = hasher.finalize();
            hex_prefix(&digest, 8)
        }
        None => "-".to_string(),
    }
}

/// SHA-256 of `secret`, truncated to 16 hex characters. Sufficient
/// to discriminate sessions while pinning that the raw secret never
/// reaches the log line. We do **not** use the full digest because
/// even a hashed bearer that ends up in many log entries leaks
/// fingerprintable information; the 16-char prefix bounds that.
fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    hex_prefix(&digest, 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::{IpAddr, Ipv4Addr};

    fn anthropic_body(
        system: &str,
        tools: serde_json::Value,
        msgs: Vec<&str>,
    ) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = msgs
            .into_iter()
            .map(|t| json!({"role": "user", "content": t}))
            .collect();
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": system,
            "tools": tools,
            "messages": messages,
        })
    }

    fn make_state() -> DriftState {
        DriftState::new(8)
    }

    /// One user message whose content is an explicit block list, so the
    /// block-level attribution has something to index.
    fn blocks_body(blocks: serde_json::Value) -> serde_json::Value {
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "you are an assistant",
            "tools": [],
            "messages": [{"role": "user", "content": blocks}],
        })
    }

    fn detail(prev: &serde_json::Value, curr: &serde_json::Value) -> String {
        early_drift_detail(
            &compute_structural_hash(prev, ApiKind::Anthropic),
            &compute_structural_hash(curr, ApiKind::Anthropic),
        )
    }

    /// The case the whole feature exists for: something prepends a
    /// block to a settled message. Before this, that read as a bare
    /// `early_messages` and named nobody.
    #[test]
    fn an_inserted_block_reports_the_count_move() {
        let before = blocks_body(json!([{"type": "text", "text": "hello"}]));
        let after = blocks_body(json!([
            {"type": "text", "text": "injected"},
            {"type": "text", "text": "hello"},
        ]));
        assert_eq!(detail(&before, &after), "0:blocks 1->2");
    }

    #[test]
    fn a_rewritten_block_reports_its_index() {
        let before = blocks_body(json!([
            {"type": "text", "text": "stable"},
            {"type": "text", "text": "original"},
        ]));
        let after = blocks_body(json!([
            {"type": "text", "text": "stable"},
            {"type": "text", "text": "rewritten"},
        ]));
        assert_eq!(detail(&before, &after), "0:block[1]");
    }

    /// Claude Code relocates its cache breakpoint to the newest block
    /// every turn. That is placement, not content — if it registered as
    /// a rewritten block the field would be noise on every request.
    #[test]
    fn a_relocated_breakpoint_is_not_a_block_change() {
        let before = blocks_body(json!([
            {"type": "text", "text": "one", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "two"},
        ]));
        let after = blocks_body(json!([
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}},
        ]));
        assert_eq!(detail(&before, &after), "", "no drift, so nothing to name");
    }

    /// A string body used to report `string` — no index, nothing named.
    /// It reads as one text block now, which costs nothing and says
    /// where the change landed.
    #[test]
    fn a_string_body_reads_as_the_one_block_it_is() {
        let before = anthropic_body("s", json!([]), vec!["first"]);
        let after = anthropic_body("s", json!([]), vec!["second"]);
        assert_eq!(detail(&before, &after), "0:block[0]");
    }

    #[test]
    fn a_lost_slot_reads_as_dropped() {
        let before = anthropic_body("s", json!([]), vec!["a", "b"]);
        let after = anthropic_body("s", json!([]), vec!["a"]);
        assert_eq!(detail(&before, &after), "1:dropped");
    }

    /// Growth is append-only and benign — the drift predicate already
    /// says so, and the detail must agree rather than blaming a slot.
    #[test]
    fn growing_into_the_window_names_nothing() {
        let before = anthropic_body("s", json!([]), vec!["a"]);
        let after = anthropic_body("s", json!([]), vec!["a", "b"]);
        assert_eq!(detail(&before, &after), "");
    }

    /// Beyond the fingerprinted blocks there is no index to report, but
    /// the slot must still be named — silence there would look like the
    /// stable case.
    #[test]
    fn a_change_past_the_block_window_still_names_the_slot() {
        let mut before: Vec<serde_json::Value> = (0..EARLY_BLOCKS_WINDOW + 2)
            .map(|i| json!({"type": "text", "text": format!("block {i}")}))
            .collect();
        let mut after = before.clone();
        let last = after.len() - 1;
        after[last] = json!({"type": "text", "text": "changed"});
        before.shrink_to_fit();
        after.shrink_to_fit();
        assert_eq!(
            detail(&blocks_body(json!(before)), &blocks_body(json!(after))),
            format!("0:block[>{}]", EARLY_BLOCKS_WINDOW - 1)
        );
    }

    #[test]
    fn first_request_emits_first_request_event() {
        let state = make_state();
        let body = anthropic_body("you are an assistant", json!([]), vec!["hi"]);
        let h = compute_structural_hash(&body, ApiKind::Anthropic);
        // Before observation: empty cache.
        assert_eq!(state.cache.lock().unwrap().len(), 0);
        observe_drift(&state, "session-A", h);
        // After observation: 1 entry, equal to the input hash.
        let cache = state.cache.lock().unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.peek("session-A"), Some(&h));
    }

    #[test]
    fn same_hash_emits_no_event() {
        let state = make_state();
        let body = anthropic_body("sys-A", json!([]), vec!["m1"]);
        let h = compute_structural_hash(&body, ApiKind::Anthropic);
        observe_drift(&state, "sess", h);
        // Second observation with identical hash: still 1 entry, same hash.
        observe_drift(&state, "sess", h);
        let cache = state.cache.lock().unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.peek("sess"), Some(&h));
    }

    #[test]
    fn system_drift_detected_with_correct_dim() {
        let state = make_state();
        let h1 = compute_structural_hash(
            &anthropic_body("sys-A", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys-B", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.tools, h2.tools);
        assert_eq!(h1.early_messages, h2.early_messages);
        assert_eq!(drift_dims(&h1, &h2), "system");
        observe_drift(&state, "sess", h1);
        observe_drift(&state, "sess", h2);
    }

    #[test]
    fn tools_drift_detected_with_correct_dim() {
        let h1 = compute_structural_hash(
            &anthropic_body("sys", json!([{"name": "a"}]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys", json!([{"name": "b"}]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        assert_eq!(h1.system, h2.system);
        assert_ne!(h1.tools, h2.tools);
        assert_eq!(h1.early_messages, h2.early_messages);
        assert_eq!(drift_dims(&h1, &h2), "tools");
    }

    #[test]
    fn early_messages_drift_detected_with_correct_dim() {
        let h1 = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["DIFFERENT"]),
            ApiKind::Anthropic,
        );
        assert_eq!(h1.system, h2.system);
        assert_eq!(h1.tools, h2.tools);
        assert_ne!(h1.early_messages, h2.early_messages);
        assert_eq!(drift_dims(&h1, &h2), "early_messages");
    }

    #[test]
    fn multi_dim_drift_lists_all_changed_dims() {
        let h1 = compute_structural_hash(
            &anthropic_body("sys-A", json!([{"name": "a"}]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys-B", json!([{"name": "b"}]), vec!["X"]),
            ApiKind::Anthropic,
        );
        assert_eq!(drift_dims(&h1, &h2), "system,tools,early_messages");
    }

    #[test]
    fn lru_evicts_at_capacity() {
        // Capacity 2: inserting a 3rd session evicts the LRU.
        let state = DriftState::new(2);
        let h = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m"]),
            ApiKind::Anthropic,
        );
        observe_drift(&state, "s1", h);
        observe_drift(&state, "s2", h);
        observe_drift(&state, "s3", h);
        let cache = state.cache.lock().unwrap();
        assert_eq!(cache.len(), 2);
        // s1 was the least-recently-used; should have been evicted.
        assert!(!cache.contains("s1"));
        assert!(cache.contains("s2"));
        assert!(cache.contains("s3"));
    }

    #[test]
    fn does_not_mutate_input() {
        let body = anthropic_body(
            "sys",
            json!([{"name": "t1", "input_schema": {"type": "object"}}]),
            vec!["m1", "m2", "m3", "m4"],
        );
        let original_bytes = serde_json::to_vec(&body).expect("serialize");
        // Compute the hash twice — across the three ApiKind shapes —
        // to exercise every branch that *could* mutate the input.
        let _ = compute_structural_hash(&body, ApiKind::Anthropic);
        let _ = compute_structural_hash(&body, ApiKind::OpenAiChat);
        let _ = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let after_bytes = serde_json::to_vec(&body).expect("re-serialize");
        assert_eq!(original_bytes, after_bytes);
    }

    #[test]
    fn session_key_hashes_authorization_does_not_log_raw() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer sk-ant-very-secret-token-do-not-log-me"
                .parse()
                .unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 1234);
        let key = derive_session_key(&headers, &addr, &json!({}), ApiKind::Anthropic);
        // The key MUST NOT contain the raw bearer string anywhere —
        // not the secret token, not the literal "Bearer", not even
        // any 8+ char substring of the secret.
        assert!(
            !key.contains("sk-ant"),
            "session key leaked raw secret prefix: {key}"
        );
        assert!(
            !key.contains("very-secret"),
            "session key leaked raw secret middle: {key}"
        );
        assert!(
            !key.contains("Bearer"),
            "session key leaked the auth scheme: {key}"
        );
        // The key SHOULD be the auth-scoped envelope, so we know the
        // `Authorization` arm was taken (not the IP fallback).
        assert!(key.starts_with("auth:"), "expected auth-scoped key: {key}");
        // And the log prefix must also not leak the raw secret.
        let log_prefix = session_key_log_prefix(&key);
        assert!(!log_prefix.contains("sk-ant"));
        assert!(!log_prefix.contains("very-secret"));
        assert!(!log_prefix.contains("Bearer"));
        assert_eq!(log_prefix.len(), 32); // 16 bytes × 2 hex chars
    }

    #[test]
    fn session_key_hashes_x_api_key_does_not_log_raw() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            "sk-very-private-api-key-12345".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 1234);
        let key = derive_session_key(&headers, &addr, &json!({}), ApiKind::Anthropic);
        assert!(!key.contains("sk-very-private"));
        assert!(key.starts_with("apikey:"));
    }

    #[test]
    fn session_key_falls_back_to_ip_then_ip_ua() {
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 5555);
        // No headers → ip-only.
        let bare = derive_session_key(&HeaderMap::new(), &addr, &json!({}), ApiKind::Anthropic);
        assert!(bare.starts_with("ip:"));
        // With UA → ipua-tuple.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::USER_AGENT, "ua-test".parse().unwrap());
        let with_ua = derive_session_key(&headers, &addr, &json!({}), ApiKind::Anthropic);
        assert!(with_ua.starts_with("ipua:"));
        assert_ne!(bare, with_ua);
    }

    #[test]
    fn openai_chat_extracts_first_system_message() {
        let body = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "you are a helpful assistant"},
                {"role": "user", "content": "hi"},
            ],
            "tools": [],
        });
        let h1 = compute_structural_hash(&body, ApiKind::OpenAiChat);
        // Same body but a different system message → system axis drifts.
        let body2 = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "you are a different assistant"},
                {"role": "user", "content": "hi"},
            ],
            "tools": [],
        });
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiChat);
        assert_ne!(h1.system, h2.system);
        // user message identical → early-messages stays identical.
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    #[test]
    fn openai_responses_uses_instructions_and_input() {
        let body = json!({
            "model": "gpt-4",
            "instructions": "be brief",
            "tools": [],
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
            ],
        });
        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let body2 = json!({
            "model": "gpt-4",
            "instructions": "be verbose",
            "tools": [],
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
            ],
        });
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);
        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    /// Shape observed from live Claude Code traffic: turn 1 sends a single
    /// user message whose *last* block carries the `cache_control` marker;
    /// on turn 2 the same message returns byte-identical except the marker
    /// moved to the newest message's first block.
    fn cc_turn1_body() -> serde_json::Value {
        json!({
            "model": "claude-haiku-4-5",
            "system": [{"type": "text", "text": "agent preamble"}],
            "tools": [{"name": "bash"}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "opening prompt"},
                    {"type": "text", "text": "project context", "cache_control": {"type": "ephemeral"}},
                ]},
            ],
        })
    }

    fn cc_turn2_body() -> serde_json::Value {
        json!({
            "model": "claude-haiku-4-5",
            "system": [{"type": "text", "text": "agent preamble"}],
            "tools": [{"name": "bash"}],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "opening prompt"},
                    {"type": "text", "text": "project context"},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "second prompt", "cache_control": {"type": "ephemeral"}},
                ]},
            ],
        })
    }

    #[test]
    fn different_conversations_on_one_credential_get_distinct_session_keys() {
        // Interactive clients (Claude Code, Codex CLI) send the same
        // `Authorization` bearer for every concurrent conversation. If the
        // session key stops at the credential, their alternating requests
        // ping-pong one LRU slot and every switch logs a false
        // `cache_drift_observed`.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        let conv_a = anthropic_body("sys", json!([]), vec!["conversation A opener"]);
        let conv_b = anthropic_body("sys", json!([]), vec!["conversation B opener"]);
        let key_a = derive_session_key(&headers, &addr, &conv_a, ApiKind::Anthropic);
        let key_b = derive_session_key(&headers, &addr, &conv_b, ApiKind::Anthropic);
        assert_ne!(
            key_a, key_b,
            "two conversations sharing one credential must not share a drift session"
        );
    }

    #[test]
    fn same_conversation_next_turn_keeps_its_session_key() {
        // The discriminator must survive normal turn-to-turn growth: the
        // opener is byte-identical on turn 2 except its relocated
        // `cache_control` marker.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        let key_t1 = derive_session_key(&headers, &addr, &cc_turn1_body(), ApiKind::Anthropic);
        let key_t2 = derive_session_key(&headers, &addr, &cc_turn2_body(), ApiKind::Anthropic);
        assert_eq!(
            key_t1, key_t2,
            "turn growth and cache_control relocation must not rotate the session key"
        );
    }

    #[test]
    fn explicit_headroom_session_header_wins_over_credentials() {
        // The Python proxy honors `x-headroom-session-id` as the highest-
        // priority session identity (prefix tracker, beta-header tracker);
        // the drift detector must respect the same explicit opt-in.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        headers.insert("x-headroom-session-id", "conv-42".parse().unwrap());
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        let body_a = anthropic_body("sys", json!([]), vec!["conversation A opener"]);
        let body_b = anthropic_body("sys", json!([]), vec!["conversation B opener"]);
        let key_a = derive_session_key(&headers, &addr, &body_a, ApiKind::Anthropic);
        assert!(
            key_a.starts_with("session:"),
            "explicit session header must define the key, got: {key_a}"
        );
        assert!(
            !key_a.contains("conv-42"),
            "session keys stay opaque even for non-secret ids: {key_a}"
        );
        // The explicit id pins the session across body variance…
        let key_b = derive_session_key(&headers, &addr, &body_b, ApiKind::Anthropic);
        assert_eq!(key_a, key_b);
        // …and different ids mean different sessions.
        let mut headers2 = headers.clone();
        headers2.insert("x-headroom-session-id", "conv-43".parse().unwrap());
        let key_c = derive_session_key(&headers2, &addr, &body_a, ApiKind::Anthropic);
        assert_ne!(key_a, key_c);
    }

    #[test]
    fn cache_control_relocation_and_growth_are_not_drift() {
        // Turn 1 → turn 2 of a single conversation: the early window gains
        // messages and the client relocates its `cache_control` marker to
        // the newest block. Neither busts the provider's prefix cache, so
        // neither is drift.
        let h1 = compute_structural_hash(&cc_turn1_body(), ApiKind::Anthropic);
        let h2 = compute_structural_hash(&cc_turn2_body(), ApiKind::Anthropic);
        assert_eq!(
            drift_dims(&h1, &h2),
            "",
            "append-only growth with marker relocation must not be drift"
        );
    }

    #[test]
    fn a_breakpoint_arriving_on_a_settled_message_is_not_drift() {
        // Claude Code sends a message as a bare string, then wraps the
        // same text in a block so it can hang a breakpoint on it. Same
        // text, same billed tokens, same prefix — the wrapper is how the
        // marker attaches, not a rewrite.
        let plain = json!({"model": "m", "system": "s", "tools": [], "messages": [
            {"role": "user", "content": "settled turn"},
        ]});
        let wrapped = json!({"model": "m", "system": "s", "tools": [], "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "settled turn",
                 "cache_control": {"type": "ephemeral"}},
            ]},
        ]});
        let h1 = compute_structural_hash(&plain, ApiKind::Anthropic);
        let h2 = compute_structural_hash(&wrapped, ApiKind::Anthropic);
        assert_eq!(
            drift_dims(&h1, &h2),
            "",
            "wrapping text to carry a breakpoint must not read as a rewrite"
        );
        // And back again when the marker moves on.
        assert_eq!(drift_dims(&h2, &h1), "");
    }

    #[test]
    fn a_rewrite_under_a_breakpoint_is_still_drift() {
        // The collapse must not blind the detector: same wrapper, new
        // text, and that is a genuine bust.
        let before = json!({"model": "m", "system": "s", "tools": [], "messages": [
            {"role": "user", "content": "settled turn"},
        ]});
        let after = json!({"model": "m", "system": "s", "tools": [], "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "REWRITTEN",
                 "cache_control": {"type": "ephemeral"}},
            ]},
        ]});
        let h1 = compute_structural_hash(&before, ApiKind::Anthropic);
        let h2 = compute_structural_hash(&after, ApiKind::Anthropic);
        assert_eq!(drift_dims(&h1, &h2), "early_messages");
    }

    #[test]
    fn a_dropped_reminder_block_is_drift_and_names_the_slot() {
        // The expensive case, measured live on 2026-08-11: the client
        // deletes a <system-reminder> block from settled history and
        // every cached token after it is rebilled. Two blocks never
        // collapse, so this keeps its structure and its detail.
        let with_reminder = json!({"model": "m", "system": "s", "tools": [], "messages": [
            {"role": "user", "content": [{"type": "text", "text": "opener"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "real turn"},
                {"type": "text", "text": "<system-reminder>hook output</system-reminder>"},
            ]},
        ]});
        let without = json!({"model": "m", "system": "s", "tools": [], "messages": [
            {"role": "user", "content": [{"type": "text", "text": "opener"}]},
            {"role": "user", "content": [{"type": "text", "text": "real turn"}]},
        ]});
        let h1 = compute_structural_hash(&with_reminder, ApiKind::Anthropic);
        let h2 = compute_structural_hash(&without, ApiKind::Anthropic);
        assert_eq!(drift_dims(&h1, &h2), "early_messages");
        assert_eq!(
            early_drift_detail(&h1, &h2),
            "1:blocks 2->1",
            "the detail must name the slot and the block count that moved"
        );
    }

    #[test]
    fn append_only_growth_without_markers_is_not_drift() {
        let h1 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m1", "m2", "m3"]),
            ApiKind::Anthropic,
        );
        assert_eq!(
            drift_dims(&h1, &h2),
            "",
            "a conversation growing into the early window must not be drift"
        );
    }

    #[test]
    fn rewritten_early_message_is_still_drift() {
        let h1 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m1", "m2", "m3"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["REWRITTEN", "m2", "m3"]),
            ApiKind::Anthropic,
        );
        assert_eq!(drift_dims(&h1, &h2), "early_messages");
    }

    #[test]
    fn shrunk_history_is_still_drift() {
        // Fewer messages than previously observed *under the same
        // session key* means the settled prefix was rewritten in place
        // — that IS a cache bust. (Reachable when the identity is
        // pinned, e.g. an explicit x-headroom-session-id; on the
        // credential arms a first-message rewrite re-keys instead —
        // see conversation_discriminator's trade-off note.)
        let h1 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m1", "m2", "m3"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        assert_eq!(drift_dims(&h1, &h2), "early_messages");
    }

    #[test]
    fn same_opener_on_different_model_gets_distinct_session_keys() {
        // Auxiliary small-model calls (title generation, summaries)
        // reuse a conversation's opener under the same credential.
        // Provider prompt caches are per-model, so these are separate
        // cache lineages and must not share a drift baseline — the
        // sidecar's different system prompt would otherwise false-warn.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        let mut main_conv = anthropic_body("sys", json!([]), vec!["shared opener"]);
        let mut sidecar = anthropic_body("sys", json!([]), vec!["shared opener"]);
        main_conv["model"] = json!("opus-large");
        sidecar["model"] = json!("haiku-small");
        let key_main = derive_session_key(&headers, &addr, &main_conv, ApiKind::Anthropic);
        let key_side = derive_session_key(&headers, &addr, &sidecar, ApiKind::Anthropic);
        assert_ne!(key_main, key_side);
    }

    #[test]
    fn key_order_variation_does_not_perturb_hashes_or_identity() {
        // The workspace's serde_json enables `preserve_order`, so two
        // serializations of the same message with different key order
        // stay distinct through Value round-trips. Canonicalization
        // must neutralize that for both the axes and the session key.
        let body_a: serde_json::Value = serde_json::from_str(
            r#"{"model":"m","system":"s","tools":[],
                "messages":[{"role":"user","content":"hello"}]}"#,
        )
        .unwrap();
        let body_b: serde_json::Value = serde_json::from_str(
            r#"{"model":"m","system":"s","tools":[],
                "messages":[{"content":"hello","role":"user"}]}"#,
        )
        .unwrap();
        assert_eq!(
            compute_structural_hash(&body_a, ApiKind::Anthropic),
            compute_structural_hash(&body_b, ApiKind::Anthropic),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        assert_eq!(
            derive_session_key(&headers, &addr, &body_a, ApiKind::Anthropic),
            derive_session_key(&headers, &addr, &body_b, ApiKind::Anthropic),
        );
    }

    #[test]
    fn cache_control_named_fields_inside_opaque_payloads_still_count() {
        // A tool schema property (or tool-call argument) that happens
        // to be NAMED cache_control is user data, not a cache marker.
        // Changing it must still read as drift on the affected axis.
        let with_schema = |ty: &str| {
            json!({
                "model": "m", "system": "s",
                "tools": [{"name": "t", "input_schema":
                    {"properties": {"cache_control": {"type": ty}}}}],
                "messages": [{"role": "user", "content": "hi"}],
            })
        };
        let h1 = compute_structural_hash(&with_schema("string"), ApiKind::Anthropic);
        let h2 = compute_structural_hash(&with_schema("integer"), ApiKind::Anthropic);
        assert_eq!(drift_dims(&h1, &h2), "tools");

        let with_tool_input = |v: &str| {
            json!({
                "model": "m", "system": "s", "tools": [],
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "x", "name": "t",
                         "input": {"cache_control": v}},
                    ]},
                ],
            })
        };
        let h3 = compute_structural_hash(&with_tool_input("a"), ApiKind::Anthropic);
        let h4 = compute_structural_hash(&with_tool_input("b"), ApiKind::Anthropic);
        assert_eq!(drift_dims(&h3, &h4), "early_messages");
    }

    #[test]
    fn bare_string_messages_only_count_for_responses_input() {
        // `input: "text"` is valid Responses sugar; a bare-string
        // `messages` on the other shapes is malformed and contributes
        // no conversation identity.
        let responses = json!({"model": "m", "instructions": "i", "input": "hello"});
        assert_eq!(
            conversation_messages(&responses, ApiKind::OpenAiResponses).len(),
            1
        );
        let malformed = json!({"model": "m", "system": "s", "messages": "oops"});
        assert!(conversation_messages(&malformed, ApiKind::Anthropic).is_empty());
        assert!(conversation_discriminator(&malformed, ApiKind::Anthropic) == "-");
    }

    #[test]
    fn openai_chat_discriminator_uses_first_non_system_message() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        let conv = |first_user: &str| {
            json!({
                "model": "gpt-4",
                "messages": [
                    {"role": "system", "content": "shared assistant config"},
                    {"role": "user", "content": first_user},
                ],
            })
        };
        let key_a = derive_session_key(&headers, &addr, &conv("opener A"), ApiKind::OpenAiChat);
        let key_b = derive_session_key(&headers, &addr, &conv("opener B"), ApiKind::OpenAiChat);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn openai_responses_discriminator_uses_first_input_item() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer shared-workspace-token".parse().unwrap(),
        );
        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4242);
        let conv = |first_input: &str| {
            json!({
                "model": "gpt-4",
                "instructions": "shared instructions",
                "input": [{"type": "message", "role": "user", "content": first_input}],
            })
        };
        let key_a =
            derive_session_key(&headers, &addr, &conv("opener A"), ApiKind::OpenAiResponses);
        let key_b =
            derive_session_key(&headers, &addr, &conv("opener B"), ApiKind::OpenAiResponses);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn early_messages_window_caps_at_three() {
        // 5 messages: hash should depend only on the first 3.
        let h1 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["a", "b", "c", "d", "e"]),
            ApiKind::Anthropic,
        );
        // Mutating message 4 only must NOT drift the early_messages hash.
        let h2 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["a", "b", "c", "DIFFERENT", "e"]),
            ApiKind::Anthropic,
        );
        assert_eq!(h1.early_messages, h2.early_messages);
        // But mutating message 1 must drift it.
        let h3 = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["DIFFERENT", "b", "c", "d", "e"]),
            ApiKind::Anthropic,
        );
        assert_ne!(h1.early_messages, h3.early_messages);
    }
}
