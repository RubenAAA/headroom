//! Reasoning/thinking block handling across turns: detect rewrites, drop
//! unsigned or headroom-signed blocks, restore and repair client blocks.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Which messages the proxy rewrote this turn, and which of those the provider
/// is entitled to refuse.
pub(super) struct RewrittenMessages {
    /// Indices whose content differs from what the client sent.
    pub(super) indices: Vec<usize>,
    /// The subset carrying a `thinking` or `redacted_thinking` block. Anthropic
    /// rejects a turn whose signed thinking blocks changed, so any index here is
    /// a rejection this proxy is capable of causing.
    pub(super) with_thinking: Vec<usize>,
    /// Indices where a signed reasoning block itself differs on the wire.
    ///
    /// Compared raw, not canonically: `cache_control` is the one key this proxy
    /// rewrites on every message by design, and adding or removing it on a
    /// signed block is still a modification of that block as far as the provider
    /// is concerned. The canonical compare above is blind to exactly that, which
    /// is why this list is kept separately rather than folded into it.
    pub(super) thinking_touched: Vec<usize>,
}

/// Compare what the client sent against what is about to go on the wire.
///
/// Uses the prefix canonicaliser, so `cache_control` placement — which this
/// proxy owns and rewrites every turn by design — does not count as a change.
pub(super) fn rewritten_message_report(
    original: &[serde_json::Value],
    forwarded: &[serde_json::Value],
) -> RewrittenMessages {
    use cache_stabilization::prefix_replay::canonicalize_for_prefix_compare;
    let mut indices = Vec::new();
    let mut with_thinking = Vec::new();
    let mut thinking_touched = Vec::new();
    for (i, (before, after)) in original.iter().zip(forwarded.iter()).enumerate() {
        if thinking_blocks_differ(before, after) {
            thinking_touched.push(i);
        }
        if canonicalize_for_prefix_compare(before) == canonicalize_for_prefix_compare(after) {
            continue;
        }
        indices.push(i);
        if carries_thinking_block(before) || carries_thinking_block(after) {
            with_thinking.push(i);
        }
    }
    RewrittenMessages {
        indices,
        with_thinking,
        thinking_touched,
    }
}

