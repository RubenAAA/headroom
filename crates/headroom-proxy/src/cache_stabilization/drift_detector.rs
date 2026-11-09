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
//! - One LRU lookup + insert. `lru` is O(1) amortised.
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

use super::ephemeral_spans::{
    block_carries_ephemeral_span, is_client_scaffolding_message, take_trailing_ephemeral_spans,
};

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
    /// Line-level breakdown of the same system block, kept so a drift
    /// event can say *what* changed inside it rather than only that it
    /// changed. Purely diagnostic — the drift decision reads
    /// [`StructuralHash::system`].
    pub system_shape: SystemShape,
    pub tools: [u8; 32],
    pub early_messages: [Option<[u8; 32]>; EARLY_MESSAGES_WINDOW],
    /// Block-level breakdown of the same early messages, kept so a
    /// drift event can say *what* changed inside a slot rather than
    /// only that the slot changed. Purely diagnostic — the drift
    /// decision reads [`StructuralHash::early_messages`].
    pub early_shapes: [Option<MessageShape>; EARLY_MESSAGES_WINDOW],
    /// The same window hashed the way it was before 2026-08-26: raw slots,
    /// reminder spans left in. Read for one purpose — counting how often the
    /// scaffolding fix stops an invalidation that would otherwise have
    /// happened — and never for the drift decision.
    ///
    /// This is a measurement scaffold with a job to finish. Once
    /// `early_scaffolding_absorbed` has run long enough to price the change,
    /// delete the field, `legacy_early_message_hashes`, and the branch in
    /// `observe` that reads them.
    pub early_messages_legacy: [Option<[u8; 32]>; EARLY_MESSAGES_WINDOW],
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

/// How many lines of the system block are fingerprinted individually.
///
/// The system block is one or two long text blocks, so block-level
/// attribution says only "the system prompt changed" — which is where
/// this investigation stalled: 8 findings in a day worth ~1M wasted
/// cache-read tokens, every one of them reading `system` and none
/// naming a component. Lines are the granularity that separates a
/// client's stable instructions from the handful of volatile lines
/// wedged among them.
///
/// 192 covers the Claude Code system prompt with room to spare; past
/// it the detail degrades to `line[>192]` rather than lying.
pub const SYSTEM_LINES_WINDOW: usize = 192;

/// What one early block *is*, alongside the hash of what it says.
///
/// A hash answers "did this change" and stops there. Three of the most
/// expensive drift events on record read `2:block[0]` — slot 2's first block
/// rewritten — and the bodies were long gone by the time anyone asked which
/// block that was. Carrying the type and the size costs eight bytes per block
/// and turns the next occurrence into an answer instead of an investigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockTag {
    /// The block's `type` field, mapped to a fixed vocabulary so the
    /// descriptor stays `Copy`. Anything unrecognised reads `other`.
    pub kind: BlockKind,
    /// Serialized length of the canonicalized block, saturating at `u32::MAX`.
    /// A size delta is what separates a truncation from a rewrite.
    pub bytes: u32,
}

/// The block types worth telling apart in a drift line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse,
    ToolResult,
    Image,
    Document,
    Other,
}

impl BlockKind {
    fn of(block: &serde_json::Value) -> Self {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => Self::Text,
            Some("thinking") => Self::Thinking,
            Some("redacted_thinking") => Self::RedactedThinking,
            Some("tool_use") => Self::ToolUse,
            Some("tool_result") => Self::ToolResult,
            Some("image") => Self::Image,
            Some("document") => Self::Document,
            _ => Self::Other,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::RedactedThinking => "redacted_thinking",
            Self::ToolUse => "tool_use",
            Self::ToolResult => "tool_result",
            Self::Image => "image",
            Self::Document => "document",
            Self::Other => "other",
        }
    }
}

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
/// Line-level fingerprint of the system block. Diagnostic only — the
/// drift decision reads [`StructuralHash::system`] and never this.
///
/// Held by value inside a `Copy` [`StructuralHash`], so every field is
/// a fixed-size array: a hash of 0 means "no such line".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemShape {
    /// Lines in the flattened system text, however many blocks carried
    /// them. Counted past [`SYSTEM_LINES_WINDOW`] so a change in the
    /// total is still reported when the detail is not.
    pub lines: usize,
    /// One hash per line, capped at [`SYSTEM_LINES_WINDOW`].
    pub line_hashes: [u32; SYSTEM_LINES_WINDOW],
    /// Byte length of each hashed line, saturating at `u16::MAX`.
    ///
    /// Read together with the hash this is the tell for *what kind* of
    /// thing moved: a line that changes hash while keeping its length
    /// is a fixed-width field — a commit SHA, a timestamp, a counter —
    /// whereas one that grows or shrinks is text arriving or leaving.
    pub line_lens: [u16; SYSTEM_LINES_WINDOW],
}

