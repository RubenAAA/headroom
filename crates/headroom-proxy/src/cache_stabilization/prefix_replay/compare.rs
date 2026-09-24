//! Canonical message comparison for replay.
//!
//! Moved out of `prefix_replay.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Does `candidate` canonically lead `current`?
///
/// This is exactly the test [`overlay_cached_prefix_reported`] applies before
/// replaying, so selecting a candidate with it cannot widen what gets
/// forwarded: a prefix that passes here is one the overlay would have accepted
/// anyway. Content only — the shared canonicalizer strips `cache_control` and
/// the rest of the per-turn transport churn.
/// Against a `current` slice the caller already canonicalized.
///
/// Both selecting a prefix and storing one walk every branch held for the
/// session, testing each against the same current conversation. Canonicalizing
/// that conversation inside those loops made the work scale with branches times
/// depth — on a 600-message session with a full alternates list, tens of
/// thousands of message projections per request, all but one set of them
/// identical. Hoisting it out leaves one projection of the current messages and
/// one of each candidate.
pub(super) fn matches_canonical_prefix(candidate: &[Value], canonical_current: &[Value]) -> bool {
    if candidate.is_empty() || canonical_current.len() < candidate.len() {
        return false;
    }
    canonicalize_slice(candidate).as_slice() == &canonical_current[..candidate.len()]
}

/// How many leading messages a stored prefix and this turn agree on.
pub(super) fn canonical_agreement_len(candidate: &[Value], canonical_current: &[Value]) -> usize {
    canonicalize_slice(candidate)
        .iter()
        .zip(canonical_current)
        .take_while(|(stored, current)| stored == current)
        .count()
}

/// How much of a stored prefix's tail may be edited and still count as the same
/// stream.
///
/// A client that edits a message inside its own history stops being a prefix of
/// what we stored, so the exact match above cannot see it — and that is the one
/// case worth replaying, because everything ahead of the edit is still cached.
/// Every such event in the 2026-08-15/16 logs edited the last message or the one
/// before it: `first_diff_index` 305 of 307, 269 of 271, 286 of 288, 321 of 323,
/// 309 of 311.
pub(super) const TAIL_EDIT_SLACK: usize = 2;

/// The shortest agreeing run that counts as evidence of one stream.
///
/// Unrelated conversations share their opening messages — the same system
/// reminder, the same first instruction — so a short agreement says nothing
/// about identity. A conversation short enough to fail this has almost nothing
/// cached to lose by declining.
pub(super) const MIN_AGREEING_RUN: usize = 4;

/// Proactive expansion is deliberately a cache *tail*. When it becomes the
/// cache-control target, its first appearance makes Anthropic write the entire
/// segment we were trying to preserve. Keep the marker on the preceding block
/// instead, leaving the one-time expansion outside the cached prefix.
pub(super) const PROACTIVE_EXPANSION_OPEN_TAG: &str = "<headroom_proactive_expansion>";

pub(super) fn is_proactive_expansion_text(text: &str) -> bool {
    text.contains(PROACTIVE_EXPANSION_OPEN_TAG)
}

pub(super) fn is_proactive_expansion_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_proactive_expansion_text)
}

/// Edge whitespace off a text block, for the comparison key only.
///
/// The forwarding side never calls this: trimming there would rewrite the
/// client's bytes for no gain. Here it costs nothing and buys the one thing the
/// key needs — that a message keys the same however the client shaped it. A
/// block left empty goes, because the other representation has no block there
/// at all.
pub(super) fn trim_text_block(block: Value) -> Option<Value> {
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return Some(block);
    }
    let Some(text) = block.get("text").and_then(Value::as_str) else {
        return Some(block);
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() == text.len() {
        return Some(block);
    }
    let trimmed = trimmed.to_string();
    let mut block = block;
    if let Some(obj) = block.as_object_mut() {
        obj.insert("text".to_string(), Value::String(trimmed));
    }
    Some(block)
}