/// True if the signed reasoning blocks of a message are not byte-identical
/// between what the client sent and what goes on the wire.
pub(super) fn thinking_blocks_differ(
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> bool {
    fn reasoning_blocks(message: &serde_json::Value) -> Vec<&serde_json::Value> {
        message
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| {
                        matches!(
                            b.get("type").and_then(|t| t.as_str()),
                            Some("thinking") | Some("redacted_thinking")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    reasoning_blocks(before) != reasoning_blocks(after)
}

/// True if a message's content holds a signed reasoning block.
pub(super) fn carries_thinking_block(message: &serde_json::Value) -> bool {
    message
        .get("content")
        .and_then(|c| c.as_array())
        .is_some_and(|blocks| {
            blocks.iter().any(|b| {
                matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
        })
}

/// Every signed reasoning block in a message array, in order.
/// Whether `after` still carries every signed reasoning block the provider
/// will read back. Anthropic reads the LAST assistant message's blocks back
/// (they must stay while a tool loop is open) and refuses any block that comes
/// back altered. Blocks from earlier assistant turns may be dropped whole:
/// `compression::prior_thinking` does so on a rebuild boundary, and the replay
/// store repeats the stripped bytes on every steady turn after. So: the last
/// assistant message's blocks match exactly, and the rest of `after` is a
/// subsequence of `before` — nothing edited, nothing invented.
pub(super) fn signed_reasoning_preserved(
    before: &[serde_json::Value],
    after: &[serde_json::Value],
) -> bool {
    fn last_assistant_blocks(messages: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .map(|m| signed_reasoning_blocks(std::slice::from_ref(m)))
            .unwrap_or_default()
    }
    if last_assistant_blocks(before) != last_assistant_blocks(after) {
        return false;
    }
    // Length gate: `after` as a subsequence of `before` needs at most
    // as many blocks (pigeonhole) — free exact pre-check before the
    // O(n·m) deep-compare scan below.
    let before_blocks = signed_reasoning_blocks(before);
    let after_blocks = signed_reasoning_blocks(after);
    if after_blocks.len() > before_blocks.len() {
        return false;
    }
    let mut remaining = before_blocks.into_iter();
    after_blocks
        .into_iter()
        .all(|block| remaining.any(|kept| kept == block))
}

pub(super) fn signed_reasoning_blocks(messages: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    messages
        .iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_array()))
        .flatten()
        .filter(|b| {
            let is_reasoning = matches!(
                b.get("type").and_then(|t| t.as_str()),
                Some("thinking") | Some("redacted_thinking")
            );
            // Genuinely signed *by the provider*, as the name says. What this
            // guards is Anthropic's refusal of a signed block that came back
            // altered, and neither an unsigned block nor one carrying our own
            // envelope has anything to violate. Counting those would make the
            // two drop stages meant to remove them look like tampering, and
            // the restore below would put back the block upstream is about to
            // refuse.
            is_reasoning && !is_unsigned_reasoning(b) && !is_headroom_signed_reasoning(b)
        })
        .collect()
}

/// Drop `thinking` blocks that carry no signature.
///
/// The counterpart to `sse::stream_finisher`. When an upstream stream dies
/// with a thinking block open, the finisher closes that block so the turn ends
/// cleanly — but the `signature_delta` never arrived, so the block the client
/// stores is unsigned. Anthropic refuses a thinking block without a valid
/// signature, which would turn one truncated answer into a conversation that
/// can no longer be sent at all.
///
/// So the blocks the proxy had to cut short are dropped on their way back up.
/// This runs first among the stages that care, ahead of prefix replay and the
/// tail breakpoint, so every one of them sees the message array that actually
/// reaches the provider. Stripping later would leave the replay store holding
/// a block that never went on the wire and overlaying it back in on every
/// later turn, which costs a re-cache rather than a refused turn.
///
/// `signed_reasoning_blocks` excludes exactly what this removes, so the
/// tampering guard downstream never mistakes this for a rewrite.
///
/// Signed blocks are never touched, and neither is a body without an unsigned
/// one — which is every body that never met a dropped stream.
pub(super) fn drop_unsigned_reasoning_blocks(
    body_to_send: bytes::Bytes,
    request_id: &str,
) -> bytes::Bytes {
    // Cheap gate: the overwhelming majority of bodies have no reasoning block
    // at all, and this spares them a parse.
    const MARKER: &[u8] = b"\"thinking\"";
    if !body_to_send.windows(MARKER.len()).any(|w| w == MARKER) {
        return body_to_send;
    }
    let Some((out, dropped, markers_moved)) =
        drop_reasoning_blocks_where(&body_to_send, is_unsigned_reasoning)
    else {
        return body_to_send;
    };
    tracing::info!(
        request_id = %request_id,
        event = "unsigned_reasoning_blocks_dropped",
        dropped,
        markers_moved,
        "removed thinking blocks with no signature; they are the tail of a \
         stream that died mid-block and upstream would refuse them"
    );
    out
}

/// Drop `thinking` blocks this proxy signed itself.
///
/// A routed turn answered by an OpenAI-shaped upstream comes back with its
/// reasoning item packed into a signature only this proxy can read — see
/// [`crate::handlers::reasoning_signature`]. That works while the
/// conversation stays on the routed model, which is what the `:translate`
/// routes were built for. The cost-aware router (#1706) broke that
/// assumption: it sends one tool-less turn to a cheap model and leaves the
/// next one, which usually declares tools, on Anthropic. The client stores
/// the block and hands it back, and Anthropic refuses a signature it never
/// issued — one cheap turn poisoning every turn after it.
///
/// So our own envelopes come off on the way to Anthropic. Nothing is lost
/// that Anthropic could have used: it cannot read the envelope, and the model
/// that wrote the reasoning is not the one being asked to continue it. The
/// `:translate` paths do not call this, so replay to the routed upstream is
/// untouched.
///
/// The gate is the prefix itself rather than `"thinking"`, so a body that
/// never met a routed turn skips this on a substring scan.
pub(crate) fn drop_headroom_signed_reasoning_blocks(
    body_to_send: bytes::Bytes,
    request_id: &str,
) -> bytes::Bytes {
    const MARKER: &[u8] = b"headroom:codex:v1:";
    if !body_to_send.windows(MARKER.len()).any(|w| w == MARKER) {
        return body_to_send;
    }
    let Some((out, dropped, markers_moved)) =
        drop_reasoning_blocks_where(&body_to_send, is_headroom_signed_reasoning)
    else {
        return body_to_send;
    };
    tracing::info!(
        request_id = %request_id,
        event = "headroom_signed_reasoning_blocks_dropped",
        dropped,
        markers_moved,
        "removed thinking blocks carrying this proxy's own reasoning envelope; \
         a routed turn wrote them and Anthropic would refuse a signature it \
         did not issue"
    );
    out
}

/// The message-array surgery both drop stages share.
///
/// Returns `None` when there is nothing to do — unparseable body, no
/// `messages`, or no block the predicate claims — so the caller can hand back
/// its original `Bytes` untouched rather than pay a re-serialize that would
/// change nothing.
pub(super) fn drop_reasoning_blocks_where(
    body_to_send: &bytes::Bytes,
    doomed: fn(&serde_json::Value) -> bool,
) -> Option<(bytes::Bytes, usize, usize)> {
    let mut v = serde_json::from_slice::<serde_json::Value>(body_to_send).ok()?;
    let messages = v.get_mut("messages").and_then(|m| m.as_array_mut())?;
    let mut dropped = 0usize;
    let mut markers_moved = 0usize;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        // Removing every block would leave a message with empty content, which
        // upstream refuses just as firmly as the doomed block does. Nothing
        // this proxy writes looks like that — `stream_finisher` always leaves a
        // text block behind — but history the proxy did not write reaches here
        // too, and trading one bad turn for a different bad turn is no trade.
        if content.iter().all(&doomed) {
            continue;
        }
        // A `cache_control` marker on a doomed block is a cache breakpoint, and
        // dropping it silently would move the cached prefix boundary and cost a
        // re-cache on every later turn of the conversation. It rides to the
        // next surviving block instead — the same carry `thinking_compactor`
        // makes when it rewrites a block out from under a marker.
        let mut carried: Option<serde_json::Value> = None;
        let mut kept: Vec<serde_json::Value> = Vec::with_capacity(content.len());
        for mut block in content.drain(..) {
            if doomed(&block) {
                dropped += 1;
                if let Some(cc) = block.get("cache_control") {
                    carried = Some(cc.clone());
                }
                continue;
            }
            if let Some(cc) = carried.take() {
                // An existing marker wins: a second one here would spend a
                // breakpoint on the same boundary, and there are only four.
                if block.get("cache_control").is_none() {
                    block["cache_control"] = cc;
                    markers_moved += 1;
                }
            }
            kept.push(block);
        }
        // Still carrying means the dropped block was last, so the marker goes
        // to whatever ends the message now.
        if let Some(cc) = carried {
            if let Some(last) = kept.last_mut() {
                if last.get("cache_control").is_none() {
                    last["cache_control"] = cc;
                    markers_moved += 1;
                }
            }
        }
        *content = kept;
    }
    if dropped == 0 {
        return None;
    }
    let out = serde_json::to_vec(&v).ok()?;
    Some((bytes::Bytes::from(out), dropped, markers_moved))
}

/// A `thinking` block this proxy signed on a routed turn.
///
/// `redacted_thinking` is included for the same reason it is everywhere else
/// here: the two types travel together and a caller that handled one and not
/// the other would leave half the problem on the wire.
pub(super) fn is_headroom_signed_reasoning(block: &serde_json::Value) -> bool {
    let is_reasoning = matches!(
        block.get("type").and_then(|t| t.as_str()),
        Some("thinking") | Some("redacted_thinking")
    );
    let ours = block
        .get("signature")
        .and_then(|s| s.as_str())
        .is_some_and(crate::handlers::reasoning_signature::is_headroom_reasoning_signature);
    is_reasoning && ours
}

/// A `thinking` block the model never got to sign.
///
/// `redacted_thinking` carries opaque `data` rather than a signature and is
/// always delivered whole, so a block with `data` is complete whatever its
/// signature says.
pub(super) fn is_unsigned_reasoning(block: &serde_json::Value) -> bool {
    let is_reasoning = matches!(
        block.get("type").and_then(|t| t.as_str()),
        Some("thinking") | Some("redacted_thinking")
    );
    let unsigned = block
        .get("signature")
        .and_then(|s| s.as_str())
        .map_or(true, |s| s.is_empty());
    is_reasoning && unsigned && block.get("data").is_none()
}

/// Put the client's message array back when the outbound body no longer
/// carries their signed reasoning blocks unchanged.
///
/// Anthropic refuses a turn whose signed `thinking` or `redacted_thinking`
/// blocks came back altered — "blocks cannot be modified", naming a message
/// index but not who modified it. The live-zone compressor excludes those
/// block types and every stage of the outbound chain returns its input
/// untouched when it has nothing to do, so today the invariant holds by
/// convention: prefix replay rewrites the message array wholesale, the hook
/// seam re-serializes whatever a hook hands back, and neither checks. This is
/// the check, taken once on the bytes that are about to leave.
///
/// Restoring only `messages` keeps every change made outside it — model
/// routing, tool pruning, the TTL pin — so a body that trips this costs one
/// turn's compression rather than the turn.
///
/// It costs less than that now. Putting the whole array back also reverted
/// messages nobody had complained about, including the opening ones — and
/// those are in the cached prefix. Measured over 09-02, the four turns that
/// took the wholesale restore are the four costliest proxy-caused re-caches
/// of the day: 573,940 of 622,325 wasted tokens, one of them dropping a
/// session's cache read from 267,681 to 17,238 in a single turn. So the
/// repair now names the messages that broke the invariant and puts back only
/// those, falling back to the whole array when it cannot map one array onto
/// the other or when the narrow repair does not settle it.
pub(super) fn restore_client_reasoning_blocks(
    body_to_send: bytes::Bytes,
    original: &bytes::Bytes,
    request_id: &str,
) -> bytes::Bytes {
    if body_to_send == original {
        return body_to_send;
    }
    // Cheap gate: nothing downstream matters for a body with no signed block,
    // and that is the overwhelming majority of them.
    const MARKER: &[u8] = b"thinking";
    if !original.windows(MARKER.len()).any(|w| w == MARKER) {
        return body_to_send;
    }

    let (Ok(before), Ok(mut after)) = (
        serde_json::from_slice::<serde_json::Value>(original),
        serde_json::from_slice::<serde_json::Value>(&body_to_send),
    ) else {
        return body_to_send;
    };

    let empty = Vec::new();
    let before_messages = before
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);
    let after_messages = after
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);
    if signed_reasoning_preserved(before_messages, after_messages) {
        return body_to_send;
    }

    let block_count = signed_reasoning_blocks(before_messages).len();
    let message_count = before_messages.len();
    let (restored, scope, restored_count) =
        match repair_signed_reasoning(before_messages, after_messages) {
            Some((messages, count)) => (messages, "offending_messages", count),
            None => (before_messages.clone(), "all_messages", message_count),
        };
    let Some(map) = after.as_object_mut() else {
        return body_to_send;
    };
    map.insert("messages".to_string(), serde_json::Value::Array(restored));
    match serde_json::to_vec(&after) {
        Ok(bytes) => {
            tracing::warn!(
                target: "headroom.proxy",
                event = "signed_reasoning_blocks_restored",
                request_id = %request_id,
                signed_blocks = block_count,
                messages_before = message_count,
                messages_restored = restored_count,
                restore_scope = scope,
                "outbound body altered the client's signed reasoning blocks; \
                 forwarding the client's copy of the messages that changed"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body_to_send,
    }
}

/// Put back only the messages whose signed reasoning the outbound chain broke.
///
/// Two things make a body unacceptable to Anthropic, and each names its own
/// messages: the last assistant message's signed blocks must arrive
/// unchanged, and no signed block may appear that the client did not send. A
/// message that merely *lost* a signed block breaks neither — that is
/// [`crate::compression::prior_thinking`] doing its job, and reverting it
/// would undo the saving for nothing.
///
/// Returns `None` when the arrays cannot be lined up index for index, or when
/// the narrow repair leaves the invariant still broken. The caller falls back
/// to the whole array on either.
pub(super) fn repair_signed_reasoning(
    before: &[serde_json::Value],
    after: &[serde_json::Value],
) -> Option<(Vec<serde_json::Value>, usize)> {
    if before.len() != after.len() {
        return None;
    }
    let last_assistant = before
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
    let sent = signed_reasoning_blocks(before);

    let mut targets = Vec::new();
    for index in 0..after.len() {
        let ours = signed_reasoning_blocks(std::slice::from_ref(&after[index]));
        let theirs = signed_reasoning_blocks(std::slice::from_ref(&before[index]));
        let last_assistant_changed = Some(index) == last_assistant && ours != theirs;
        let invented = ours.iter().any(|block| !sent.contains(block));
        if last_assistant_changed || invented {
            targets.push(index);
        }
    }
    if targets.is_empty() {
        return None;
    }

    let mut repaired = after.to_vec();
    for index in &targets {
        repaired[*index] = before[*index].clone();
    }
    if !signed_reasoning_preserved(before, &repaired) {
        return None;
    }
    Some((repaired, targets.len()))
}