impl Default for SystemShape {
    fn default() -> Self {
        Self {
            lines: 0,
            line_hashes: [0; SYSTEM_LINES_WINDOW],
            line_lens: [0; SYSTEM_LINES_WINDOW],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageShape {
    /// Content blocks in the message, or `None` for a plain string
    /// body (which has no block structure to index into).
    pub blocks: Option<usize>,
    /// One hash per block, capped at [`EARLY_BLOCKS_WINDOW`].
    pub block_hashes: [Option<[u8; 8]>; EARLY_BLOCKS_WINDOW],
    /// What each hashed block is and how big it is. Diagnostic only —
    /// the drift decision never reads it.
    pub block_tags: [Option<BlockTag>; EARLY_BLOCKS_WINDOW],
}

/// Compute a [`StructuralHash`] for the body shape implied by `kind`.
///
/// `body` is borrowed; **this function never mutates it**. The
/// `does_not_mutate_input` test in the module below pins this with a
/// clone-and-compare assertion.
pub fn compute_structural_hash(body: &serde_json::Value, kind: ApiKind) -> StructuralHash {
    compute_structural_hash_with(body, kind, false)
}

/// [`compute_structural_hash`], leaving `defer_loading: true` tools out of
/// the `tools` axis when `deferred_tools_unkeyed` is set and `kind` is
/// Anthropic.
///
/// Set it only when those tools reach the provider still deferred
/// (first-party Anthropic). The API keeps deferred tools out of the prompt
/// it caches, and Claude Code appends each tool its ToolSearch loads still
/// marked deferred. Hashing them called every load drift, which dropped the
/// stored prefix and let offload rewrite history the provider still held —
/// a full re-write of the message tier (upstream 18a588b3f: 12% of cache
/// writes). Third-party upstreams get every tool eager, so a load there does
/// change the provider's key and must keep counting.
///
/// Whether any tool is deferred still counts: Claude Code's own cache-break
/// check names that flip ("defer_loading presence flipped") as a prompt
/// change.
pub fn compute_structural_hash_with(
    body: &serde_json::Value,
    kind: ApiKind,
    deferred_tools_unkeyed: bool,
) -> StructuralHash {
    let system_value = extract_system(body, kind);
    let system = hash_canonical(&system_value, false);
    let system_shape = system_shape(&system_value);
    let tools_value = if deferred_tools_unkeyed && matches!(kind, ApiKind::Anthropic) {
        without_deferred_tools(extract_tools(body))
    } else {
        extract_tools(body)
    };
    let tools = hash_canonical(&tools_value, false);
    let (early_messages, early_shapes) = early_message_hashes(body, kind);
    StructuralHash {
        system,
        system_shape,
        tools,
        early_messages,
        early_shapes,
        early_messages_legacy: legacy_early_message_hashes(body, kind),
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
///
/// Kept as the readable reference even though the hot path streams via
/// [`write_canonical`]: `streaming_canonical_matches_reference_tree`
/// pins the two byte-identical, so writer drift re-baselines sessions
/// loudly instead of silently.
#[allow(dead_code)]
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

/// Write the canonical form of `value` (same bytes
/// [`canonicalize_for_hash`] would serialize to) directly into `w,
/// without building the intermediate `Value` tree. Byte-identical to
/// `serde_json::to_vec(&canonicalize_for_hash(value, in_opaque))`:
/// sorted keys, `cache_control` dropped outside opaque payloads,
/// compact separators, strings escaped by `serde_json` itself.
fn write_canonical<W: std::io::Write>(
    w: &mut W,
    value: &serde_json::Value,
    in_opaque: bool,
) -> std::io::Result<()> {
    match value {
        serde_json::Value::Null => write!(w, "null"),
        serde_json::Value::Bool(b) => write!(w, "{b}"),
        serde_json::Value::Number(n) => write!(w, "{n}"),
        serde_json::Value::String(s) => {
            // `&String` serializes directly as a JSON string literal —
            // no clone, escaping handled by `serde_json` itself.
            serde_json::to_writer(&mut *w, s)?;
            Ok(())
        }
        serde_json::Value::Array(items) => {
            write!(w, "[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    write!(w, ",")?;
                }
                write_canonical(w, item, in_opaque)?;
            }
            write!(w, "]")
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map
                .keys()
                .filter(|k| in_opaque || k.as_str() != "cache_control")
                .collect();
            keys.sort();
            write!(w, "{{")?;
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    write!(w, ",")?;
                }
                serde_json::to_writer(&mut *w, &k)?;
                write!(w, ":")?;
                let opaque = in_opaque || OPAQUE_PAYLOAD_KEYS.contains(&k.as_str());
                write_canonical(w, &map[*k], opaque)?;
            }
            write!(w, "}}")
        }
    }
}

/// SHA-256 over the canonical bytes of `value`, streamed without the
/// intermediate tree or buffer (measured 1.6x on `system`, 1.44x on
/// `tools`). Digest-identical to `hash_value(&canonicalize_for_hash(..))`;
/// on the impossible writer failure path it hashes the empty byte
/// string, matching [`hash_value`]'s stability contract.
fn hash_canonical(value: &serde_json::Value, in_opaque: bool) -> [u8; 32] {
    struct DigestSink<'a>(&'a mut Sha256);
    impl std::io::Write for DigestSink<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hasher = Sha256::new();
    if write_canonical(&mut DigestSink(&mut hasher), value, in_opaque).is_err() {
        return Sha256::digest(b"").into();
    }
    hasher.finalize().into()
}

/// Canonical byte length of `value` without buffering the bytes.
/// Same number `serde_json::to_vec(&canonicalize_for_hash(..)).len()`
/// would report (measured 1.56x); used where only the size feeds a
/// tag, never the digest.
fn canonical_len(value: &serde_json::Value, in_opaque: bool) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = write_canonical(&mut counter, value, in_opaque);
    counter.0
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

/// The tools the provider keys its cache on: every `defer_loading: true`
/// entry dropped, plus one marker when any was present.
fn without_deferred_tools(tools: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Array(items) = tools else {
        return tools;
    };
    let deferred = |t: &serde_json::Value| {
        t.get("defer_loading").and_then(serde_json::Value::as_bool) == Some(true)
    };
    let any_deferred = items.iter().any(deferred);
    let mut kept: Vec<serde_json::Value> = items.into_iter().filter(|t| !deferred(t)).collect();
    if any_deferred {
        kept.push(serde_json::json!({"defer_loading": true}));
    }
    serde_json::Value::Array(kept)
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

/// [`early_message_hashes`] as it behaved before the scaffolding fix: every
/// message slotted by raw index, every reminder span left in.
///
/// Kept only so the fix can be counted against the behaviour it replaced.
fn legacy_early_message_hashes(
    body: &serde_json::Value,
    kind: ApiKind,
) -> [Option<[u8; 32]>; EARLY_MESSAGES_WINDOW] {
    let mut out = [None; EARLY_MESSAGES_WINDOW];
    for (slot, msg) in conversation_messages(body, kind)
        .into_iter()
        .take(EARLY_MESSAGES_WINDOW)
        .enumerate()
    {
        let msg = string_content_as_block(msg);
        out[slot] = Some(hash_canonical(&msg, false));
    }
    out
}

/// A message with the `<system-reminder>` spans trailing its blocks removed.
///
/// The message-level counterpart to skipping a scaffolding-only message: the
/// client also hangs a span off a message it already sent, which rewrites that
/// slot's hash without changing anything the conversation said.
fn without_trailing_spans(msg: &serde_json::Value) -> Cow<'_, serde_json::Value> {
    let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
        return Cow::Borrowed(msg);
    };
    if !blocks.iter().any(block_carries_ephemeral_span) {
        return Cow::Borrowed(msg);
    }
    let kept: Vec<serde_json::Value> = blocks
        .iter()
        .filter_map(|block| match take_trailing_ephemeral_spans(block) {
            // The block must not be touched: a span embedded in prose is prose.
            None => Some(block.clone()),
            Some((kept, _)) => kept,
        })
        .collect();
    let mut out = msg.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("content".to_string(), serde_json::Value::Array(kept));
    }
    Cow::Owned(out)
}

