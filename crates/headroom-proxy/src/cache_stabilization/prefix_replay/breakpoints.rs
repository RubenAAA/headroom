//! Cache-breakpoint placement on replayed messages.
//!
//! Moved out of `prefix_replay.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

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
    relocate_ephemeral_blocks_counted(messages).0
}

/// How many `<system-reminder>` spans a whole message list carries.
///
/// Counts the opening tag wherever text sits — string content and any block
/// with a `text` field, every role — deliberately reaching wider than
/// relocation moves. The point is conservation: a span this proxy dropped,
/// wherever it sat, then shows up as `spans_out < spans_in` on the relocation
/// event. Four reminder-loss defects were each found days later from the
/// model's behaviour because nothing counted this.
///
/// One pass over text already parsed and in hand; nothing is serialised.
pub(super) fn count_ephemeral_spans(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|message| match message.get("content") {
            Some(Value::String(text)) => text.matches(SYSTEM_REMINDER_OPEN_TAG).count(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map_or(0, |text| text.matches(SYSTEM_REMINDER_OPEN_TAG).count())
                })
                .sum(),
            _ => 0,
        })
        .sum()
}

/// A message's content shape: `string`, `array`, or `absent`.
///
/// Named apart from [`block_type_shape`], which reports the block types inside
/// an array. Relocation cares only which of the two forms the newest message
/// arrived in, because a string tail has to be promoted before it can receive
/// anything and an `absent` one cannot receive at all.
pub(super) fn content_shape(message: &Value) -> &'static str {
    match message.get("content") {
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        _ => "absent",
    }
}

/// Fold a raided message's text kinds into the distinct set for the log.
pub(super) fn note_text_kinds(seen: &mut Vec<&'static str>, message: &Value) {
    for kind in text_block_kinds(message).split(',') {
        // Re-borrowed from the closed vocabulary rather than kept as an owned
        // string, so nothing user-controlled can reach the log by this route
        // even if `text_block_kinds` ever grows a case.
        let kind = match kind {
            "system-reminder" => "system-reminder",
            "plain+system-reminder" => "plain+system-reminder",
            "other-tag" => "other-tag",
            "plain" => "plain",
            _ => continue,
        };
        if !seen.contains(&kind) {
            seen.push(kind);
        }
    }
}

/// Role of a raided message, from a closed vocabulary.
pub(super) fn role_label(message: &Value) -> &'static str {
    match message.get("role").and_then(Value::as_str) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => "other",
    }
}

/// What one relocation pass did, for the log line.
///
/// Relocation has produced four separate reminder-loss defects — spans deleted
/// on retry turns, deleted on turns ending in an assistant message, duplicated
/// when inline, and lifted out of assistant messages leaving empty text blocks.
/// Each took days to find because the log said how many blocks moved, and only
/// on turns where something moved. Every field here answers one of those
/// questions from a single line.
#[derive(Debug, Default, Clone)]
pub struct RelocationReport {
    /// Spans appended to the newest message.
    pub blocks_moved: usize,
    /// Spans in the whole request before the pass, and after it. These must
    /// agree: relocation moves the client's scaffolding, it never removes it.
    pub spans_in: usize,
    pub spans_out: usize,
    /// Bytes of scaffolding lifted out of history.
    pub bytes_moved: usize,
    /// Which messages were raided, and the roles they held. Only a `user` turn
    /// may be raided — an `assistant` here is the regression that emptied text
    /// blocks the model itself had written.
    pub source_indices: Vec<usize>,
    pub source_roles: Vec<&'static str>,
    /// What the raided messages' text was, in [`text_block_kinds`]' closed
    /// vocabulary, distinct. `system-reminder` is the client's own block form;
    /// `plain+system-reminder` is the inline shape that was once sent twice.
    pub span_kinds: Vec<&'static str>,
    /// The destination's content shape as it arrived — the newest USER message,
    /// which is not always the newest one — and whether relocation had to give
    /// it block form before it could land anything.
    pub tail_shape: &'static str,
    pub tail_promoted: bool,
    /// Why nothing moved, `""` when something did. Set on the bail paths so a
    /// no-op is visible: until this existed a bail and a request with no
    /// scaffolding in it both wrote nothing at all.
    pub skip_reason: &'static str,
}

impl RelocationReport {
    /// A pass that never reached the messages — the caller's own bail.
    pub fn skipped(skip_reason: &'static str) -> Self {
        Self {
            skip_reason,
            ..Self::default()
        }
    }
}

