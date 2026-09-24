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
    assert!(tail[1]["text"]
        .as_str()
        .unwrap()
        .contains("<system-reminder>"));
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
    assert!(tail[1]["text"]
        .as_str()
        .unwrap()
        .contains("<system-reminder>"));
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

// ── the relocation report ───────────────────────────────────────────

/// The conservation check, and the fields that say where the spans came
/// from. Four reminder-loss defects were each found from the model behaving
/// oddly turns later; this is what a single log line has to answer instead.
#[test]
fn a_relocated_request_accounts_for_every_span() {
    let msgs = vec![
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>one</system-reminder>"}]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
        json!({"role": "user", "content": [
                {"type": "text", "text": "prose\n\n<system-reminder>two</system-reminder>"}]}),
        text_msg("user", "newest"),
    ];
    let (out, report) = relocate_ephemeral_blocks_reported(msgs);
    assert_eq!(report.spans_in, 2);
    assert_eq!(report.spans_out, 2, "spans are moved, never dropped");
    assert_eq!(report.blocks_moved, 2);
    assert_eq!(report.skip_reason, "");
    assert_eq!(report.source_indices, vec![0, 2]);
    assert_eq!(report.source_roles, vec!["user"]);
    assert_eq!(
        report.span_kinds,
        vec!["system-reminder", "plain+system-reminder"]
    );
    assert_eq!(
        report.bytes_moved,
        "<system-reminder>one</system-reminder>".len()
            + "<system-reminder>two</system-reminder>".len()
    );
    assert_eq!(report.tail_shape, "array");
    assert!(!report.tail_promoted);
    assert_eq!(out.last().unwrap()["content"].as_array().unwrap().len(), 3);
}

/// A bail has to be visible. Reported as nothing at all — which is what the
/// event did until it carried `skip_reason` — it looks exactly like a
/// request with no scaffolding in it.
#[test]
fn a_no_op_relocation_names_why_it_bailed() {
    let no_user_at_all =
        vec![json!({"role": "assistant", "content": [{"type": "text", "text": "prefill"}]})];
    let (_, report) = relocate_ephemeral_blocks_reported(no_user_at_all);
    assert_eq!(report.skip_reason, "no_user_message");

    let nothing_to_move = vec![text_msg("user", "opener"), text_msg("user", "newest")];
    let (_, report) = relocate_ephemeral_blocks_reported(nothing_to_move);
    assert_eq!(report.skip_reason, "nothing_to_move");
    assert_eq!(report.spans_in, 0);
    assert_eq!(report.spans_out, 0);

    let (_, report) = relocate_ephemeral_blocks_reported(Vec::new());
    assert_eq!(report.skip_reason, "empty_messages");
}

/// Message 0 has to read the same whether or not it is the destination. It
/// IS the destination on a conversation's first turn, being the only user
/// message there, and stops being one as soon as the conversation grows — so
/// sparing the destination gave message 0 two forms and killed the prefix at
/// its first block. Measured 2026-08-14 on the capture-beta capture: forwarded
/// blocks 2240/67929/6996/179 on turn 1 and 1948/6996 on turn 2, from an
/// inbound message that was byte-identical both times.
#[test]
fn message_zero_reads_the_same_whether_or_not_it_is_the_destination() {
    // The reminder trails INSIDE the first block, which is the shape the
    // capture showed. A reminder in a block of its own would survive the old
    // behaviour untouched and prove nothing.
    let opener = json!({"role": "user", "content": [
            {"type": "text", "text": "opener<system-reminder>x</system-reminder>"}]});

    let (turn_one, report) = relocate_ephemeral_blocks_reported(vec![opener.clone()]);
    assert_eq!(
        report.source_indices,
        vec![0],
        "the destination is a source"
    );
    assert_eq!(
        report.spans_out, report.spans_in,
        "the span is moved, not lost"
    );

    let (turn_two, _) = relocate_ephemeral_blocks_reported(vec![
        opener,
        json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
        text_msg("user", "newest"),
    ]);

    assert_eq!(
        turn_one[0]["content"][0], turn_two[0]["content"][0],
        "message 0 leads with the same block on both turns"
    );
    assert_eq!(turn_one[0]["content"][0]["text"], "opener");
}