/// Hash each of the first [`EARLY_MESSAGES_WINDOW`] *substantive* conversation
/// messages individually (canonicalized). Slots the conversation has
/// not grown into yet stay `None`.
///
/// Ephemeral client scaffolding is skipped rather than slotted, and the spans
/// hanging off the messages that remain are stripped before hashing. Both
/// exist because this hash decides whether to drop the session's whole stored
/// prefix, and Claude Code's reminder churn lands squarely in this window: 80
/// of 87 stored prefixes on 2026-08-26 carried a reminder in messages 0-2, and
/// 34 held the withdrawable standalone `role: "system"` form at index 1.
///
/// Without the skip, one withdrawal shifts every later slot and the detector
/// reads a conversation that did not change as a rebuild boundary. Measured
/// 2026-08-23/26: 5 events, 430,296 tokens, worst turn 145,891 — every one of
/// them within 137s of the previous turn, so the provider's 1h cache was alive
/// and the invalidation, not the client, is what threw it away.
///
/// The judgment this overturns is real and was right when written: the note on
/// [`string_content_as_block`] calls a reminder deleted from settled history
/// "the real thing", on 2026-08-11 data. The relocation pass that normalised
/// these away for every subsystem was removed five days later, on 2026-08-16,
/// and `prefix_replay` alone was given the job. It runs after this hash, and
/// this hash had already dropped the prefix it would have replayed.
///
/// Nothing else is loosened. An edit to real early content, a rewritten
/// `thinking` block, and a changed `system` or `tools` axis all still read as
/// drift — see `the_fix_absorbs_the_withdrawal_without_blinding_the_detector`.
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
        .enumerate()
        .filter(|(index, msg)| !is_client_scaffolding_message(*index, msg))
        .map(|(_, msg)| msg)
        .take(EARLY_MESSAGES_WINDOW)
        .enumerate()
    {
        let msg = string_content_as_block(msg);
        let msg = without_trailing_spans(&msg);
        out[slot] = Some(hash_canonical(&msg, false));
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
    let mut block_tags = [None; EARLY_BLOCKS_WINDOW];
    let blocks = match msg.get("content") {
        Some(serde_json::Value::Array(arr)) => {
            for (i, block) in arr.iter().take(EARLY_BLOCKS_WINDOW).enumerate() {
                let full = hash_canonical(block, false);
                let mut short = [0u8; 8];
                short.copy_from_slice(&full[..8]);
                block_hashes[i] = Some(short);
                block_tags[i] = Some(BlockTag {
                    kind: BlockKind::of(block),
                    bytes: canonical_len(block, false).try_into().unwrap_or(u32::MAX),
                });
            }
            Some(arr.len())
        }
        _ => None,
    };
    MessageShape {
        blocks,
        block_hashes,
        block_tags,
    }
}