/// As [`relocate_ephemeral_blocks`], plus the number of blocks it moved.
///
/// The count exists so the caller can log whether this ran at all. Without it
/// a turn that diverged anyway is indistinguishable from one where relocation
/// never fired, and both look the same in the log — which is exactly the hole
/// hit while attributing a 210k-token divergence on 2026-08-13.
pub fn relocate_ephemeral_blocks_counted(messages: Vec<Value>) -> (Vec<Value>, usize) {
    let (out, report) = relocate_ephemeral_blocks_reported(messages);
    (out, report.blocks_moved)
}

/// As [`relocate_ephemeral_blocks_counted`], with the full account of what
/// moved, from where, and what stopped it. See [`RelocationReport`].
pub fn relocate_ephemeral_blocks_reported(messages: Vec<Value>) -> (Vec<Value>, RelocationReport) {
    let mut report = RelocationReport {
        spans_in: count_ephemeral_spans(&messages),
        ..RelocationReport::default()
    };
    if messages.is_empty() {
        report.skip_reason = "empty_messages";
        return (messages, report);
    }
    // Scaffolding lands on the newest USER message, which is not always the
    // newest message. Gating the whole pass on the tail's role made the raid
    // conditional on something that alternates turn to turn: a request ending
    // in an assistant message left history alone, the next one ending in a user
    // message stripped it, and message 0 flipped between two forms inside the
    // cached prefix. Measured 2026-08-14 on the 07:14Z run: 8 passes moved a
    // block, every one of them out of index 0, and 7 cost a full re-cache —
    // 507,265 tokens, 31.5% of all creation. Same defect the content-shape gate
    // used to cause, same fix: what the pass does to history must not depend on
    // the tail.
    let Some(dest) = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        // The output is the input, so the conservation count is too. Counted
        // twice this would be a second full scan on a path every such request
        // takes.
        report.spans_out = report.spans_in;
        report.skip_reason = "no_user_message";
        return (messages, report);
    };
    report.tail_shape = content_shape(&messages[dest]);

    let mut messages = messages;
    let mut span_kinds: Vec<&'static str> = Vec::new();
    let mut collected: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    // Anything after the destination is an assistant turn: never a source, never
    // a recipient. It rides along untouched and is re-appended before the spans
    // are counted, so conservation still sees the whole request.
    let after = messages.split_off(dest + 1);
    // The destination is a source like every other user message. Excluding it
    // left the pass depending on where the destination SITS, which the tail-role
    // fix above did not reach. A conversation's first turn has message 0 as its
    // only user message, so message 0 was the destination and kept its
    // scaffolding; from the second turn the destination had moved forward and
    // the same blocks were lifted out of it. Message 0 therefore had two forms
    // in every conversation and the prefix died at its first block. Measured
    // 2026-08-14 on the capture-beta capture: inbound message 0 byte-identical
    // across both turns at 79,165 chars, forwarded blocks 2240/67929/6996/179
    // and then 1948/6996. Stripping the destination too and re-appending the
    // spans below costs nothing — they land past the breakpoint — and makes a
    // message's forwarded form independent of its distance from the tail.
    for (index, mut msg) in messages.into_iter().enumerate() {
        let is_dest = index == dest;
        // The destination has always been role-gated; the source was not. So a
        // reminder tag written by the MODEL — quoting it while discussing this
        // very code will do it — was lifted out of an assistant turn and the
        // block that held it left as `""`. The model read its own words back as
        // empty. Only the client puts scaffolding on a request, and it only
        // ever puts it on a user turn.
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            out.push(msg);
            continue;
        }
        // String content carries reminders inline, and used to pass straight
        // through here — invisible to relocation exactly as it was to the
        // comparison filter.
        if let Some(text) = msg
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            let Some((kept, spans)) = split_trailing_ephemeral_spans(&text) else {
                out.push(msg);
                continue;
            };
            report.source_indices.push(index);
            let role = role_label(&msg);
            if !report.source_roles.contains(&role) {
                report.source_roles.push(role);
            }
            note_text_kinds(&mut span_kinds, &msg);
            report.bytes_moved += spans.iter().map(String::len).sum::<usize>();
            collected.extend(
                spans
                    .into_iter()
                    .map(|s| serde_json::json!({"type": "text", "text": s})),
            );
            if kept.is_empty() && !is_dest {
                continue;
            }
            if let Some(obj) = msg.as_object_mut() {
                obj.insert("content".to_string(), Value::String(kept));
            }
            out.push(msg);
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            out.push(msg);
            continue;
        };
        if !blocks
            .iter()
            .any(|block| take_trailing_ephemeral_spans(block).is_some())
        {
            out.push(msg);
            continue;
        }
        report.source_indices.push(index);
        let role = role_label(&msg);
        if !report.source_roles.contains(&role) {
            report.source_roles.push(role);
        }
        note_text_kinds(&mut span_kinds, &msg);
        // Lift trailing spans rather than whole blocks. A block that ends with
        // a reminder but carries real text before it would otherwise leave with
        // the text still attached — and a block whose reminder sits mid-prose
        // is not scaffolding at all, so it is not touched.
        let mut keep: Vec<Value> = Vec::with_capacity(blocks.len());
        for block in blocks.iter() {
            let Some((kept_block, spans)) = take_trailing_ephemeral_spans(block) else {
                keep.push(block.clone());
                continue;
            };
            report.bytes_moved += spans.iter().map(String::len).sum::<usize>();
            collected.extend(
                spans
                    .into_iter()
                    .map(|s| serde_json::json!({"type": "text", "text": s})),
            );
            if let Some(kept_block) = kept_block {
                keep.push(kept_block);
            }
        }
        // A message that was nothing but scaffolding leaves with it. Emptying
        // its content instead would be rejected by the API, and keeping the
        // message is what let this churn survive block-level relocation: the
        // client drops the whole message a turn later, every index after it
        // shifts, and the prefix dies there. Dropping it is safe because the
        // client's own next request is that same sequence without it.
        // The destination is exempt: its spans are re-appended a few lines down,
        // so emptying it here would drop the very message they land on.
        if keep.is_empty() && !is_dest {
            continue;
        }
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(keep));
        }
        out.push(msg);
    }
    if collected.is_empty() {
        // Nothing was mutated, so the output is the input and its span count
        // with it. Same reason as the `no_user_message` path: no second scan.
        out.extend(after);
        report.spans_out = report.spans_in;
        report.skip_reason = "nothing_to_move";
        return (out, report);
    }
    report.span_kinds = span_kinds;
    let moved = collected.len();
    // Give a string-content tail block form so it can receive the scaffolding.
    // Only when there is something to move, so an ordinary turn keeps its bytes
    // byte-for-byte as the client sent them.
    if let Some(tail_msg) = out.last_mut()
        && let Some(text) = tail_msg.get("content").and_then(Value::as_str)
    {
        let text = text.to_string();
        let mut blocks = Vec::with_capacity(1);
        if !text.is_empty() {
            blocks.push(serde_json::json!({"type": "text", "text": text}));
        }
        if let Some(obj) = tail_msg.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(blocks));
        }
        report.tail_promoted = true;
    }
    if let Some(blocks) = out
        .last_mut()
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
    {
        blocks.extend(collected);
        report.blocks_moved = moved;
        out.extend(after);
        report.spans_out = count_ephemeral_spans(&out);
        return (out, report);
    }
    // No block-style destination to land on: the blocks were lifted out of
    // history but have nowhere to go, so report zero moved rather than claiming
    // a relocation that did not happen. The spans are gone from the output,
    // which is what `spans_out < spans_in` is there to announce.
    report.skip_reason = "no_block_tail";
    out.extend(after);
    report.spans_out = count_ephemeral_spans(&out);
    (out, report)
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
    place_tail_cache_breakpoints(messages, 1, false).0
}