/// Match the stored prefix against this turn, stepping over scaffolding the
/// client has since withdrawn. Returns how many of THIS turn's messages the
/// stored prefix covers, or `None` when they genuinely disagree.
///
/// The index-aligned compare this backs up cannot survive a withdrawal. The
/// client drops one scaffolding message out of the middle of history and every
/// message behind it shifts by one, so the compare meets an `assistant` message
/// where it stored a `system` one and calls the whole prefix diverged. Measured
/// on 2026-08-26: 7 of 16 content-divergence declines were this, first diff on
/// `role` every time, one of them 37,448 tokens.
///
/// Only the STORED side may be stepped over. A skip there is free — the caller
/// forwards `prev_fwd` whole, so the withdrawn message still goes out, holding
/// the provider's cached bytes exactly as they were. Stepping over a message on
/// the CURRENT side would mean not forwarding something the client sent, which
/// is how reminders get lost; the caller declines to those instead.
///
/// One exception: a scaffolding message the client SHRANK rather than removed.
/// Claude Code ends a turn with a `system` message carrying two reminders as two
/// blocks, then re-renders it next turn with only the first, as one string. The
/// two forms compare unequal, and skipping only the stored copy leaves the
/// shrunken one to be spliced in right behind it — two `system` messages back
/// to back, which the adjacency net declines. Measured on 2026-09-07: one
/// session declined 82 turns running, 1.47M cached tokens rebuilt. When the
/// current message at the same slot is itself scaffolding, it is that
/// replacement: the stored copy goes out from cache and the shrunken one is
/// consumed with it. Nothing the client wrote is lost — the stored copy is a
/// superset of what it sent this turn.
pub(super) fn align_over_withdrawn_scaffolding(
    previous_originals: &[Value],
    current_originals: &[Value],
) -> Option<usize> {
    let mut current_index = 0usize;
    for (stored_index, stored) in previous_originals.iter().enumerate() {
        let stored_canonical = canonicalize_for_prefix_compare(stored);
        if current_originals
            .get(current_index)
            .is_some_and(|current| canonicalize_for_prefix_compare(current) == stored_canonical)
        {
            current_index += 1;
            continue;
        }
        if is_client_scaffolding_message(stored_index, stored) {
            if current_originals
                .get(current_index)
                .is_some_and(|current| is_client_scaffolding_message(current_index, current))
            {
                current_index += 1;
            }
            continue;
        }
        return None;
    }
    Some(current_index)
}

/// Position of the first `role: "system"` message the Anthropic API would
/// refuse, or `None` when the sequence is acceptable.
///
/// The rule upstream enforces: a `system` message must follow a `user` message
/// or an `assistant` message ending in a server tool result, and the
/// directive-only form (empty content) is allowed anywhere. Splicing two
/// legal sequences together cannot break that on its own — the join reproduces
/// an adjacency the client itself wrote — so this is a net under the splice
/// rather than a working part of it, and it should never fire. It is here
/// because the failure it catches costs a whole turn: the request 400s, and a
/// re-cache of the entire conversation follows the retry.
pub(super) fn first_illegal_system_position(messages: &[Value]) -> Option<usize> {
    messages.iter().enumerate().position(|(index, message)| {
        if message.get("role").and_then(Value::as_str) != Some("system") {
            return false;
        }
        // The directive-only form carries no content and is legal anywhere.
        if has_empty_canonical_content(message) {
            return false;
        }
        let Some(previous) = index.checked_sub(1).and_then(|i| messages.get(i)) else {
            return true;
        };
        match previous.get("role").and_then(Value::as_str) {
            Some("user") => false,
            Some("assistant") => !ends_in_server_tool_result(previous),
            _ => true,
        }
    })
}

/// Whether an assistant message's last content block is a server-side tool
/// result (`web_search_tool_result` and its siblings), which the API accepts
/// as a predecessor for a `system` message.
pub(super) fn ends_in_server_tool_result(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.last())
        .and_then(|block| block.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.ends_with("_tool_result"))
}

/// Strip reminder spans from a text block, dropping the block when only
/// scaffolding was there. Counterpart to [`split_ephemeral_spans`] for the
/// comparison key; the forwarding side lifts the same spans in
/// [`relocate_ephemeral_blocks_counted`].
pub(super) fn without_ephemeral_spans(block: Value) -> Option<Value> {
    if !block_carries_ephemeral_span(&block) {
        return Some(block);
    }
    let text = block
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (kept, spans) = split_ephemeral_spans(&text);
    if spans.is_empty() {
        return Some(block);
    }
    if kept.trim().is_empty() {
        return None;
    }
    let mut block = block;
    if let Some(obj) = block.as_object_mut() {
        obj.insert("text".to_string(), Value::String(kept));
    }
    Some(block)
}