/// SHA-256 over a string's JSON literal form, streamed without the
/// intermediate `String` + `Value` wrapper. Digest-identical to
/// `hash_value(&Value::String(s.to_owned()))`.
fn hash_json_str(s: &str) -> [u8; 32] {
    struct DigestSink<'a>(&'a mut Sha256);
    impl std::io::Write for DigestSink<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hasher = Sha256::new();
    if serde_json::to_writer(DigestSink(&mut hasher), s).is_err() {
        return Sha256::digest(b"").into();
    }
    hasher.finalize().into()
}

/// SHA-256 over `serde_json::to_vec(value)`. Re-serializing the
/// borrowed `Value` defends against trivial whitespace differences
/// from the wire — operators care about *semantic* drift, not
/// formatter drift.
#[allow(dead_code)]
fn hash_value(value: &serde_json::Value) -> [u8; 32] {
    // Stream canonical bytes straight into the digest instead of
    // buffering a `to_vec` first (measured 1.14x). `to_writer` on a
    // `Value` only fails on IO errors, and the digest sink never
    // fails — but on the impossible error path we hash the empty byte
    // string, exactly like the old `unwrap_or_default()` contract, so
    // the digest stays stable rather than panicking.
    struct DigestSink<'a>(&'a mut Sha256);
    impl std::io::Write for DigestSink<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hasher = Sha256::new();
    if serde_json::to_writer(DigestSink(&mut hasher), value).is_err() {
        return Sha256::digest(b"").into();
    }
    hasher.finalize().into()
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
    observe(state, session_key, current, Origin::Inbound).0
}

/// The same comparison run over the body the proxy forwards, against its own
/// `DriftState`.
///
/// Split out for the event names alone. Both readings happen on every request,
/// and reusing `observe_drift` would double `cache_drift_observed` — every
/// existing count of that name would silently become a count of two passes
/// over different bodies. `_outbound` keeps the two greppable apart.
pub fn observe_outbound_drift(
    state: &DriftState,
    session_key: &str,
    current: StructuralHash,
) -> Option<String> {
    observe(state, session_key, current, Origin::Outbound).0
}

/// Like [`observe_outbound_drift`], plus whether this lane key was seen here
/// for the first time.
///
/// `None` dims mean two different things and a caller reasoning about what the
/// provider keyed on has to tell them apart: on a birth turn there is nothing
/// to compare against, while on any later turn `None` is a positive finding —
/// the forwarded body is unchanged in all three dimensions. Callers that treat
/// the second as "no information" throw away the only evidence that a stabilizer
/// held a client edit back.
pub fn observe_outbound_drift_with_birth(
    state: &DriftState,
    session_key: &str,
    current: StructuralHash,
) -> (Option<String>, bool) {
    observe(state, session_key, current, Origin::Outbound)
}

#[derive(Clone, Copy)]
enum Origin {
    Inbound,
    Outbound,
}

/// Like [`observe_drift`], plus whether this lane key was observed for the
/// first time (the `cache_drift_first_request` arm).
///
/// Seeding decisions need birth, and `dims.is_none()` is NOT birth — stable
/// append-only turns also return `None`. Callers must not infer birth from
/// the dims; use this function.
pub fn observe_drift_with_birth(
    state: &DriftState,
    session_key: &str,
    current: StructuralHash,
) -> (Option<String>, bool) {
    observe(state, session_key, current, Origin::Inbound)
}

fn observe(
    state: &DriftState,
    session_key: &str,
    current: StructuralHash,
    origin: Origin,
) -> (Option<String>, bool) {
    // `session_key` is really a stream-lane key
    // ([`stream_lane_key`]): the lookup is per lane, but the logged
    // session hash stays the session part so existing dashboards keep
    // joining. Bare session keys (no lane suffix) log exactly as before.
    let session_prefix = session_key_log_prefix(lane_session_part(session_key));
    let lane_prefix = session_key_log_prefix(session_key);
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
        None => note_first_observation(
            &mut cache,
            session_key,
            current,
            &session_prefix,
            &lane_prefix,
            origin,
        ),
        Some(previous) => note_repeat_observation(
            &mut cache,
            session_key,
            previous,
            current,
            &session_prefix,
            &lane_prefix,
            origin,
        ),
    }
}