/// One `cache_control` position: which message, and which block within it.
#[derive(Clone, Copy)]
pub(super) struct CacheTarget {
    pub(super) message_idx: usize,
    pub(super) block_idx: usize,
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
/// slots are already spoken for. Ask [`message_slots_within_budget`] first.
///
/// `scaffold` adds one more marker on the opening scaffolding, on top of
/// `tail_slots` rather than out of it — see [`opening_scaffolding_target`]. Ask
/// [`message_slots_within_budget`] whether there is room for it.
///
/// # Why one tail slot is survivable
///
/// A single tail marker is the default, and the budget also falls back to it
/// when `tools` has taken a slot. That is safe because Anthropic writes a cache
/// entry only where a breakpoint says to, then looks back roughly 20 blocks from
/// a miss to find one it already holds. A recent turn of this client adds at
/// most 10 blocks, measured, so the lookback still reaches the entry the
/// previous turn wrote and the read starts there rather than at nothing.
///
/// The second slot is a hedge against a longer jump, not a requirement. That is
/// the margin the scaffolding marker is allowed to spend when the budget leaves
/// no other way to pay for it.
pub fn place_tail_cache_breakpoints(
    messages: Vec<Value>,
    tail_slots: usize,
    scaffold: bool,
) -> (Vec<Value>, usize) {
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
    //
    // Message 0 is never a seal either (`i > 0` below), measured 2026-08-17. On
    // the first turn the latest user message IS message 0, and Claude Code's
    // opener carries `<system-reminder>` blocks, so the seal landed on the very
    // first block and stranded everything after it: 16,971 bytes forwarded
    // outside the cached prefix, billed as 5,959 fresh input tokens where the
    // base client billed 18. It bought nothing — creation came out the same
    // either way (57,993 against the client's 57,895), because `system` and
    // `tools` markers were already writing that prefix — so it was 4.4 points of
    // an 11.5% loss against an unproxied client, paid for no benefit.
    //
    // The seal exists to keep a withdrawn reminder out of an ALREADY cached
    // prefix. On turn one nothing is cached yet, so there is nothing to protect,
    // and from turn two the reminder is held steady by prefix replay, whose
    // append-only guard is deliberately blind to reminder churn.
    let mut sealed = false;
    let final_message = messages.len().saturating_sub(1);
    let latest_user_message = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"));

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
                    if i > 0
                        && (i == final_message || latest_user_message == Some(i))
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
        } else if let Some(text) = msg
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
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
            let final_ephemeral = i > 0
                && (i == final_message || latest_user_message == Some(i))
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