/// The newest user turn is stored with its reminders, not held back.
///
/// This used to be capped out of the stored prefix, because relocation would
/// strip its spans once the conversation grew past it and the fat stored
/// copy then read as an edit inside the cached prefix — 24,565 tokens on one
/// turn, 2026-08-14. Nothing rewrites those spans now, and storing the
/// message is what makes the client's own withdrawal of a reminder
/// harmless: the guard sees it and declines, so the model never reads a
/// reminder the client dropped.
#[test]
fn the_newest_user_turn_is_stored_with_its_reminders() {
    let landed = vec![
        text_msg("user", "opener"),
        json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
        json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1"},
                {"type": "text", "text": "prose"},
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
    ];
    assert_eq!(replayable_stored_prefix_len(&landed), 3);

    // An assistant prefill after it changes nothing: it carries real content
    // of its own, so the trailing scan keeps both.
    let mut prefilled = landed.clone();
    prefilled.push(json!({"role": "assistant", "content": [{"type": "text", "text": "prefill"}]}));
    assert_eq!(replayable_stored_prefix_len(&prefilled), 4);

    // Pure scaffolding at the end is still trimmed. It was never in the
    // provider's cached prefix, so holding it would make its replacement
    // next turn look like an edit.
    let mut trailing_scaffolding = landed.clone();
    trailing_scaffolding.push(json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>y</system-reminder>"}]}));
    assert_eq!(replayable_stored_prefix_len(&trailing_scaffolding), 3);
}

/// The defect that made this pass the single largest source of re-cached
/// tokens: what it did to HISTORY depended on the tail's role. A request
/// ending in an assistant prefill left message 0 alone, the next one ending
/// in a user turn stripped it, and message 0 alternated between two forms
/// inside the cached prefix. Measured 2026-08-14: 7 re-caches, 507,265
/// tokens, every pass raiding index 0.
#[test]
fn history_is_raided_the_same_whatever_the_tail_is() {
    let history = || {
        vec![
            json!({"role": "user", "content": [
                    {"type": "text", "text": "opener<system-reminder>x</system-reminder>"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "newest"}]}),
        ]
    };
    let (user_tail, user_report) = relocate_ephemeral_blocks_reported(history());

    let mut with_prefill = history();
    with_prefill
        .push(json!({"role": "assistant", "content": [{"type": "text", "text": "prefill"}]}));
    let (assistant_tail, assistant_report) = relocate_ephemeral_blocks_reported(with_prefill);

    assert_eq!(user_report.source_indices, vec![0]);
    assert_eq!(
        user_report.source_indices, assistant_report.source_indices,
        "the tail's role must not decide whether history is raided"
    );
    assert_eq!(user_report.bytes_moved, assistant_report.bytes_moved);
    assert_eq!(
        user_tail[..3],
        assistant_tail[..3],
        "message 0 has to forward identically on both turns"
    );
    assert_eq!(assistant_report.spans_in, assistant_report.spans_out);
    assert_eq!(
        assistant_tail.last().unwrap()["content"][0]["text"],
        "prefill",
        "the trailing assistant message rides along untouched"
    );
}

/// A string tail has to be given block form before it can take anything,
/// and that promotion is itself a rewrite of the client's bytes.
#[test]
fn a_promoted_string_tail_is_reported_as_one() {
    let msgs = vec![
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
        json!({"role": "user", "content": "newest"}),
    ];
    let (_, report) = relocate_ephemeral_blocks_reported(msgs);
    assert_eq!(report.tail_shape, "string");
    assert!(report.tail_promoted);
    assert_eq!(report.spans_in, report.spans_out);
}

/// Spans lifted with nowhere to land are gone from the request. Behaviour
/// unchanged — the point is that the count now says so, instead of the loss
/// surfacing as the model ignoring instructions it was never shown.
#[test]
fn spans_lifted_with_nowhere_to_land_show_up_as_lost() {
    let msgs = vec![
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}),
        json!({"role": "user", "content": {"not": "a shape we can append to"}}),
    ];
    let (_, report) = relocate_ephemeral_blocks_reported(msgs);
    assert_eq!(report.skip_reason, "no_block_tail");
    assert_eq!(report.tail_shape, "absent");
    assert_eq!(report.spans_in, 1);
    assert_eq!(report.spans_out, 0, "the span left the request");
    assert_eq!(report.blocks_moved, 0);
}