/// Count sibling lanes under the same session: same-opener streams sharing
/// one credential (subagent fan-out). Scan only (no recency change).
/// Extracted from `note_first_observation` without behavior change.
fn count_sibling_lanes(cache: &LruCache<String, StructuralHash>, session_key: &str) -> usize {
    let session = lane_session_part(session_key);
    let mut siblings = 0;
    if session_key.contains(LANE_SEPARATOR) {
        for (key, _) in cache.iter() {
            if key.as_str() != session_key && lane_session_part(key.as_str()) == session {
                siblings += 1;
            }
        }
    }
    siblings
}

/// Record a first-seen session lane: log it (flagging subagent fan-out when
/// a sibling lane already exists) and store the baseline.
/// Extracted from `observe` without behavior change.
fn note_first_observation(
    cache: &mut LruCache<String, StructuralHash>,
    session_key: &str,
    current: StructuralHash,
    session_prefix: &str,
    lane_prefix: &str,
    origin: Origin,
) -> (Option<String>, bool) {
    match origin {
        Origin::Inbound => {
            tracing::info!(
                event = "cache_drift_first_request",
                session_key_hash = %session_prefix,
                lane_key_hash = %lane_prefix,
                current_hash_prefix = %structural_hash_log_prefix(&current),
                "cache_drift detector observed a new session"
            );
            // A second lane under one session means same-opener
            // streams are sharing the credential (subagent fan-out).
            // Before lanes each alternation read as drift and dropped
            // the stored prefix; now it is one line per new stream.
            // Scan only (no recency change); first_request is rare.
            let siblings = count_sibling_lanes(cache, session_key);
            if siblings > 0 {
                tracing::warn!(
                    event = "stream_lane_detected",
                    session_key_hash = %session_prefix,
                    lane_key_hash = %lane_prefix,
                    lane_count = siblings + 1,
                    "new stream lane under a known session: subagent \
                     fan-out sharing one opener, or a rewritten \
                     system/history. Each lane tracks its own prefix \
                     baseline from here."
                );
            }
        }
        Origin::Outbound => tracing::info!(
            event = "cache_drift_first_request_outbound",
            session_key_hash = %session_prefix,
            lane_key_hash = %lane_prefix,
            current_hash_prefix = %structural_hash_log_prefix(&current),
            "cache_drift detector observed a new session on the forwarded body"
        ),
    }
    cache.put(session_key.to_string(), current);
    (None, true)
}

/// Record a stable turn (append-only growth included): absorb withdrawn
/// client scaffolding without dropping the stored prefix, then refresh LRU
/// recency by reinserting. No event.
/// Extracted from `note_repeat_observation` without behavior change.
fn note_stable_turn(
    cache: &mut LruCache<String, StructuralHash>,
    session_key: &str,
    previous: &StructuralHash,
    current: &StructuralHash,
    session_prefix: &str,
    lane_prefix: &str,
    origin: Origin,
) {
    // The fix earning its keep: this turn holds still only because
    // the early window now steps over withdrawn client scaffolding.
    // Before 2026-08-26 it read as a rebuild boundary and dropped
    // the session's whole stored prefix. Counted here rather than
    // inferred from re-cache events, which are far too rare to
    // price a change against — 9 in three days.
    if matches!(origin, Origin::Inbound)
        && early_window_drifted(
            &previous.early_messages_legacy,
            &current.early_messages_legacy,
        )
    {
        tracing::info!(
            event = "early_scaffolding_absorbed",
            session_key_hash = %session_prefix,
            lane_key_hash = %lane_prefix,
            "withdrawn client scaffolding in the early window no longer \
             reads as drift; the stored prefix survives"
        );
    }
    // Stable (append-only growth included). No event.
    // Update LRU recency by reinserting.
    cache.put(session_key.to_string(), *current);
}