    // One extra marker for the shared opening scaffolding. Claude Code's message
    // 0 starts with the same `<system-reminder>` blocks in every session of a
    // project, so a breakpoint on the last of them names a prefix — system,
    // tools, scaffolding — that every session of that project reads instead of
    // writes. Its slot comes out of `system`, not out of the tail; the caller
    // settles that before saying `true` here.
    let scaffold_target = if scaffold {
        opening_scaffolding_target(&out)
    } else {
        None
    };

    // Re-place the breakpoints on the latest ordinary blocks, newest first. A
    // proactive expansion is a one-time tail and must never become the cache
    // target: doing so converts its first appearance into a cache write.
    let mut placed = 0usize;
    for target in scaffold_target
        .iter()
        .chain(cacheable_targets.iter().rev().take(tail_slots))
    {
        let CacheTarget {
            message_idx,
            block_idx,
        } = *target;
        if let Some(content) = out[message_idx]
            .get_mut("content")
            .and_then(|c| c.as_array_mut())
            && let Some(Value::Object(block)) = content.get_mut(block_idx)
        {
            // A short message 0 can be both the scaffold target and the
            // newest ordinary one. Counting the second write would report a
            // marker that is not there and, upstream, licence a `system`
            // strip on a budget that was never spent.
            let already_marked = block
                .insert(
                    "cache_control".to_string(),
                    serde_json::json!({"type": "ephemeral"}),
                )
                .is_some();
            if !already_marked {
                placed += 1;
            }
        }
    }
    (out, placed)
}

/// The last block of the opening run of client scaffolding in message 0, if
/// message 0 opens with one.
///
/// Claude Code's first user message begins with `<system-reminder>` blocks that
/// are a function of the project, not of the session: the same bytes arrive with
/// every new session under the same working directory. A breakpoint here caches
/// `system` + `tools` + those blocks as one prefix, which the next session reads
/// rather than writes.
///
/// The run has to be at the very head. A session whose recall was injected in
/// front of the scaffolding — every conversation the proxy stored before the
/// injector learned to sit behind it — has no run at index 0 and gets nothing,
/// which is what keeps its already-cached message 0 exactly as the provider
/// holds it.
pub(super) fn opening_scaffolding_target(messages: &[Value]) -> Option<CacheTarget> {
    let first = messages.first()?;
    if first.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let blocks = first.get("content").and_then(Value::as_array)?;
    let run = blocks
        .iter()
        .take_while(|block| is_ephemeral_client_block(block))
        .count();
    if run == 0 {
        return None;
    }
    Some(CacheTarget {
        message_idx: 0,
        block_idx: run - 1,
    })
}

/// Whether message 0 opens with client scaffolding, and so whether
/// [`place_tail_cache_breakpoints`] has somewhere to put a scaffolding marker.
///
/// Answered from the first block alone, before normalization, because the marker
/// budget has to be settled before placement runs. String content is wrapped
/// into a single block by placement, so it is read the same way here.
pub fn opens_with_scaffolding(messages: &[Value]) -> bool {
    let Some(first) = messages.first() else {
        return false;
    };
    if first.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match first.get("content") {
        Some(Value::Array(blocks)) => blocks.first().is_some_and(is_ephemeral_client_block),
        Some(Value::String(text)) => is_ephemeral_client_text(text),
        _ => false,
    }
}