/// The role gate, from the report's side: an `assistant` in `source_roles`
/// is the regression that emptied blocks the model itself wrote.
#[test]
fn model_output_is_never_a_reported_source() {
    let msgs = vec![
        json!({"role": "assistant", "content": [
                {"type": "text", "text": "I wrote <system-reminder>x</system-reminder>"}]}),
        text_msg("user", "newest"),
    ];
    let (_, report) = relocate_ephemeral_blocks_reported(msgs);
    assert!(report.source_roles.is_empty());
    assert_eq!(report.spans_in, report.spans_out);
}

// ── divergence_text_heads ───────────────────────────────────────────

/// `first_diff_path` says WHERE; this says WHAT. Without it the 2026-08-13
/// divergence had to be inferred from message shapes.
#[test]
fn divergence_heads_show_the_text_on_each_side() {
    let stored = vec![text_msg("user", "read the file")];
    let current = vec![text_msg("user", "read the file now")];
    let (head_stored, head_current) = divergence_text_heads(&stored, &current, 0).unwrap();
    assert_eq!(head_stored, "read the file");
    assert_eq!(head_current, "read the file now");
}

/// A newline must print as `\n`, not as a break in the log line, and a long
/// message must not write the whole conversation into it.
#[test]
fn divergence_heads_are_escaped_and_truncated() {
    let stored = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "line\tone\nline two"}]})];
    let current = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "x".repeat(500)}]})];
    let (head_stored, head_current) = divergence_text_heads(&stored, &current, 0).unwrap();
    assert_eq!(head_stored, "line\\tone\\nline two");
    assert!(!head_current.contains('\n'));
    assert_eq!(
        head_current.chars().count(),
        DIFF_TEXT_HEAD_CHARS + 1,
        "head plus the ellipsis that marks the cut"
    );
}

/// A block that came or went is not a text difference, and the shape fields
/// already report it. Better empty than a head from the wrong block.
#[test]
fn divergence_heads_are_empty_when_a_block_came_or_went() {
    let stored = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "a"}, {"type": "text", "text": "b"}]})];
    let current = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "a"}]})];
    let (head_stored, head_current) = divergence_text_heads(&stored, &current, 0).unwrap();
    assert_eq!(head_stored, "");
    assert_eq!(head_current, "");
}

#[test]
fn divergence_heads_are_none_when_the_messages_agree() {
    let msgs = vec![text_msg("user", "same")];
    assert!(divergence_text_heads(&msgs, &msgs, 0).is_none());
    assert!(divergence_text_heads(&msgs, &msgs, 9).is_none());
}

// ── relocation rewrites bytes, so it stays conservative ─────────────

/// The model quoting the tag is not scaffolding.
///
/// Observed live on 2026-08-14: an assistant turn discussing this file
/// contained the literal tag, relocation lifted it out of the middle of the
/// prose, and the block that held it was left as `""`. The model read its
/// own words back as empty. The destination was role-gated and the source
/// was not.
#[test]
fn an_assistant_message_is_never_a_relocation_source() {
    let msgs = vec![
        json!({"role": "assistant", "content": [
                {"type": "text", "text":
                    "I wrote <system-reminder>foo</system-reminder> in the test case"}]}),
        json!({"role": "assistant", "content":
                "and <system-reminder>bar</system-reminder> here too"}),
        text_msg("user", "newest"),
    ];
    assert_eq!(
        relocate_ephemeral_blocks(msgs.clone()),
        msgs,
        "model output is returned byte-identical"
    );
}

/// A span in the middle of prose is prose. Lifting it is what emptied the
/// block above, and it would mangle the sentence even when it did not.
#[test]
fn a_reminder_in_mid_prose_leaves_the_message_alone() {
    let msgs = vec![
        json!({"role": "user", "content": [
                {"type": "text", "text":
                    "before <system-reminder>x</system-reminder> after"}]}),
        json!({"role": "user", "content":
                "before <system-reminder>y</system-reminder> after"}),
        text_msg("user", "newest"),
    ];
    assert_eq!(relocate_ephemeral_blocks(msgs.clone()), msgs);
}