/// Compare against the stored baseline: stable turns (append-only growth
/// included) just refresh recency; drift logs the moved axes.
/// Extracted from `observe` without behavior change.
fn note_repeat_observation(
    cache: &mut LruCache<String, StructuralHash>,
    session_key: &str,
    previous: StructuralHash,
    current: StructuralHash,
    session_prefix: &str,
    lane_prefix: &str,
    origin: Origin,
) -> (Option<String>, bool) {
    let dims = drift_dims(&previous, &current);
    if dims.is_empty() {
        note_stable_turn(
            cache,
            session_key,
            &previous,
            &current,
            session_prefix,
            lane_prefix,
            origin,
        );
        return (None, false);
    }
    match origin {
        Origin::Inbound => tracing::warn!(
            event = "cache_drift_observed",
            session_key_hash = %session_prefix,
            lane_key_hash = %lane_prefix,
            drift_dims = %dims,
            // Empty unless the early window moved; the other two
            // axes are single values with nothing to break down.
            early_drift = %early_drift_detail(&previous, &current),
            // Empty unless the system block moved; see
            // [`system_drift_detail`] for the vocabulary.
            system_drift = %system_drift_detail(
                &previous.system_shape,
                &current.system_shape,
            ),
            previous_hash_prefix = %structural_hash_log_prefix(&previous),
            current_hash_prefix = %structural_hash_log_prefix(&current),
            "cache_drift detector observed structural change between turns of the same session"
        ),
        // INFO, not WARN. The forwarded hot zone moving is expected
        // whenever the inbound one did; only the case where the
        // client held still is interesting, and the classifier —
        // not the log reader — is what separates those.
        Origin::Outbound => tracing::info!(
            event = "cache_drift_observed_outbound",
            session_key_hash = %session_prefix,
            lane_key_hash = %lane_prefix,
            drift_dims = %dims,
            early_drift = %early_drift_detail(&previous, &current),
            // Empty unless the system block moved; see
            // [`system_drift_detail`] for the vocabulary.
            system_drift = %system_drift_detail(
                &previous.system_shape,
                &current.system_shape,
            ),
            previous_hash_prefix = %structural_hash_log_prefix(&previous),
            current_hash_prefix = %structural_hash_log_prefix(&current),
            "cache_drift detector observed structural change in the body the proxy forwarded"
        ),
    }
    cache.put(session_key.to_string(), current);
    (Some(dims), false)
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

/// Separator between the session key and the lane suffix in a
/// [`stream_lane_key`]. A control character no session-key arm emits:
/// `auth:`/`apikey:`/`ip:`/`ipua:`/`session:` keys are all printable.
pub(crate) const LANE_SEPARATOR: char = '\u{1f}';

/// Stream lane: the behavior-store key for one lineage under a session.
///
/// Same-opener subagent streams share a session key (auth + model + first
/// message) while carrying different systems. Keying the drift baseline, the
/// replay tracker, and the tool/pin stores by session alone lets the streams
/// overwrite each other's state: each system flip reads as drift, drops the
/// stored prefix, and the next turn re-emits un-replayed bytes past the
/// provider's stable entry. Measured 2026-09-08: Sonnet streams seconds
/// apart, 83 recache events, ~535k tokens, `actual_cache_read` pinned while
/// expected grew.
///
/// The lane folds the system hash into the key. It is stable from the first
/// turn (the system block is present on every request) and under append-only
/// growth, and it deliberately says nothing about tools: clients add tools
/// mid-conversation, and a lane that rotated on every tool change would never
/// engage replay. Same-system streams sharing a lane ride the replay
/// tracker's existing alternates machinery; a flip is a lane switch, not an
/// invalidation, so flipping back resumes with the tracker intact.
///
/// A rewritten system mints a new lane, which costs what today's rebuild
/// costs (one full write) — the old lane survives, so returning is cheaper
/// than it is today.
pub(crate) fn stream_lane_key(session_key: &str, hash: &StructuralHash) -> String {
    let mut lane = String::with_capacity(session_key.len() + 18);
    lane.push_str(session_key);
    lane.push(LANE_SEPARATOR);
    lane.push_str(&hex_prefix(&hash.system, 8));
    lane
}

/// The session part of a [`stream_lane_key`]: everything before the first
/// [`LANE_SEPARATOR`]. A bare session key (pre-lane callers, unit tests)
/// has no separator and is returned whole, so lane-aware lookups stay
/// backward compatible with unlaned keys.
pub(crate) fn lane_session_part(lane_key: &str) -> &str {
    match lane_key.split_once(LANE_SEPARATOR) {
        Some((session, _)) => session,
        None => lane_key,
    }
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
/// Warn if a continuation round disturbed the hot zone it inherited.
///
/// Continuation rounds POST straight to the upstream and never pass the
/// forwarding path, so `observe_outbound_drift` cannot see them. They do not
/// need it: a continuation only appends, so the answer is not "did this drift
/// from the previous turn" but "did this round leave the prefix it started
/// from alone". That is a comparison against `base`, and it needs no session
/// key and no LRU.
///
/// Silent on the good path. A line here means continuation rounds are paying
/// full input price for a prefix the provider had already cached.
pub fn check_continuation_prefix(
    base: &StructuralHash,
    continuation: &serde_json::Value,
    kind: ApiKind,
    request_id: &str,
    round: usize,
) {
    let current = compute_structural_hash(continuation, kind);
    let dims = drift_dims(base, &current);
    if !dims.is_empty() {
        tracing::warn!(
            event = "continuation_prefix_moved",
            request_id = %request_id,
            round = round,
            drift_dims = %dims,
            base_hash_prefix = %structural_hash_log_prefix(base),
            current_hash_prefix = %structural_hash_log_prefix(&current),
            "continuation round changed the cache hot zone it inherited; \
             this round cannot hit the prefix the first round cached"
        );
    }
}

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
/// Flatten the system block to its text and fingerprint it line by line.
///
/// Only `text` is read, so a `cache_control` breakpoint moving between
/// blocks does not read as a rewritten system prompt — the same reason
/// [`message_shape`] canonicalizes before hashing. Blocks are joined
/// with a newline, which makes the line index stable against a block
/// boundary moving as long as the text itself holds still.
fn system_shape(system: &serde_json::Value) -> SystemShape {
    let mut text = String::new();
    match system {
        serde_json::Value::String(s) => text.push_str(s),
        serde_json::Value::Array(blocks) => {
            for block in blocks {
                let piece = match block {
                    serde_json::Value::String(s) => Some(s.as_str()),
                    _ => block.get("text").and_then(serde_json::Value::as_str),
                };
                if let Some(piece) = piece {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(piece);
                }
            }
        }
        _ => {}
    }

    let mut shape = SystemShape::default();
    for (index, line) in text.lines().enumerate() {
        shape.lines = index + 1;
        if index >= SYSTEM_LINES_WINDOW {
            continue;
        }
        // 0 is reserved for "no such line", so fold it away rather than
        // let an empty line read as absent.
        //
        // Hash the JSON string literal directly: `hash_value` would
        // serialize `Value::String(line.to_string())`, i.e. two allocs
        // (the `String` plus the `Value` wrapper) per line for ~200
        // lines. `to_writer` on the `&str` emits byte-identical quoted
        // bytes, so the digest — and every downstream line hash — is
        // unchanged (measured 1.3x/line).
        let digest = hash_json_str(line);
        let hash = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
        shape.line_hashes[index] = if hash == 0 { 1 } else { hash };
        shape.line_lens[index] = u16::try_from(line.len()).unwrap_or(u16::MAX);
    }
    shape
}

/// Name what changed inside the system block, line by line.
///
/// `system` on its own says the cached prefix's head was rewritten but
/// not by whom, and the head is the most expensive thing to lose: eight
/// such findings in one day cost ~1M cache-read tokens. This turns each
/// one into one of:
///
/// - `lines 210->212` — lines were inserted or removed. Something is
///   appending to the system prompt; the count is the tell.
/// - `line[57]:42->60` — line 57 was rewritten and grew 18 bytes. Text
///   arrived; the length delta says how much.
/// - `line[57]:42->42` — line 57 changed without changing length. A
///   fixed-width field moved: a commit SHA, a timestamp, a counter.
///   These are the ones worth holding still, because nothing a client
///   means to say varies this way.
/// - `line[>192]` — changed past [`SYSTEM_LINES_WINDOW`], so no index.
///
/// Structure only. No system text reaches the log, on the same terms as
/// [`early_drift_detail`]: the point is to name the component, and the
/// shape does that without carrying a prompt into an operator's
/// terminal.
fn system_drift_detail(prev: &SystemShape, curr: &SystemShape) -> String {
    let mut parts: Vec<String> = Vec::new();
    if prev.lines != curr.lines {
        parts.push(format!("lines {}->{}", prev.lines, curr.lines));
    }

    // Cap the named lines: a wholesale rewrite would otherwise print a
    // line index per line and bury the cases worth reading.
    const MAX_NAMED: usize = 4;
    let mut named = 0;
    for index in 0..SYSTEM_LINES_WINDOW {
        if prev.line_hashes[index] == curr.line_hashes[index] {
            continue;
        }
        if named == MAX_NAMED {
            parts.push("...".to_string());
            break;
        }
        parts.push(format!(
            "line[{index}]:{}->{}",
            prev.line_lens[index], curr.line_lens[index]
        ));
        named += 1;
    }

    // Both sides ran past the window and nothing inside it moved: the
    // change is real but unindexable. Say so rather than stay silent.
    if parts.is_empty() && (prev.lines > SYSTEM_LINES_WINDOW || curr.lines > SYSTEM_LINES_WINDOW) {
        parts.push(format!("line[>{SYSTEM_LINES_WINDOW}]"));
    }
    parts.join(",")
}

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
                format!(
                    "block[{}]{}",
                    changed.join(","),
                    block_deltas(prev, curr, &changed)
                )
            }
        }
        // One side had blocks and the other a bare string, or both are
        // strings — either way there is no index to give.
        _ => "string".to_string(),
    }
}