/// The fewest `system` breakpoints worth keeping.
///
/// One names the whole `system` prefix. Claude Code's second only shortens the
/// span the first already covers, and the scaffolding breakpoint sits
/// immediately behind `system` and names that span itself, so the second buys
/// nothing once the scaffolding marker is there. Going to zero is different: it
/// leaves `system` and `tools` with no checkpoint of their own ahead of message
/// 0, which is [`strip_system_cache_control`]'s business and gated on its flag.
pub(super) const SYSTEM_MARKERS_KEPT: usize = 1;

/// How the provider's four `cache_control` slots are divided for one request.
pub struct MessageSlots {
    /// Tail breakpoints to place, counting back from the newest message.
    pub tail: usize,
    /// Whether one more may go on the opening scaffolding.
    pub scaffold: bool,
    /// Slots `system` and `tools` keep, once `system` has given up what it can.
    pub reserved: usize,
}

/// Divide the four slots, letting `system` yield before the tail does.
///
/// The tail pair is the part that must not shrink. Anthropic writes a cache
/// entry only where a breakpoint says to, and looks a short way back from a
/// miss, so the older of the two tail markers is what lets a turn whose tail was
/// edited still read from the message before it. A second `system` marker cannot
/// do that job, and with the scaffolding marker sitting right behind `system` it
/// is not doing any other job either — so it is the one that goes.
///
/// The scaffolding slot yields next, ahead of the tail, when even the shortened
/// `system` plus `tools` leaves no room. And `system` is only asked to give up
/// its marker when the scaffolding marker actually materialises — otherwise the
/// freed slot goes unused and the request simply loses a checkpoint. Every other
/// division is exactly what it was before the scaffolding marker existed.
pub fn message_slots_within_budget(
    body: &Value,
    messages: &[Value],
    requested_tail: usize,
) -> MessageSlots {
    let tools = count_field_markers(body, "tools");
    let system = count_field_markers(body, "system");
    // Only from two tail slots up. With one the breakpoint belongs at the tail,
    // where it caches the whole conversation; a scaffolding marker on its own
    // would trade the entire history for the opener.
    let wanted = requested_tail >= 2 && opens_with_scaffolding(messages);
    // One marker, and never the last one. The instruction is to give up Claude
    // Code's second `system` breakpoint, not to take the field over.
    let shortened = if system > SYSTEM_MARKERS_KEPT {
        system - 1
    } else {
        system
    };

    let divide = |system_kept: usize| {
        let reserved = system_kept + tools;
        let free = ANTHROPIC_CACHE_CONTROL_LIMIT.saturating_sub(reserved);
        let tail = requested_tail.min(free);
        MessageSlots {
            tail,
            scaffold: wanted && tail >= 2 && free > tail,
            reserved,
        }
    };

    let paid_for = divide(shortened);
    if paid_for.scaffold {
        paid_for
    } else {
        divide(system)
    }
}

/// Give up `system` breakpoints, last one first, until the request fits the
/// provider's limit. Returns how many it removed.
///
/// This is the enforcing half of [`message_slots_within_budget`], run against
/// the markers really placed rather than the ones planned, so a plan that did
/// not come off cannot leave the request over the limit. Never goes below
/// [`SYSTEM_MARKERS_KEPT`].
///
/// `on_messages` is [`place_tail_cache_breakpoints`]'s own count, which is exact:
/// it strips every marker it finds on the content blocks before placing its own.
/// Markers a client hung on a message *object* are not counted, the same
/// omission the budget has always made.
pub fn trim_system_breakpoints_to_budget(body: &mut Value, on_messages: usize) -> usize {
    let tools = count_field_markers(body, "tools");
    let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut system = blocks
        .iter()
        .filter(|b| b.get("cache_control").is_some())
        .count();
    let mut removed = 0;
    while system > SYSTEM_MARKERS_KEPT
        && system + tools + on_messages > ANTHROPIC_CACHE_CONTROL_LIMIT
    {
        let Some(block) = blocks
            .iter_mut()
            .rev()
            .find(|b| b.get("cache_control").is_some())
            .and_then(Value::as_object_mut)
        else {
            break;
        };
        block.remove("cache_control");
        system -= 1;
        removed += 1;
    }
    removed
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

/// `cache_control` markers on the objects of a top-level array field. `system`
/// blocks and `tools` entries both carry theirs as a direct key, and a `system`
/// sent as a plain string has none.
pub(super) fn count_field_markers(body: &Value, field: &str) -> usize {
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
        if let Value::Object(obj) = block
            && obj.remove("cache_control").is_some()
        {
            removed += 1;
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
pub(super) fn estimate_message_tokens(messages: &[Value]) -> Vec<u64> {
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