/// The shape the client actually sends still moves: a whole reminder block,
/// and a reminder appended to the end of a message's text.
#[test]
fn trailing_reminders_still_relocate() {
    let msgs = vec![
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>whole</system-reminder>"}]}),
        json!({"role": "user", "content": "prose\n\n<system-reminder>appended</system-reminder>"}),
        text_msg("user", "newest"),
    ];
    let out = relocate_ephemeral_blocks(msgs);
    assert_eq!(
        out[0]["content"].as_array().unwrap().len(),
        1,
        "the whole-block reminder left history"
    );
    assert_eq!(
        out[1]["content"],
        json!("prose"),
        "the appended one left too"
    );
    let tail = out[2]["content"].as_array().unwrap();
    assert_eq!(tail.len(), 3, "both ride on the newest message");
    assert!(tail[1]["text"].as_str().unwrap().contains("whole"));
    assert!(tail[2]["text"].as_str().unwrap().contains("appended"));
}

/// Every scrap of message text, scaffolding removed, whitespace collapsed.
fn prose(messages: &[Value]) -> String {
    let mut out = String::new();
    let mut push = |text: &str| {
        out.push(' ');
        out.push_str(&split_ephemeral_spans(text).0);
    };
    for message in messages {
        match message.get("content") {
            Some(Value::String(text)) => push(text),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        push(text);
                    }
                }
            }
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Relocation may move spans and may drop a block that held nothing else.
/// It may never lose a character of prose.
#[test]
fn relocation_loses_no_prose() {
    let msgs = vec![
        json!({"role": "user", "content": [
                {"type": "text", "text": "opener"},
                {"type": "text", "text": "<system-reminder>whole</system-reminder>"}]}),
        json!({"role": "assistant", "content": [
                {"type": "text", "text":
                    "I wrote <system-reminder>quoted</system-reminder> in the test"}]}),
        json!({"role": "user", "content":
                "mid <system-reminder>embedded</system-reminder> prose"}),
        json!({"role": "user", "content": "tail text\n<system-reminder>appended</system-reminder>"}),
        json!({"role": "user", "content": "newest"}),
    ];
    let out = relocate_ephemeral_blocks(msgs.clone());
    assert_eq!(prose(&msgs), prose(&out), "no text is lost in the move");
    assert_eq!(
        reminder_spans(&msgs),
        reminder_spans(&out),
        "and no span is lost or duplicated"
    );
}

// ── reminder conservation across the forward path ───────────────────