/// What the rewritten blocks were, and how their size moved.
///
/// Reads ` tool_result 4195B->1202B`, or ` thinking 812B` when only the bytes
/// held still. Empty when neither side tagged the block, so a caller that
/// prints it unconditionally is safe.
fn block_deltas(prev: &MessageShape, curr: &MessageShape, changed: &[String]) -> String {
    let parts: Vec<String> = changed
        .iter()
        .filter_map(|i| i.parse::<usize>().ok())
        .filter_map(
            |i| match (prev.block_tags.get(i)?, curr.block_tags.get(i)?) {
                (Some(p), Some(c)) if p.kind == c.kind && p.bytes == c.bytes => {
                    Some(format!("{} {}B", p.kind.name(), p.bytes))
                }
                (Some(p), Some(c)) if p.kind == c.kind => {
                    Some(format!("{} {}B->{}B", p.kind.name(), p.bytes, c.bytes))
                }
                (Some(p), Some(c)) => Some(format!(
                    "{} {}B->{} {}B",
                    p.kind.name(),
                    p.bytes,
                    c.kind.name(),
                    c.bytes
                )),
                _ => None,
            },
        )
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join("+"))
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
    derive_session_key_with_model(headers, client_addr, body, kind, None)
}

/// As [`derive_session_key`], but with the conversation identity taken from
/// `identity_model` instead of the body's own `model` field.
///
/// The cost-aware router rewrites `model` before this runs, and the
/// discriminator folds the model in on purpose — a genuine model switch does
/// start a new provider cache lineage. A reroute is not that switch: it is the
/// proxy's own choice, and letting it rotate the key made every rerouted turn
/// report `cache_drift_first_request` and threw away the conversation's
/// confirmed-frozen prefix. Passing the model the *client* asked for keeps one
/// conversation on one key whichever upstream served it.
pub fn derive_session_key_with_model(
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    body: &serde_json::Value,
    kind: ApiKind,
    identity_model: Option<&str>,
) -> String {
    match identity_branch(headers, client_addr) {
        IdentityBranch::Pinned(sid) => format!("session:{}", hash_secret(&sid)),
        IdentityBranch::Branch(branch) => {
            let conv = conversation_discriminator(body, kind, identity_model);
            format!("{branch}:{conv}")
        }
    }
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
fn conversation_discriminator(
    body: &serde_json::Value,
    kind: ApiKind,
    identity_model: Option<&str>,
) -> String {
    let model = identity_model
        .or_else(|| body.get("model").and_then(|m| m.as_str()))
        .unwrap_or("");
    match conversation_messages(body, kind).first() {
        Some(first) => {
            let mut hasher = Sha256::new();
            hasher.update(model.as_bytes());
            // NUL separator: domain-separate the model from the
            // message bytes so no (model, message) pair can alias
            // another by shifting bytes across the boundary.
            hasher.update([0u8]);
            // Canonical message bytes streamed in; the old
            // `to_vec(&canonical).unwrap_or_default()` fed empty bytes
            // on the impossible serialization failure, and a failed
            // `write_canonical` simply feeds the prefix bytes hashed so
            // far. Serialization of a borrowed `Value` into an
            // infallible sink cannot fail in practice.
            let _ = write_canonical(&mut DigestSink(&mut hasher), first, false);
            let digest = hasher.finalize();
            hex_prefix(&digest, 8)
        }
        None => "-".to_string(),
    }
}

/// Auto-derived identity branch, split from the conversation discriminator
/// so [`derive_session_key_with_model`] and [`model_free_lineage_key`] share
/// it by construction. `match` arms are exhaustive: adding a branch forces
/// updating both callers at compile time, so identity can never silently
/// merge (or split) for one consumer and not the other.
enum IdentityBranch {
    /// Explicit `x-headroom-session-id`: operator-managed identity, shared
    /// across models and systems already.
    Pinned(String),
    /// Everything in the session key except the discriminator
    /// (`auth:<h16>`, `apikey:<h>`, `ipua:<h16>`, `ip:<addr>`).
    Branch(String),
}

fn identity_branch(headers: &HeaderMap, client_addr: &SocketAddr) -> IdentityBranch {
    if let Some(sid) = headers
        .get("x-headroom-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        return IdentityBranch::Pinned(sid.to_string());
    }
    if let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        return IdentityBranch::Branch(format!("auth:{}", hash_secret(token)));
    }
    // `x-api-key` is the Anthropic/OpenAI-Responses convention.
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return IdentityBranch::Branch(format!("apikey:{}", hash_secret(key)));
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
        return IdentityBranch::Branch(format!("ipua:{:016x}", h.finish()));
    }
    IdentityBranch::Branch(format!("ip:{ip}"))
}