/// Anthropic refuses `cache_control` on a `thinking` block outright —
/// `messages.N.content.0.thinking.cache_control: Extra inputs are not
/// permitted`, a 400 for the whole turn. Extended thinking makes assistant
/// messages whose only block is a thinking block, so "the last block of the
/// message" lands on one often enough to refuse 8% of turns (measured
/// 2026-08-12). Such a block is not a legal target; the placement pass falls
/// back to an earlier block, or to an earlier message when there is none.
pub(super) fn is_thinking_block(block: &Value) -> bool {
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
pub(super) const NON_SEMANTIC_KEYS: &[&str] = &[
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
pub(super) const OPAQUE_PAYLOAD_KEYS: &[&str] = &["input", "arguments", "json"];

pub(super) fn is_non_semantic(key: &str) -> bool {
    NON_SEMANTIC_KEYS.contains(&key)
}

pub(super) fn is_opaque_payload(key: &str) -> bool {
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
/// Openings that mark a client-side side errand rather than a conversation turn.
///
/// Claude Code runs these against the live conversation: it resends the whole
/// history and appends a synthetic final message asking for something the user
/// never sees.
pub(super) const SIDE_ERRAND_OPENINGS: &[&str] = &["[SUGGESTION MODE:"];

/// Whether this turn is a side errand rather than a step in the conversation.
///
/// Such a request shares the session key with the real conversation — same
/// model, same opening message — so parking it makes it the session's
/// "previous turn". The next real turn then diverges at the final message and
/// recaches everything from there. Measured on 2026-08-20: 26 of 90 prefix
/// divergences in one day, each one a full recache of a prefix that had not
/// actually changed.
///
/// Only the last message is examined: the history in front of it is the real
/// conversation, which is exactly why the collision happens.
pub fn is_side_errand(messages: &[Value]) -> bool {
    let Some(last) = messages.last() else {
        return false;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let opening = match last.get("content") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(blocks)) => blocks
            .first()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        _ => "",
    };
    SIDE_ERRAND_OPENINGS
        .iter()
        .any(|marker| opening.trim_start().starts_with(marker))
}

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
                    // Anthropic string sugar → canonical block form, then back
                    // through the array arm so a reminder embedded in the
                    // string is treated exactly like one that arrived as its
                    // own block. Inserting the block directly, as this used to,
                    // skipped the filter below and left the reminder text in
                    // the key.
                    let text = val.as_str().unwrap_or_default();
                    let blocks =
                        Value::Array(vec![serde_json::json!({"type": "text", "text": text})]);
                    out.insert(key.clone(), canonicalize_for_prefix_compare(&blocks));
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
                    // Drop the client's ephemeral scaffolding, for the same
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
                    //
                    // Span level first, block level second. Reversed — as this
                    // was — a block that OPENS with a reminder and carries real
                    // text after it was dropped whole by the block-level
                    // predicate, taking the text with it, so string sugar
                    // holding `<system-reminder>…</system-reminder>\nDo X` keyed
                    // as no content at all while the same message in block form
                    // kept `Do X`.
                    .filter_map(without_ephemeral_spans)
                    // What survives the lift is a block with an unclosed tag,
                    // which `split_ephemeral_spans` deliberately leaves alone.
                    .filter(|v| !is_ephemeral_client_block(v))
                    // Edge whitespace, for the key only. `split_ephemeral_spans`
                    // trims what it leaves behind, so a message whose reminder
                    // was embedded in the text keys as `Do X`, while the same
                    // message in block form keeps the separator on the
                    // neighbouring block — `Do X\n` — because that block never
                    // carried a span and nothing trimmed it. The two then differ
                    // at `content[0].text`, which is what all three declines
                    // logged on 2026-08-13 named.
                    .filter_map(trim_text_block)
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// Canonicalize a whole slice of messages (helper for slice comparisons).
pub(super) fn canonicalize_slice(messages: &[Value]) -> Vec<Value> {
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
pub(super) fn has_empty_canonical_content(message: &Value) -> bool {
    canonicalize_for_prefix_compare(message)
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
}

/// Length of the replayable stored prefix after removing trailing messages the
/// next turn will rewrite.
///
/// A message whose canonical content is empty is pure scaffolding that was
/// never in the provider's cached prefix.
///
/// A second rule used to cap this at the last message carrying a movable
/// ephemeral span — the message relocation had just landed its collection on.
/// Relocation stripped those blocks out again once the conversation grew past
/// the message, so holding the fat copy in replay state read as an edit inside
/// the cached prefix and busted it. Three events on 2026-08-14 measured that:
/// message 20 of 21 lost `text,text` four messages later (24,565 re-created),
/// and messages 196 of 198 and 164 of 166 each gained a plain block against the
/// branch they were compared with (128,616 and 126,388, both falling back to a
/// 21,359-token read — system and tools, the breakpoint before any message).
///
/// Both halves of that went with relocation. Nothing rewrites a message's spans
/// after the fact now, so the stored copy stays the message's only form, and
/// keeping it is what makes the client's own withdrawal of a reminder harmless:
/// replay forwards the stored bytes over it. Capping here would exclude the
/// newest user turn — the one message that always carries spans now — and hand
/// exactly that withdrawal a way through.
pub(super) fn replayable_stored_prefix_len(original_messages: &[Value]) -> usize {
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
    // Compare canonicalized messages one at a time with early exit
    // instead of materializing two full `Vec<Value>`s and `==`-ing
    // them (identical semantics: `Vec` equality is length — checked
    // above via the slice bound — plus ordered element equality with
    // short-circuit). Steady-state cost is the same canonicalizations;
    // a mismatch skips the rest plus both outer allocations.
    for (cur, prev) in current_messages[..prefix_len].iter().zip(prev_orig.iter()) {
        if canonicalize_for_prefix_compare(cur) != canonicalize_for_prefix_compare(prev) {
            return None;
        }
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
///
/// Takes no confirmed floor, so the non-inflation bound does not apply: the
/// pre-floor posture. Production calls [`overlay_cached_prefix_reported`] with
/// the tracker's provider-confirmed count; this shim stays for callers with no
/// count in scope. (Upstream defaults the floor to `None` for a fully
/// size-bounded overlay, but this overlay never had the bound — `None` here
/// keeps the historical unbounded behavior instead of inventing declines on
/// paths that never asked for one.)
///
/// Assumes the stored prefix belongs to this conversation. Callers that got it
/// from the fallback — which hands back the session's most recent prefix even
/// when nothing continues it — must use [`overlay_cached_prefix_reported`] and
/// pass `continues_chain: false`.
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
        true,
        None,
    )
    .0
}