/// Every `<system-reminder>` span in a message list, sorted.
///
/// Walks every string rather than the known content shapes: a span that
/// moved between block form and string sugar must still be counted, or the
/// check passes by looking in the wrong place.
fn reminder_spans(messages: &[Value]) -> Vec<String> {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.extend(split_ephemeral_spans(text).1),
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            Value::Object(map) => map.values().for_each(|value| walk(value, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for message in messages {
        walk(message, &mut out);
    }
    out.sort();
    out
}

/// The forward path in the order `proxy.rs` runs it: the overlay, then
/// breakpoint placement. Compression is the identity here — what is under
/// test is what the replay layer does to the client's content, not what the
/// compressor does to it.
///
/// No relocation stage. The client's `<system-reminder>` spans go out where
/// the client put them, so these tests read the same path production does.
fn forward(
    inbound: &[Value],
    previous: Option<&(Vec<Value>, Vec<Value>)>,
) -> (Vec<Value>, Option<ReplaySkip>) {
    let (overlaid, skip) = overlay_cached_prefix_reported(
        inbound.to_vec(),
        inbound,
        previous.map(|(original, _)| original.as_slice()),
        previous.map(|(_, forwarded)| forwarded.as_slice()),
        true,
        None,
    );
    (place_tail_cache_breakpoints(overlaid, 1, false).0, skip)
}

/// What the store holds after a turn: the inbound messages and the bytes
/// that went out.
fn stored_turn(inbound: &[Value]) -> (Vec<Value>, Vec<Value>) {
    (inbound.to_vec(), forward(inbound, None).0)
}

#[track_caller]
fn assert_reminders_conserved(inbound: &[Value], forwarded: &[Value]) {
    assert_eq!(
        reminder_spans(inbound),
        reminder_spans(forwarded),
        "every reminder the client sent must go out exactly once"
    );
}

/// The stored prefix carries the reminder relocation put on its newest
/// message, and the replay strips it from there. The current turn has to be
/// the thing that re-supplies it.
#[test]
fn replayed_turn_keeps_the_reminder_it_relocated() {
    let turn_n = vec![
        text_msg("user", "opener"),
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>a</system-reminder>"}]}),
    ];
    let stored = stored_turn(&turn_n);
    assert_eq!(reminder_spans(&stored.1).len(), 1, "stored bytes carry it");

    let mut turn_n1 = turn_n.clone();
    turn_n1.push(text_msg("assistant", "reply"));
    turn_n1.push(text_msg("user", "newest"));
    let (forwarded, skip) = forward(&turn_n1, Some(&stored));
    assert_eq!(skip, None, "the prefix replays");
    assert_reminders_conserved(&turn_n1, &forwarded);
}

/// A turn no longer than the stored prefix — a client retry of an unchanged
/// turn is exactly this shape. `optimized[n..]` is empty, so nothing
/// re-supplies the spans the strip takes out of the stored prefix.
#[test]
fn turn_no_longer_than_the_stored_prefix_keeps_its_reminders() {
    let turn = vec![
        text_msg("user", "opener"),
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>a</system-reminder>"}]}),
    ];
    let stored = stored_turn(&turn);
    let (forwarded, _) = forward(&turn, Some(&stored));
    assert_reminders_conserved(&turn, &forwarded);
}

/// A declined replay forwards this turn's own bytes, so nothing it carries
/// can go missing.
#[test]
fn declined_turn_keeps_its_reminders() {
    let stored = stored_turn(&[text_msg("user", "opener"), text_msg("assistant", "reply")]);
    let turn = vec![
        text_msg("user", "a different opener"),
        json!({"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>a</system-reminder>"},
                {"type": "text", "text": "more"}]}),
        text_msg("user", "newest"),
    ];
    let (forwarded, skip) = forward(&turn, Some(&stored));
    assert!(skip.is_some(), "the prefix diverged");
    assert_reminders_conserved(&turn, &forwarded);
}

/// A reminder sitting mid-text rather than in its own block. The stored
/// prefix holds it inline, because the turn that produced those bytes ended
/// with an assistant message and relocation declined there.
#[test]
fn inline_reminder_is_not_duplicated_by_the_replay() {
    let turn_n = vec![
        json!({"role": "user", "content": [
                {"type": "text", "text": "do the thing\n\n<system-reminder>x</system-reminder>"}]}),
        text_msg("assistant", "reply"),
    ];
    let stored = stored_turn(&turn_n);

    let mut turn_n1 = turn_n.clone();
    turn_n1.push(text_msg("user", "newest"));
    let (forwarded, skip) = forward(&turn_n1, Some(&stored));
    assert_eq!(skip, None, "the prefix replays");
    assert_reminders_conserved(&turn_n1, &forwarded);
}

/// A tail with string content. Relocation promotes it to block form so it
/// can take the scaffolding; nothing may be lost in the promotion.
#[test]
fn string_content_tail_keeps_the_reminders_moved_onto_it() {
    let turn = vec![
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>a</system-reminder>"}]}),
        json!({"role": "user", "content": "newest"}),
    ];
    let (forwarded, _) = forward(&turn, None);
    assert_reminders_conserved(&turn, &forwarded);
}

/// A reminder on a message that is not the last one, on a turn whose last
/// message is an assistant message. Relocation declines there — no user
/// message at the end to move the span to — so the span is still sitting
/// inside the region the replay overwrites.
#[test]
fn reminder_in_history_survives_a_turn_ending_in_an_assistant_message() {
    let turn_n = vec![
        text_msg("user", "opener"),
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "out"},
                {"type": "text", "text": "<system-reminder>a</system-reminder>"}]}),
        text_msg("user", "newest"),
    ];
    let stored = stored_turn(&turn_n);

    let mut turn_n1 = turn_n.clone();
    turn_n1.push(text_msg("assistant", "reply"));
    let (forwarded, _) = forward(&turn_n1, Some(&stored));
    assert_reminders_conserved(&turn_n1, &forwarded);
}