/// Cross-model lineage key: `(identity branch, H(canonical first message))`.
///
/// Two sessions share a lineage iff they share credential/opener AND opening
/// message, whatever the model or system prompt. Used to find donor sessions
/// for offload-gate seeding when a model switch (or resumed conversation)
/// mints a fresh session key. Full 64-hex message hash, deliberately NOT the
/// 8-hex session prefix: the index must not conflate distinct conversations
/// the way session keys may.
///
/// `None` when there is no auto-derived identity to match on (explicit
/// session id — already shared — or a body with no conversation messages).
pub(crate) fn model_free_lineage_key(
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    body: &serde_json::Value,
    kind: ApiKind,
) -> Option<(String, String)> {
    let IdentityBranch::Branch(branch) = identity_branch(headers, client_addr) else {
        return None;
    };
    let first = conversation_messages(body, kind).into_iter().next()?;
    let mut hasher = Sha256::new();
    let _ = write_canonical(&mut DigestSink(&mut hasher), first, false);
    Some((branch, hex_prefix(&hasher.finalize(), 32)))
}

/// Sink shared by [`conversation_discriminator`] and
/// [`model_free_lineage_key`]: canonical bytes straight into the digest.
struct DigestSink<'a>(&'a mut Sha256);

impl std::io::Write for DigestSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
mod tests;

// Globs cap each item at its own visibility: `pub` items stay public
// API, the rest stay in-crate. Modules with no `pub` item would
// otherwise warn that they re-export nothing.
#[allow(unused_imports)]
pub use self::{};