/// Several reminders, on several history messages, in one request — block
/// form, inline in an assistant message, and inline in string sugar.
#[test]
fn many_reminders_across_history_all_reach_the_wire() {
    let turn = vec![
        json!({"role": "user", "content": [
                {"type": "text", "text": "opener"},
                {"type": "text", "text": "<system-reminder>a</system-reminder>"}]}),
        json!({"role": "assistant", "content": [
                {"type": "text", "text": "reply <system-reminder>b</system-reminder>"}]}),
        json!({"role": "user", "content": "third <system-reminder>c</system-reminder> turn"}),
        text_msg("user", "newest"),
    ];
    let (forwarded, _) = forward(&turn, None);
    assert_reminders_conserved(&turn, &forwarded);
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
    //
    // The reminder-bearing message sits at index 1, not 0. Message 0 is
    // deliberately exempt from sealing — Claude Code's opener begins with a
    // reminder block, and sealing there stranded the entire message array
    // outside the cached prefix (see the note in
    // `place_tail_cache_breakpoints`). This test is about a reminder in the
    // live TAIL, which is what it always meant.
    let msgs = vec![
        text_msg("user", "opener"),
        json!({"role": "user", "content": [
                {"type": "tool_result", "content": "output"},
                reminder(),
                {"type": "text", "text": "trailing"}]}),
    ];
    let out = normalize_message_cache_control(msgs);
    let blocks = out[1]["content"].as_array().unwrap();
    assert!(blocks[0].get("cache_control").is_some());
    assert!(blocks[1].get("cache_control").is_none());
    assert!(
        blocks[2].get("cache_control").is_none(),
        "a block after the reminder must not be the cache target"
    );
}

/// The production shape that made message 0 an exception, measured
/// 2026-08-17. Claude Code's first user message opens with a reminder block
/// carrying CLAUDE.md, so sealing on it left nothing in `messages` cacheable
/// and 16,971 bytes billed as fresh input where the client billed none.
#[test]
fn an_opening_reminder_does_not_strand_the_first_turn() {
    let msgs = vec![json!({"role": "user", "content": [
                reminder(),
                {"type": "text", "text": "the actual question"}]})];
    let (out, placed) = place_tail_cache_breakpoints(msgs, 1, false);
    assert_eq!(placed, 1, "turn one must still get a breakpoint");
    let blocks = out[0]["content"].as_array().unwrap();
    assert!(
        blocks[1].get("cache_control").is_some(),
        "the marker belongs on the real question, after the opening reminder"
    );
}

#[test]
fn latest_user_reminder_seals_an_assistant_ended_tool_turn() {
    // Live 2026-08-13 shape: the newest user message carried transient
    // scaffolding, then an assistant tool-use message ended the request.
    // Caching through the assistant also cached the reminder and caused a
    // 6,434-token rebuild as soon as the client withdrew it.
    // Indices are shifted by one opener: message 0 is exempt from sealing,
    // so the shape this test is about — a reminder on the newest user
    // message, mid-conversation — has to live where it really lives.
    let msgs = vec![
        text_msg("user", "opener"),
        json!({"role": "user", "content": [
                {"type": "text", "text": "question"},
                reminder()]}),
        json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tool-1", "name": "search", "input": {}}]}),
    ];
    let (out, placed) = place_tail_cache_breakpoints(msgs, 2, false);

    assert_eq!(placed, 2, "the opener and the question are both cacheable");
    assert!(out[1]["content"][0].get("cache_control").is_some());
    assert!(out[1]["content"][1].get("cache_control").is_none());
    assert!(out[2]["content"][0].get("cache_control").is_none());
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
    assert_eq!(text_block_kinds(&json!({"role": "user"})), "");
}

/// String content is classified rather than skipped. Reporting `""` for it
/// hid a reminder living inside the string behind the same output a message
/// with no text at all produces.
#[test]
fn text_kinds_classify_string_content() {
    assert_eq!(text_block_kinds(&json!({"content": "plain"})), "plain");
    assert_eq!(
        text_block_kinds(
            &json!({"content": "do the thing\n\n<system-reminder>x</system-reminder>"})
        ),
        "plain+system-reminder"
    );
    assert_eq!(
        text_block_kinds(&json!({"content": "<system-reminder>x</system-reminder>"})),
        "system-reminder"
    );
}

#[test]
fn text_kinds_tolerate_leading_whitespace() {
    let m = json!({"content": [
            {"type": "text", "text": "\n  <system-reminder>x</system-reminder>"}]});
    assert_eq!(text_block_kinds(&m), "system-reminder");
}

/// Withdrawing a reminder must not disturb a single byte of stored history.
///
/// Claude Code decorates its newest user message with a reminder and takes
/// it off the turn after, so the change always lands in the prefix TAIL,
/// next to the breakpoints. Honouring it means rebuilding from the last
/// breakpoint that still matches, which is far back: measured live on
/// 2026-08-16, one such turn wrote 151k and read 18k where its neighbours
/// read 165k and wrote under 1k.
///
/// So replay forwards the stored bytes and the withdrawn reminder rides
/// along. This pins the price of that: one stale span per decorated
/// message, never more, and all of it inside the cached prefix at 0.1x.
#[test]
fn withdrawn_reminders_leave_forwarded_history_untouched() {
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
            let blocks = last
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .unwrap();
            blocks.retain(|b| !is_ephemeral_client_block(b));
        }
        if turn > 0 {
            client.push(text_msg("assistant", &format!("reply {turn}")));
        }
        client.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("ask {turn}")},
            {"type": "text", "text": format!("<system-reminder>r{turn}</system-reminder>")},
        ]}));

        // Compression is the identity here, so this measures the replay
        // path and nothing else.
        let originals = client.clone();
        let (prev_orig, prev_fwd) = match &prev {
            Some((o, f)) => (Some(o.as_slice()), Some(f.as_slice())),
            None => (None, None),
        };
        let forwarded = overlay_cached_prefix(originals.clone(), &originals, prev_orig, prev_fwd);

        // The point of the exercise: everything the provider cached last
        // turn goes back out identical, so the read survives the withdrawal.
        if let Some(stored) = prev_fwd {
            assert_eq!(
                &forwarded[..stored.len()],
                stored,
                "turn {turn}: replayed history diverged from the cached bytes"
            );
        }
        // One user message, one span it was decorated with. Growth that
        // outruns this is accumulation and would mean the guard is wrong.
        assert_eq!(
            reminder_count(&forwarded),
            turn + 1,
            "turn {turn}: expected one stale span per decorated message, \
                 found {} in {} messages",
            reminder_count(&forwarded),
            forwarded.len()
        );
        prev = Some((originals, forwarded));
    }
}
/// The production shape: Claude Code resends the whole conversation with a
/// synthetic final message. Parking it poisoned the session's previous
/// turn and cost a full recache on the next real turn.
#[test]
fn a_suggestion_turn_is_a_side_errand() {
    let msgs = vec![
        json!({"role": "user", "content": "real conversation opener"}),
        json!({"role": "assistant", "content": "an answer"}),
        json!({"role": "user", "content": [{
            "type": "text",
            "text": "[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]\n\nFIRST: Look at the user's recent messages"
        }]}),
    ];
    assert!(is_side_errand(&msgs));
}

#[test]
fn the_string_sugar_form_is_caught_too() {
    let msgs = vec![json!({
        "role": "user",
        "content": "[SUGGESTION MODE: Suggest what the user might naturally type next]"
    })];
    assert!(is_side_errand(&msgs));
}

#[test]
fn an_ordinary_turn_is_not_a_side_errand() {
    let msgs = vec![
        json!({"role": "user", "content": "real conversation opener"}),
        json!({"role": "assistant", "content": "an answer"}),
        json!({"role": "user", "content": [{"type": "text", "text": "and now do the next thing"}]}),
    ];
    assert!(!is_side_errand(&msgs));
}

/// The marker only counts at the head of the newest message. A turn that
/// quotes it — this conversation, for one — is still a real turn.
#[test]
fn a_turn_quoting_the_marker_is_still_a_real_turn() {
    let msgs = vec![json!({"role": "user", "content": [{
        "type": "text",
        "text": "why does [SUGGESTION MODE: ...] show up in the replay logs?"
    }]})];
    assert!(!is_side_errand(&msgs));
}

#[test]
fn an_assistant_tail_is_never_a_side_errand() {
    let msgs = vec![json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "[SUGGESTION MODE: ...]"}]
    })];
    assert!(!is_side_errand(&msgs));
}

#[test]
fn an_empty_conversation_is_not_a_side_errand() {
    assert!(!is_side_errand(&[]));
}
