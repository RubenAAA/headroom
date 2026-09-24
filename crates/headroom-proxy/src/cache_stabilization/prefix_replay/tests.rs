use super::*;
use crate::cache_stabilization::ephemeral_spans::is_pure_client_scaffolding;
use serde_json::json;

fn text_msg(role: &str, text: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": text}]})
}

// Reproduces the two prefix declines logged on 2026-08-13 in conversation
// 2e82d794bb9707c9. Both name `content[0].text` at an index equal to
// `stored_prefix_msgs - 1` — the newest message of the stored prefix.
#[test]
fn reminder_embedded_in_string_then_split_into_blocks_compares_equal() {
    // 20:12:03 — diff_shape 'string' -> 'text,text',
    //            diff_text_kinds '' -> 'plain,system-reminder'.
    let stored = vec![
        json!({"role":"user","content":"do the thing\n\n<system-reminder>x</system-reminder>"}),
    ];
    let current = vec![
        json!({"role":"user","content":[
            {"type":"text","text":"do the thing"},
            {"type":"text","text":"<system-reminder>x</system-reminder>"}
        ]}),
        text_msg("assistant", "ok"),
    ];
    assert!(matches_canonical_prefix(
        &stored,
        &canonicalize_slice(&current)
    ));
}

#[test]
fn reminder_embedded_in_string_then_withdrawn_compares_equal() {
    // 20:04:29 — diff_shape 'string' -> 'string', diff_text_kinds '' -> ''.
    // The kinds field reports empty for any non-array content, so it cannot
    // show a reminder living inside the string.
    let stored = vec![
        json!({"role":"user","content":"do the thing\n\n<system-reminder>x</system-reminder>"}),
    ];
    let current = vec![
        json!({"role":"user","content":"do the thing"}),
        text_msg("assistant", "ok"),
    ];
    assert!(matches_canonical_prefix(
        &stored,
        &canonicalize_slice(&current)
    ));
}

// Control: the same string→blocks representation change, with the reminder
// arriving as its own block rather than embedded. The filter sees it here,
// so this must pass — isolating the cause to the embedded case above.
#[test]
fn reminder_as_its_own_block_compares_equal() {
    let stored = vec![json!({"role":"user","content":"do the thing"})];
    let current = vec![
        json!({"role":"user","content":[
            {"type":"text","text":"do the thing"},
            {"type":"text","text":"<system-reminder>x</system-reminder>"}
        ]}),
        text_msg("assistant", "ok"),
    ];
    assert!(matches_canonical_prefix(
        &stored,
        &canonicalize_slice(&current)
    ));
}

/// One message, every representation the client sends it in — they must all
/// give the same key.
///
/// The three declines logged on 2026-08-13 all name `content[0].text` at a
/// `first_diff_index` one short of `stored_prefix_msgs`, so the difference
/// is in the surviving PLAIN text, not in the reminder block or a block
/// count. [`split_ephemeral_spans`] trims what it leaves behind, so the
/// embedded form keys as `"Do X"`; the block form keeps the separator on
/// the neighbouring block, which never carried a span and so was never
/// trimmed, and keys as `"Do X\n"`. One of those declines was followed 35
/// seconds later by an 88,606-token `aftershock_of_diverged_prefix`.
#[test]
fn every_representation_of_one_message_canonicalizes_alike() {
    let reference = canonicalize_for_prefix_compare(&json!({"role":"user","content":"Do X"}));
    let variants = vec![
        // Embedded in string sugar, with and without a separator.
        json!({"role":"user","content":"Do X\n<system-reminder>foo</system-reminder>"}),
        json!({"role":"user","content":"Do X\n\n<system-reminder>foo</system-reminder>"}),
        json!({"role":"user","content":"Do X<system-reminder>foo</system-reminder>"}),
        // A newline after the closing tag, and spaces before the open tag.
        json!({"role":"user","content":"Do X\n<system-reminder>foo</system-reminder>\n"}),
        json!({"role":"user","content":"Do X\n   <system-reminder>foo</system-reminder>"}),
        // A span in the middle, and several spans.
        json!({"role":"user","content":"<system-reminder>a</system-reminder>\nDo X"}),
        json!({"role":"user","content":
                "<system-reminder>a</system-reminder>\nDo X\n<system-reminder>b</system-reminder>"}),
        // The same message as blocks. The separator stays on the plain
        // block here — nothing lifts a span out of it, so nothing trims it.
        json!({"role":"user","content":[
                {"type":"text","text":"Do X"},
                {"type":"text","text":"<system-reminder>foo</system-reminder>"}]}),
        json!({"role":"user","content":[
                {"type":"text","text":"Do X\n"},
                {"type":"text","text":"<system-reminder>foo</system-reminder>"}]}),
        json!({"role":"user","content":[
                {"type":"text","text":"Do X\n\n"},
                {"type":"text","text":"<system-reminder>foo</system-reminder>"}]}),
        // The client withdraws the reminder. This is the 21:07:09 decline:
        // stored `text,text` / `plain,system-reminder` against current
        // `string` / `plain`.
        json!({"role":"user","content":"Do X\n"}),
        json!({"role":"user","content":[{"type":"text","text":"Do X\n"}]}),
    ];
    for variant in variants {
        assert_eq!(
            canonicalize_for_prefix_compare(&variant),
            reference,
            "representation must not change the key: {variant}"
        );
    }
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

    let out = overlay_cached_prefix(optimized, &current_orig, Some(&prev_orig), Some(&prev_fwd));
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

    let out = overlay_cached_prefix(optimized, &current_orig, Some(&prev_orig), Some(&prev_fwd));
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

fn scaffolding_msg(role: &str) -> Value {
    json!({
        "role": role,
        "content": "<system-reminder>\nskills available\n</system-reminder>",
    })
}

/// The client withdrawing a standalone reminder message must not cost the
/// prefix. Every message behind it shifts by one, which an index-aligned
/// compare reads as a divergence at that point — 7 of 16 content
/// divergences on 2026-08-26, one of them 37,448 tokens.
#[test]
fn overlay_replays_across_a_withdrawn_scaffolding_message() {
    let u0 = text_msg("user", "first");
    let scaffold = scaffolding_msg("system");
    let a1 = text_msg("assistant", "reply");
    let u2 = text_msg("user", "second");
    let prev_orig = vec![u0.clone(), scaffold.clone(), a1.clone(), u2.clone()];
    let prev_fwd = vec![
        text_msg("user", "first-c"),
        scaffold.clone(),
        text_msg("assistant", "reply-c"),
        text_msg("user", "second-c"),
    ];

    // The client has dropped the reminder and added a turn.
    let a3 = text_msg("assistant", "newest");
    let current_orig = vec![u0, a1, u2, a3.clone()];
    let optimized = current_orig.clone();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(skip.is_none(), "withdrawal must not decline: {skip:?}");
    assert_eq!(out[..4], prev_fwd[..], "cached bytes replayed verbatim");
    assert_eq!(out[4], a3, "this turn's own tail follows");
    assert_eq!(out.len(), 5);
    assert!(
        out.contains(&scaffold),
        "the withdrawn reminder still goes out: it is in the cached prefix"
    );
}

/// The tail `system` message of the stored turn carried two reminders as
/// two blocks; this turn the client re-renders it with only the first, as
/// one string. Seen on 2026-09-07: skipping only the stored copy left the
/// shrunken one to be spliced in right behind it, `system` after `system`,
/// and the adjacency net declined 82 turns running, 1.47M cached tokens
/// rebuilt. The shrunken copy is a replacement, so it is consumed with the
/// stored one and the cached two-block form goes out.
#[test]
fn overlay_replays_across_a_shrunken_scaffolding_message() {
    let u0 = text_msg("user", "first");
    let a1 = json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]});
    let u2 = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]});
    let two_blocks = json!({
        "role": "system",
        "content": [
            {"type": "text", "text": "Only you see that command's output."},
            {"type": "text", "text": "First privately list what you need next."},
        ],
    });
    let one_string = json!({
        "role": "system",
        "content": "Only you see that command's output.",
    });
    let prev_orig = vec![u0.clone(), a1.clone(), u2.clone(), two_blocks.clone()];
    let prev_fwd = vec![
        text_msg("user", "first-c"),
        a1.clone(),
        u2.clone(),
        two_blocks.clone(),
    ];

    let a4 = text_msg("assistant", "newest");
    let u5 = text_msg("user", "again");
    let current_orig = vec![u0, a1, u2, one_string.clone(), a4.clone(), u5.clone()];
    let optimized = current_orig.clone();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(
        skip.is_none(),
        "a shrunken reminder must not decline: {skip:?}"
    );
    assert_eq!(out[..4], prev_fwd[..], "cached bytes replayed verbatim");
    assert_eq!(out[4], a4, "this turn's own tail follows the cached prefix");
    assert_eq!(out[5], u5);
    assert_eq!(
        out.len(),
        6,
        "the shrunken copy is consumed, not spliced in twice"
    );
    assert!(
        !out.contains(&one_string),
        "the shrunken form is not forwarded: the cached two-block form covers it"
    );
    assert_eq!(
        first_illegal_system_position(&out),
        None,
        "no system message may land behind another system message"
    );
}

/// The alignment itself, without the overlay: a scaffolding slot whose
/// current occupant is also scaffolding consumes one current message; a
/// slot the client emptied consumes none. Both step over the stored copy.
#[test]
fn align_consumes_a_replaced_scaffolding_message_but_not_a_withdrawn_one() {
    let u0 = text_msg("user", "first");
    let a1 = text_msg("assistant", "reply");
    let two_blocks = json!({
        "role": "system",
        "content": [
            {"type": "text", "text": "reminder A"},
            {"type": "text", "text": "reminder B"},
        ],
    });
    let one_string = json!({"role": "system", "content": "reminder A"});
    let prev_orig = vec![u0.clone(), a1.clone(), two_blocks];

    let replaced = vec![
        u0.clone(),
        a1.clone(),
        one_string,
        text_msg("assistant", "next"),
    ];
    assert_eq!(
        align_over_withdrawn_scaffolding(&prev_orig, &replaced),
        Some(3),
        "the shrunken reminder occupies the stored slot and is consumed"
    );

    let withdrawn = vec![u0.clone(), a1.clone(), text_msg("assistant", "next")];
    assert_eq!(
        align_over_withdrawn_scaffolding(&prev_orig, &withdrawn),
        Some(2),
        "a withdrawn reminder consumes nothing on the current side"
    );

    let edited = vec![u0, a1, text_msg("user", "a real edit")];
    assert_eq!(
        align_over_withdrawn_scaffolding(&prev_orig, &edited),
        Some(2),
        "a non-scaffolding message at the slot is not consumed by the skip"
    );
}

/// The same withdrawal, on the form Claude Code actually sends most of the
/// time: a `role: "system"` message with no `<system-reminder>` wrapper at
/// all. Four in five of the stored ones look like this — output-style
/// banners, hook context, skill listings — and keying on the tag left every
/// one of them stranding the whole prefix. See
/// [`is_client_scaffolding_message`] for the counts.
#[test]
fn overlay_replays_across_a_withdrawn_untagged_system_message() {
    let u0 = text_msg("user", "first");
    let banner = json!({
        "role": "system",
        "content": "PreToolUse:Bash hook additional context: check the path first",
    });
    let a1 = text_msg("assistant", "reply");
    let u2 = text_msg("user", "second");
    let prev_orig = vec![u0.clone(), banner.clone(), a1.clone(), u2.clone()];
    let prev_fwd = vec![
        text_msg("user", "first-c"),
        banner.clone(),
        text_msg("assistant", "reply-c"),
        text_msg("user", "second-c"),
    ];

    let a3 = text_msg("assistant", "newest");
    let current_orig = vec![u0, a1, u2, a3.clone()];
    let optimized = current_orig.clone();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(
        skip.is_none(),
        "an untagged banner is scaffolding too: {skip:?}"
    );
    assert_eq!(out[..4], prev_fwd[..], "cached bytes replayed verbatim");
    assert_eq!(out[4], a3);
}

/// An OpenAI-Chat body opens with its real system prompt. Losing that is a
/// changed prompt, not withdrawn scaffolding, and the prefix has to go.
#[test]
fn overlay_declines_when_the_opening_system_prompt_is_withdrawn() {
    let sys = json!({"role": "system", "content": "You are a helpful assistant."});
    let u1 = text_msg("user", "first");
    let a2 = text_msg("assistant", "reply");
    let prev_orig = vec![sys, u1.clone(), a2.clone()];
    let prev_fwd = prev_orig.clone();

    let current_orig = vec![u1, a2, text_msg("user", "second")];
    let optimized = current_orig.clone();

    let (_, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(
        skip.is_some(),
        "a withdrawn opening system prompt must not be stepped over"
    );
}

/// Stepping over stored scaffolding must not turn into stepping over a real
/// edit hiding behind it.
#[test]
fn overlay_still_declines_a_real_edit_behind_withdrawn_scaffolding() {
    let u0 = text_msg("user", "first");
    let prev_orig = vec![
        u0.clone(),
        scaffolding_msg("system"),
        text_msg("assistant", "reply"),
    ];
    let prev_fwd = vec![
        text_msg("user", "first-c"),
        scaffolding_msg("system"),
        text_msg("assistant", "reply-c"),
    ];
    let current_orig = vec![
        u0,
        text_msg("assistant", "EDITED"),
        text_msg("user", "next"),
    ];
    let optimized = current_orig.clone();

    let (_, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(
        matches!(skip, Some(ReplaySkip::PrefixContentDiverged { .. })),
        "an edited message is still a divergence: {skip:?}"
    );
}

/// Scaffolding the client has just ATTACHED inside the prefix is not
/// stepped over. Doing so would leave it off the wire, which is how
/// reminders get lost; declining only costs cache.
#[test]
fn overlay_declines_rather_than_drop_newly_attached_scaffolding() {
    let u0 = text_msg("user", "first");
    let a1 = text_msg("assistant", "reply");
    let prev_orig = vec![u0.clone(), a1.clone()];
    let prev_fwd = vec![
        text_msg("user", "first-c"),
        text_msg("assistant", "reply-c"),
    ];
    let current_orig = vec![u0, scaffolding_msg("system"), a1, text_msg("user", "next")];
    let optimized = current_orig.clone();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(
        skip.is_some(),
        "must not silently drop an attached reminder"
    );
    assert!(
        out.contains(&scaffolding_msg("system")),
        "the attached reminder stays on the wire"
    );
}

/// A pass that deletes from mid-history would shift the tail under the
/// splice. Nothing does today; the point is that it cannot start doing it
/// quietly.
#[test]
fn overlay_declines_when_the_pipeline_dropped_a_message() {
    let u0 = text_msg("user", "first");
    let a1 = text_msg("assistant", "reply");
    let prev_orig = vec![u0.clone()];
    let prev_fwd = vec![text_msg("user", "first-c")];
    let current_orig = vec![u0.clone(), a1, text_msg("user", "next")];
    // The pipeline handed back one message fewer than the client sent.
    let optimized = vec![u0, text_msg("user", "next")];

    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert_eq!(
        skip,
        Some(ReplaySkip::OptimizedShorterThanOriginals),
        "a shifted tail must decline, not splice"
    );
    assert_eq!(out, optimized, "and hand back what it was given");
}

#[test]
fn scaffolding_is_the_message_with_no_prose_of_its_own() {
    assert!(is_pure_client_scaffolding(&scaffolding_msg("system")));
    assert!(is_pure_client_scaffolding(&scaffolding_msg("user")));
    // The model quotes these tags when it discusses them. Its own words are
    // not scaffolding, whatever they contain.
    assert!(!is_pure_client_scaffolding(&scaffolding_msg("assistant")));
    // Prose of its own means the client would be sending it regardless.
    assert!(!is_pure_client_scaffolding(&json!({
        "role": "user",
        "content": "do the thing\n<system-reminder>note</system-reminder>",
    })));
    assert!(!is_pure_client_scaffolding(&text_msg("user", "plain")));
    // One block, reminder first and the user's real prompt after it. The
    // block-level "starts with a reminder" test passes this; treating it as
    // scaffolding would step over a genuine turn.
    assert!(!is_pure_client_scaffolding(&json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": "<system-reminder>note</system-reminder>\nplease do the thing",
        }],
    })));
    // Reminder blocks alongside a real one are not scaffolding either.
    assert!(!is_pure_client_scaffolding(&json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "<system-reminder>note</system-reminder>"},
            {"type": "text", "text": "please do the thing"},
        ],
    })));
    // Block form of the standalone carrier still counts.
    assert!(is_pure_client_scaffolding(&json!({
        "role": "system",
        "content": [{"type": "text", "text": "<system-reminder>note</system-reminder>"}],
    })));
}

/// The predicate is what stands between a withdrawal and a real turn, so a
/// message it misreads is a message stepped over. This is the shape that
/// nearly got through.
#[test]
fn overlay_declines_when_a_real_turn_opens_with_a_reminder() {
    let u0 = text_msg("user", "first");
    let carrier = json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": "<system-reminder>note</system-reminder>\nthe real prompt",
        }],
    });
    let prev_orig = vec![u0.clone(), carrier, text_msg("assistant", "reply")];
    let prev_fwd = vec![
        text_msg("user", "first-c"),
        text_msg("user", "carrier-c"),
        text_msg("assistant", "reply-c"),
    ];
    // The client has edited that turn away, not withdrawn scaffolding.
    let current_orig = vec![u0, text_msg("assistant", "reply"), text_msg("user", "next")];
    let optimized = current_orig.clone();

    let (_, skip) = overlay_cached_prefix_reported(
        optimized,
        &current_orig,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert!(
        matches!(skip, Some(ReplaySkip::PrefixContentDiverged { .. })),
        "a turn carrying real prose must not be stepped over: {skip:?}"
    );
}

/// A divergence costs only what follows it: the leading run that still
/// agrees is replayed from the stored prefix.
///
/// The premise for declining the whole prefix used to be that compression
/// is deterministic, so this turn's own bytes reproduce what the provider
/// cached anyway. Capture refutes it — early messages carry reminder spans
/// the client attaches and withdraws, so a freshly computed message 0
/// disagrees with the frozen one and the miss lands at the very first
/// message. See `overlay_cached_prefix_reported`.
#[test]
fn overlay_replays_agreeing_run_when_a_later_message_diverges() {
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
        true,
        None,
    );
    assert_eq!(
        skip,
        Some(ReplaySkip::PrefixContentDiverged {
            first_diff_index: 2,
            replayed_prefix_msgs: 2,
        }),
        "the index is reported, and so is how much was salvaged"
    );
    let mut expected = prev_fwd[..2].to_vec();
    expected.extend_from_slice(&optimized[2..]);
    assert_eq!(
        out, expected,
        "the agreeing run comes from the stored prefix, the rest from this turn"
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

    let once = overlay_cached_prefix(optimized, &current_orig, Some(&prev_orig), Some(&prev_fwd));
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
    let (out, placed) = place_tail_cache_breakpoints(msgs.clone(), 2, false);

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
    let (out, placed) = place_tail_cache_breakpoints(msgs, 2, false);

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
    let (out, placed) = place_tail_cache_breakpoints(msgs, 2, false);

    assert_eq!(placed, 1);
    assert_eq!(marked_messages(&out), vec![0]);
    assert_eq!(
        out[1], expansion,
        "the expansion must remain bare and unmarked"
    );
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
    let (out, placed) = place_tail_cache_breakpoints(msgs, 2, false);
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
    let (out, placed) = place_tail_cache_breakpoints(msgs, 2, false);
    assert_eq!(placed, 2);
    assert_eq!(marked_messages(&out), vec![0, 1]);
}

#[test]
fn slots_beyond_the_cacheable_messages_place_what_there_is() {
    let msgs = vec![
        json!({"role": "user", "content": "plain"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
    ];
    let (out, placed) = place_tail_cache_breakpoints(msgs, 3, false);
    assert_eq!(placed, 2);
    assert_eq!(marked_messages(&out), vec![0, 1]);
}

/// A first user message with no scaffolding at its head, so the budget
/// offers no scaffolding slot and every division is the pre-existing one.
fn plain_opener() -> Vec<Value> {
    vec![json!({"role":"user","content":[{"type":"text","text":"hello"}]})]
}

/// Claude Code's opener: the CLAUDE.md reminder, a second reminder, then
/// what the user typed. Only the first two are shared across sessions.
fn scaffolded_opener() -> Value {
    json!({"role":"user","content":[
        {"type":"text","text":"<system-reminder>claudeMd</system-reminder>"},
        {"type":"text","text":"<system-reminder>gitStatus</system-reminder>"},
        {"type":"text","text":"build a parser"}
    ]})
}

fn marked_positions(messages: &[Value]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if let Some(blocks) = m.get("content").and_then(Value::as_array) {
            for (j, b) in blocks.iter().enumerate() {
                if b.get("cache_control").is_some() {
                    out.push((i, j));
                }
            }
        }
    }
    out
}

/// The shared prefix gets its own breakpoint, on top of the tail pair rather
/// than out of it. The tail pair is what a tail-edited turn reads back from.
#[test]
fn the_opening_scaffolding_adds_a_breakpoint_of_its_own() {
    let messages = vec![
        scaffolded_opener(),
        json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
        json!({"role":"user","content":[{"type":"text","text":"go on"}]}),
    ];
    let (out, placed) = place_tail_cache_breakpoints(messages, 2, true);
    assert_eq!(placed, 3);
    assert_eq!(
        marked_positions(&out),
        vec![(0, 1), (1, 0), (2, 0)],
        "the last reminder, and both tail markers still in place"
    );
}

/// Claude Code's shape: two `system` markers, none on `tools`. The second
/// `system` marker is what pays for the scaffolding breakpoint.
#[test]
fn the_second_system_marker_pays_for_the_scaffolding_breakpoint() {
    let body = json!({
        "system": [
            {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ],
        "tools": [{"name": "t"}],
    });
    let messages = vec![
        scaffolded_opener(),
        json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
        json!({"role":"user","content":[{"type":"text","text":"go on"}]}),
    ];
    let slots = message_slots_within_budget(&body, &messages, 2);
    assert_eq!(slots.tail, 2, "the tail pair must survive intact");
    assert!(slots.scaffold);
    assert_eq!(slots.reserved, 1, "one system marker, none on tools");

    // And the trim really takes it, leaving four markers exactly.
    let mut body = body;
    let (_out, placed) = place_tail_cache_breakpoints(messages, slots.tail, slots.scaffold);
    assert_eq!(placed, 3);
    assert_eq!(trim_system_breakpoints_to_budget(&mut body, placed), 1);
    assert_eq!(count_field_markers(&body, "system"), 1);
    assert_eq!(placed + count_field_markers(&body, "system"), 4);
}

/// A `tools` marker leaves no slot to buy. The scaffolding breakpoint is
/// what gives way then, never a tail one.
#[test]
fn the_scaffolding_yields_before_the_tail_does() {
    let body = json!({
        "system": [
            {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral"}}
        ],
        "tools": [{"name": "t", "cache_control": {"type": "ephemeral"}}],
    });
    let messages = vec![
        scaffolded_opener(),
        json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
        json!({"role":"user","content":[{"type":"text","text":"go on"}]}),
    ];
    let slots = message_slots_within_budget(&body, &messages, 2);
    assert!(!slots.scaffold);
    assert_eq!(
        (slots.tail, slots.reserved),
        (1, 3),
        "and `system` keeps both markers, since nothing was bought with one"
    );
}

/// With a single tail slot the budget never offers a scaffolding one: a
/// breakpoint on message 0 alone would trade the whole conversation for its
/// opener.
#[test]
fn one_slot_still_goes_to_the_tail() {
    let messages = vec![
        scaffolded_opener(),
        json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
        json!({"role":"user","content":[{"type":"text","text":"go on"}]}),
    ];
    let body = json!({"system": [
        {"type": "text", "text": "s1", "cache_control": {"type": "ephemeral"}},
        {"type": "text", "text": "s2", "cache_control": {"type": "ephemeral"}}
    ]});
    let slots = message_slots_within_budget(&body, &messages, 1);
    assert!(!slots.scaffold);
    assert_eq!(slots.tail, 1);

    let (out, placed) = place_tail_cache_breakpoints(messages, slots.tail, slots.scaffold);
    assert_eq!(placed, 1);
    assert_eq!(marked_positions(&out), vec![(2, 0)]);
}

/// Three turns, the third of which rewrites the text of the newest message
/// the second one sent. That is the case the tail hedge exists for, and the
/// scaffolding marker must come through it untouched.
///
/// The scaffolding block is the same bytes on every turn, so its entry stays
/// live no matter what the tail does. Exactly one tail marker lands on the
/// newest message — a message can only hold one — and the older tail marker
/// still names the turn before it.
#[test]
fn an_edited_tail_leaves_the_scaffolding_breakpoint_alone() {
    let turn = |tail: &str, depth: usize| {
        let mut messages = vec![scaffolded_opener()];
        for i in 0..depth {
            messages.push(json!({"role":"assistant","content":[
                {"type":"text","text": format!("reply {i}")}
            ]}));
            let text = if i + 1 == depth {
                tail.to_string()
            } else {
                format!("follow up {i}")
            };
            messages.push(json!({"role":"user","content":[
                {"type":"text","text": text}
            ]}));
        }
        messages
    };

    let tail_slots = 2;
    let turns = [
        turn("keep going", 1),
        turn("and then this", 2),
        // Turn 3 keeps turn 2's depth and rewrites its newest text block.
        turn("actually, do it the other way", 2),
    ];

    let mut scaffold_seen: Option<Value> = None;
    for (n, messages) in turns.into_iter().enumerate() {
        let newest = messages.len() - 1;
        let (out, placed) = place_tail_cache_breakpoints(messages, tail_slots, true);
        let marks = marked_positions(&out);

        // The scaffolding breakpoint survives, on the same block, carrying
        // the same bytes as every other turn.
        assert!(
            marks.contains(&(0, 1)),
            "turn {}: scaffolding breakpoint gone: {marks:?}",
            n + 1
        );
        let scaffold = out[0]["content"][1]["cache_control"].clone();
        match &scaffold_seen {
            None => scaffold_seen = Some(scaffold),
            Some(first) => assert_eq!(&scaffold, first, "turn {}", n + 1),
        }

        // Exactly one tail marker on the newest message.
        let on_newest = marks.iter().filter(|(m, _)| *m == newest).count();
        assert_eq!(
            on_newest,
            1,
            "turn {}: {on_newest} markers on the newest message: {marks:?}",
            n + 1
        );

        // And the tail never spends more than it was given. The scaffolding
        // marker is the one extra, paid for out of `system` by the caller.
        let tail_markers = marks.len() - 1;
        assert!(
            tail_markers <= tail_slots,
            "turn {}: {tail_markers} tail markers for {tail_slots} slots",
            n + 1
        );
        assert_eq!(placed, marks.len());
    }
}

/// A session whose recall was injected in front of the reminders — every
/// conversation stored before the injector moved — has no run at the head,
/// so its message 0 is left exactly as the provider already cached it.
#[test]
fn a_recall_in_front_of_the_scaffolding_gets_no_breakpoint() {
    let messages = vec![
        json!({"role":"user","content":[
            {"type":"text","text":"<!--ctx:injected--><session_recall/>"},
            {"type":"text","text":"<system-reminder>claudeMd</system-reminder>"},
            {"type":"text","text":"build a parser"}
        ]}),
        json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
        json!({"role":"user","content":[{"type":"text","text":"go on"}]}),
    ];
    assert!(!opens_with_scaffolding(&messages));
    let (out, placed) = place_tail_cache_breakpoints(messages, 2, true);
    assert_eq!(placed, 2);
    assert_eq!(
        marked_positions(&out),
        vec![(1, 0), (2, 0)],
        "the tail keeps both, exactly as before this change"
    );
}

/// Turn one: message 0 is both the shared opener and the newest message.
/// Two distinct blocks, so two markers, and neither is counted twice.
#[test]
fn turn_one_marks_the_scaffolding_and_the_tail_of_the_same_message() {
    let (out, placed) = place_tail_cache_breakpoints(vec![scaffolded_opener()], 2, true);
    assert_eq!(placed, 2);
    assert_eq!(marked_positions(&out), vec![(0, 1), (0, 2)]);
}

/// An opener that is nothing but scaffolding collapses to one marker. The
/// count has to say one: upstream reads it as licence to drop the client's
/// `system` breakpoints.
#[test]
fn an_all_scaffolding_opener_is_counted_once() {
    let messages = vec![json!({"role":"user","content":[
        {"type":"text","text":"<system-reminder>claudeMd</system-reminder>"}
    ]})];
    let (out, placed) = place_tail_cache_breakpoints(messages, 2, true);
    assert_eq!(placed, 1);
    assert_eq!(marked_positions(&out), vec![(0, 0)]);
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
    let slots = message_slots_within_budget(&body, &plain_opener(), 2);
    assert_eq!((slots.tail, slots.reserved), (2, 2));
    assert!(!slots.scaffold);
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
    let slots = message_slots_within_budget(&body, &plain_opener(), 2);
    assert_eq!((slots.tail, slots.reserved), (1, 3));
}

#[test]
fn a_full_budget_places_no_message_markers_at_all() {
    let body = json!({
        "system": (0..4)
            .map(|i| json!({"type": "text", "text": i.to_string(),
                            "cache_control": {"type": "ephemeral"}}))
            .collect::<Vec<_>>(),
    });
    let slots = message_slots_within_budget(&body, &plain_opener(), 2);
    assert_eq!((slots.tail, slots.reserved), (0, 4));

    // And zero slots really means none placed, not one.
    let msgs = vec![json!({"role": "user", "content": [{"type": "text", "text": "a"}]})];
    let (out, placed) = place_tail_cache_breakpoints(msgs, 0, false);
    assert_eq!(placed, 0);
    assert!(marked_messages(&out).is_empty());
}

#[test]
fn a_string_system_reserves_nothing() {
    let body = json!({"system": "one prompt", "messages": []});
    let slots = message_slots_within_budget(&body, &plain_opener(), 2);
    assert_eq!((slots.tail, slots.reserved), (2, 0));
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
    t.update_from_response(first_tokens, 0, &fwd, None, String::new());
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
    store.begin_request("reminder-tail", "S", originals, forwarded, String::new());
    store.complete("reminder-tail", 5_000, 0);

    // On the next turn the client replaces the reminder-only tail with a
    // real bare-string message. Only the stable prefix is compared, so the
    // replay succeeds instead of reporting text -> string divergence.
    let replacement = json!({"role": "assistant", "content": "real next message"});
    let current = vec![stable_original, replacement];
    let (stored_originals, stored_forwarded, _) = store
        .previous_turn_for("S", &current, None)
        .expect("the stable prefix remains replayable");
    assert_eq!(stored_originals.as_slice(), &current[..1]);
    assert_eq!(stored_forwarded, vec![stable_forwarded.clone()]);

    let (out, skip) = overlay_cached_prefix_reported(
        current.clone(),
        &current,
        Some(&stored_originals),
        Some(&stored_forwarded),
        true,
        None,
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

    tracker.update_from_response(5_000, 0, &messages, Some(&messages), String::new());

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
        String::new(),
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
        String::new(),
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
    t.update_from_response(5000, 0, &fwd, None, String::new());
    t.invalidate();
    assert!(t.last_forwarded_messages().is_empty());
    assert_eq!(t.frozen_message_count(), 0);
}

// ---- cross-session prefix adoption ----

/// `n` alternating messages with distinct text, as the client sent them.
fn conversation(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            text_msg(role, &format!("message {i} {}", "x".repeat(600)))
        })
        .collect()
}

/// What the proxy forwarded for [`conversation`]: different bytes, so a
/// replay is told apart from a rebuild.
fn forwarded_for(originals: &[Value]) -> Vec<Value> {
    originals
        .iter()
        .map(|m| {
            let mut m = m.clone();
            m["content"][0]["text"] = Value::String(format!(
                "{} [forwarded]",
                m["content"][0]["text"].as_str().unwrap()
            ));
            m
        })
        .collect()
}

fn record_turn(store: &SessionReplayStore, key: &str, request: &str, originals: &[Value]) {
    store.begin_request(
        request,
        key,
        originals.to_vec(),
        forwarded_for(originals),
        String::new(),
    );
    store.complete(request, 0, 5000);
}

fn adoption_log() -> (AdoptionHook, Arc<Mutex<Vec<(AdoptionDonor, String)>>>) {
    let seen: Arc<Mutex<Vec<(AdoptionDonor, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let hook: AdoptionHook = Arc::new(move |donor: &AdoptionDonor, key: &str| {
        sink.lock().unwrap().push((donor.clone(), key.to_string()));
    });
    (hook, seen)
}

#[test]
fn a_new_session_adopts_the_longest_leading_slice_of_another_sessions_prefix() {
    let mut store = SessionReplayStore::new(8);
    let (hook, seen) = adoption_log();
    store.set_adoption_hook(hook);
    // The donor went on past the fork point.
    record_turn(&store, "donor", "req-a", &conversation(14));

    let mut incoming = conversation(12);
    incoming.push(text_msg("user", "the adopter's own new tail"));
    let (originals, forwarded, chain_id) = store
        .previous_turn_for("adopter", &incoming, None)
        .expect("adopted");
    assert_eq!(originals, conversation(12));
    assert_eq!(forwarded, forwarded_for(&conversation(12)));
    assert_ne!(
        chain_id, 0,
        "an adopted prefix is a stream this turn continues"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[(
            AdoptionDonor::Session("donor".into()),
            "adopter".to_string()
        )]
    );

    // The donor is untouched.
    let (donor_originals, ..) = store
        .previous_turn_for("donor", &conversation(14), None)
        .unwrap();
    assert_eq!(donor_originals.len(), 14);
}

/// `cache_control` marker placement is not cache-key material; text is.
/// The adoption gate hashes systems through [`forwarded_system_digest`],
/// so a marker that moved must digest equal while a word changed must
/// not — otherwise every turn declines or every system aliases.
#[test]
fn system_digest_ignores_markers_but_sees_text() {
    let marked = json!([{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]);
    let plain = json!([{"type": "text", "text": "hello"}]);
    let edited = json!([{"type": "text", "text": "goodbye"}]);
    assert_eq!(
        forwarded_system_digest(Some(&marked)),
        forwarded_system_digest(Some(&plain)),
        "a moved marker must not rotate the digest"
    );
    assert_ne!(
        forwarded_system_digest(Some(&marked)),
        forwarded_system_digest(Some(&edited)),
        "changed text must rotate the digest"
    );
    assert_eq!(forwarded_system_digest(None), forwarded_system_digest(None));
    assert_ne!(
        forwarded_system_digest(None),
        forwarded_system_digest(Some(&plain)),
        "absent and present systems are different lineages"
    );
}

/// The adoption gate: message agreement alone is not a replay source. A
/// new lane continuing another lane's history under a different system
/// declines instead of splicing donor bytes under a system no provider
/// cache holds — the same provider outcome as a clean miss, honestly
/// measured, with no seeding left behind.
#[test]
fn adoption_declines_when_systems_differ() {
    let store = SessionReplayStore::new(8);
    let digest_a = forwarded_system_digest(Some(
        &json!([{"type": "text", "text": "main instructions"}]),
    ));
    let digest_b = forwarded_system_digest(Some(
        &json!([{"type": "text", "text": "subagent preamble"}]),
    ));
    assert_ne!(digest_a, digest_b);

    // Donor lane served 12 turns under system A.
    let history = conversation(12);
    store.begin_request(
        "req-a",
        "donor-lane",
        history.clone(),
        forwarded_for(&history),
        digest_a,
    );
    store.complete("req-a", 0, 5000);

    // Adopter continues the history under system B.
    let mut incoming = conversation(12);
    incoming.push(text_msg("user", "the adopter's own new tail"));
    let err = store
        .previous_turn_for("adopter-lane", &incoming, Some(&digest_b))
        .expect_err("foreign-system adoption must decline");
    assert_eq!(err, PrefixMiss::SystemChanged);
    // And nothing was installed: a retry declines the same way instead
    // of finding a seeded tracker that leads.
    let err = store
        .previous_turn_for("adopter-lane", &incoming, Some(&digest_b))
        .expect_err("declined adoption must not seed the tracker");
    assert_eq!(err, PrefixMiss::SystemChanged);
}

/// Same lineage, same system — the `cd` whose pin travelled with it:
/// adoption proceeds and the turn replays the donor's bytes.
#[test]
fn adoption_allows_when_systems_match() {
    let store = SessionReplayStore::new(8);
    let digest = forwarded_system_digest(Some(
        &json!([{"type": "text", "text": "shared instructions"}]),
    ));

    let history = conversation(12);
    store.begin_request(
        "req-a",
        "donor-lane",
        history.clone(),
        forwarded_for(&history),
        digest.clone(),
    );
    store.complete("req-a", 0, 5000);

    let mut incoming = conversation(12);
    incoming.push(text_msg("user", "the adopter's own new tail"));
    let (originals, forwarded, chain_id) = store
        .previous_turn_for("adopter-lane", &incoming, Some(&digest))
        .expect("same-system adoption must proceed");
    assert_eq!(originals, conversation(12));
    assert_eq!(forwarded, forwarded_for(&conversation(12)));
    assert_ne!(
        chain_id, 0,
        "an adopted prefix is a stream this turn continues"
    );
}

#[test]
fn an_adopted_prefix_is_carried_forward_as_the_adopters_own() {
    let store = SessionReplayStore::new(8);
    record_turn(&store, "donor", "req-a", &conversation(14));
    let mut incoming = conversation(12);
    incoming.push(text_msg("user", "the adopter's own new tail"));
    store
        .previous_turn_for("adopter", &incoming, None)
        .expect("adopted");
    record_turn(&store, "adopter", "req-b", &incoming);

    incoming.push(text_msg("assistant", "reply"));
    incoming.push(text_msg("user", "and again"));
    let (originals, forwarded, _) = store
        .previous_turn_for("adopter", &incoming, None)
        .expect("the adopter replays its own previous turn");
    assert_eq!(originals.len(), 13);
    assert_eq!(forwarded, forwarded_for(&originals));
}

/// The session key hashes message 0 with `canonicalize_for_hash`, which
/// keeps `<system-reminder>` text, so a CLAUDE.md edit mid-conversation
/// mints a new key. The next turn then arrives as a stranger carrying the
/// whole history; it must find the old key's prefix.
#[test]
fn a_conversation_whose_opening_reminder_changed_adopts_its_own_old_prefix() {
    let store = SessionReplayStore::new(8);
    let mut donor_history = conversation(14);
    donor_history[0] = json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>CLAUDE.md v1</system-reminder>"},
        {"type": "text", "text": "build a parser"}
    ]});
    record_turn(&store, "auth:token:conv-v1", "req-a", &donor_history);

    let mut incoming = donor_history.clone();
    incoming[0]["content"][0]["text"] =
        Value::String("<system-reminder>CLAUDE.md v2, edited</system-reminder>".into());
    incoming.push(text_msg("user", "next turn under the new key"));
    let (originals, forwarded, _) = store
        .previous_turn_for("auth:token:conv-v2", &incoming, None)
        .expect("the reminder is not part of the canonical prefix");
    assert_eq!(originals, donor_history);
    assert_eq!(forwarded, forwarded_for(&donor_history));
}

#[test]
fn the_most_recent_donor_wins_an_equal_length_match() {
    let store = SessionReplayStore::new(8);
    record_turn(&store, "older", "req-a", &conversation(14));
    std::thread::sleep(Duration::from_millis(5));
    record_turn(&store, "newer", "req-b", &conversation(14));
    let (hook, seen) = adoption_log();
    let mut store = store;
    store.set_adoption_hook(hook);

    let mut incoming = conversation(14);
    incoming.push(text_msg("user", "tail"));
    store
        .previous_turn_for("adopter", &incoming, None)
        .expect("adopted");
    assert_eq!(
        seen.lock().unwrap()[0].0,
        AdoptionDonor::Session("newer".into())
    );
}

#[test]
fn an_evicted_or_expired_tracker_leaves_the_head_index() {
    let mut store = SessionReplayStore::new(1);
    store.set_session_ttl_for_test(Duration::from_millis(20));
    record_turn(&store, "first", "req-a", &conversation(14));
    record_turn(&store, "second", "req-b", &conversation(14));
    let head = adoption_head_hash(&conversation(14)).unwrap();
    assert_eq!(
        store.head_index.lock().unwrap().get(&head).cloned(),
        Some(vec!["second".to_string()]),
        "the LRU evicted `first`, so the index must not name it"
    );

    std::thread::sleep(Duration::from_millis(30));
    assert!(matches!(
        store.previous_turn_for("second", &conversation(14), None),
        Err(PrefixMiss::IdlePastTtl)
    ));
    assert!(
        store.head_index.lock().unwrap().get(&head).is_none(),
        "the last key under this head expired, so the bucket goes too"
    );
}

#[test]
fn concurrent_completions_of_one_session_leave_one_whole_file_and_no_temporaries() {
    let dir = TempDir::new("persist-race");
    let store = SessionReplayStore::with_persistence(8, dir.0.clone());
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            std::thread::spawn(move || {
                let originals = conversation(12 + i);
                let request = format!("req-{i}");
                store.begin_request(
                    &request,
                    "shared",
                    originals.clone(),
                    forwarded_for(&originals),
                    String::new(),
                );
                store.complete(&request, 0, 5000);
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }

    let snapshot = read_persisted_prefix(&dir.0, "shared").expect("a whole, parseable file");
    assert!(!snapshot.forwarded.is_empty());
    assert_eq!(snapshot.forwarded, forwarded_for(&snapshot.originals));
    let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn no_adoption_below_the_message_floor() {
    let store = SessionReplayStore::new(8);
    record_turn(&store, "donor", "req-a", &conversation(14));

    let short = conversation(CROSS_SESSION_ADOPT_MIN_MESSAGES - 1);
    assert!(matches!(
        store.previous_turn_for("adopter", &short, None),
        Err(PrefixMiss::NoTrackerForSession)
    ));
    assert!(store.history_will_be_rewritten("adopter", &short));
}

#[test]
fn no_adoption_when_only_the_opening_messages_match() {
    let store = SessionReplayStore::new(8);
    record_turn(&store, "donor", "req-a", &conversation(14));

    // Same head, so the donor is examined; diverges before the floor.
    let mut incoming = conversation(14);
    incoming[4] = text_msg("user", "a different fourth message");
    assert!(matches!(
        store.previous_turn_for("adopter", &incoming, None),
        Err(PrefixMiss::NoTrackerForSession)
    ));
    assert!(store.history_will_be_rewritten("adopter", &incoming));
}

#[test]
fn a_persisted_prefix_of_another_session_is_adopted_after_a_restart() {
    let dir = TempDir::new("adopt");
    let before = SessionReplayStore::with_persistence(8, dir.0.clone());
    record_turn(&before, "donor", "req-a", &conversation(14));
    drop(before);

    let mut after = SessionReplayStore::with_persistence(8, dir.0.clone());
    let (hook, seen) = adoption_log();
    after.set_adoption_hook(hook);
    let mut incoming = conversation(12);
    incoming.push(text_msg("user", "the adopter's own new tail"));
    let (originals, forwarded, _) = after
        .previous_turn_for("adopter", &incoming, None)
        .expect("adopted from disk");
    assert_eq!(originals, conversation(12));
    assert_eq!(forwarded, forwarded_for(&conversation(12)));

    let digest = persisted_path(&dir.0, "donor")
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[(
            AdoptionDonor::PersistedDigest(digest),
            "adopter".to_string()
        )]
    );
}

#[test]
fn a_persisted_prefix_written_without_a_head_hash_is_still_found() {
    let dir = TempDir::new("adopt-old-file");
    let before = SessionReplayStore::with_persistence(8, dir.0.clone());
    record_turn(&before, "donor", "req-a", &conversation(14));
    drop(before);
    let path = persisted_path(&dir.0, "donor");
    let mut file: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(file.as_object_mut().unwrap().remove("head_hash").is_some());
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

    let after = SessionReplayStore::with_persistence(8, dir.0.clone());
    let mut incoming = conversation(12);
    incoming.push(text_msg("user", "tail"));
    let (originals, ..) = after
        .previous_turn_for("adopter", &incoming, None)
        .expect("adopted from a file hashed on first scan");
    assert_eq!(originals.len(), 12);
}

/// A directory that cleans itself up, so these tests leave nothing behind.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "headroom-replay-{name}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A conversation long enough to clear `MIN_CACHED_TOKENS`, plus one turn.
fn persisted_turn(store: &SessionReplayStore, key: &str) -> Vec<Value> {
    let big = "x".repeat(7000);
    let forwarded = vec![text_msg("user", &big)];
    store.begin_request(
        "req-1",
        key,
        forwarded.clone(),
        forwarded.clone(),
        String::new(),
    );
    store.complete("req-1", 0, 5000);
    forwarded
}

/// The offload gate asks this before touching frozen history: only a
/// history the provider cannot read back may be rewritten for free.
#[test]
fn history_is_rewritten_only_when_nothing_stored_replays_it() {
    let dir = TempDir::new("rewritten");
    let key = "auth:abc:03";
    let store = SessionReplayStore::with_persistence(8, dir.0.clone());
    let history = vec![text_msg("user", "hello"), text_msg("assistant", "hi")];

    assert!(
        store.history_will_be_rewritten(key, &history),
        "nothing stored, so the whole history is a fresh write"
    );

    let stored = persisted_turn(&store, key);
    let mut extended = stored.clone();
    extended.push(text_msg("assistant", "reply"));
    extended.push(text_msg("user", "next"));
    assert!(
        !store.history_will_be_rewritten(key, &extended),
        "the stored turn is a prefix of this one, so it replays"
    );

    assert!(
        store.history_will_be_rewritten(key, &history),
        "a history the stored turn is not a prefix of is written fresh"
    );
}

/// The companion number: when the history will be rewritten, how much of
/// it the provider may still be holding.
#[test]
fn the_agreed_prefix_is_the_leading_run_a_stored_turn_still_matches() {
    let dir = TempDir::new("agreed");
    let key = "auth:abc:04";
    let store = SessionReplayStore::with_persistence(8, dir.0.clone());

    let unrelated = vec![text_msg("user", "hello")];
    assert_eq!(
        store.agreed_prefix_len(key, &unrelated),
        None,
        "no tracker means nothing cached to lose"
    );

    let stored = persisted_turn(&store, key);
    let mut edited = stored.clone();
    edited.push(text_msg("assistant", "reply"));
    edited.push(text_msg("user", "next"));
    assert_eq!(
        store.agreed_prefix_len(key, &edited),
        Some(stored.len()),
        "the whole stored turn still agrees"
    );

    let diverged = vec![text_msg("user", "something else entirely")];
    assert_eq!(
        store.agreed_prefix_len(key, &diverged),
        Some(0),
        "nothing agrees, so a rewrite costs the provider nothing"
    );
}

#[test]
fn a_prefix_survives_the_process_that_wrote_it() {
    let dir = TempDir::new("survives");
    let key = "auth:abc:02";
    let forwarded = {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, key)
    };

    // A new store is what a restart produces: empty memory, same directory.
    let restarted = SessionReplayStore::with_persistence(8, dir.0.clone());
    let next = {
        let mut next = forwarded.clone();
        next.push(text_msg("assistant", "ok"));
        next
    };
    let (originals, replayed, chain_id) = restarted
        .previous_turn_for(key, &next, None)
        .expect("the persisted prefix should be found");
    assert_eq!(
        replayed, forwarded,
        "the forwarded bytes come back verbatim"
    );
    assert_eq!(originals, forwarded);
    assert_ne!(chain_id, 0, "and as a real chain, so the splice can run");
}

#[test]
fn the_frozen_boundary_survives_too() {
    // This is the half that costs the tokens: without it
    // `frozen_message_count` is 0, compression stops treating the history as
    // frozen, and the rewritten bytes no longer match the provider's prefix.
    let dir = TempDir::new("frozen");
    let key = "auth:abc:02";
    {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, key);
    }
    let restarted = SessionReplayStore::with_persistence(8, dir.0.clone());
    restarted.hydrate(key);
    let frozen = restarted
        .trackers
        .lock()
        .expect("lock")
        .get(key)
        .map(PrefixReplayTracker::frozen_message_count);
    assert_eq!(frozen, Some(1));
}

#[test]
fn without_a_directory_nothing_is_written() {
    let dir = TempDir::new("memory-only");
    let store = SessionReplayStore::new(8);
    persisted_turn(&store, "auth:abc:02");
    assert_eq!(
        std::fs::read_dir(&dir.0).expect("read_dir").count(),
        0,
        "persistence is opt-in"
    );
}

#[test]
fn a_stale_snapshot_is_ignored() {
    // Past PERSIST_MAX_AGE the provider has dropped the entry, so replaying
    // its bytes would write a prefix nobody holds.
    let dir = TempDir::new("stale");
    let key = "auth:abc:02";
    {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, key);
    }
    let path = persisted_path(&dir.0, key);
    let mut snapshot: PersistedPrefix =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
    snapshot.saved_at_unix = unix_now() - PERSIST_MAX_AGE.as_secs() - 1;
    std::fs::write(&path, serde_json::to_vec(&snapshot).expect("encode")).expect("write");

    assert!(read_persisted_prefix(&dir.0, key).is_none());
}

#[test]
fn one_session_never_reads_another_session_file() {
    let dir = TempDir::new("scoped");
    {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, "auth:abc:02");
    }
    assert!(read_persisted_prefix(&dir.0, "auth:abc:15").is_none());
    assert!(read_persisted_prefix(&dir.0, "auth:def:02").is_none());
}

#[test]
fn the_session_key_is_never_written_to_disk() {
    // It can carry a credential, which is why the logs only print a hash of
    // it. A file outlives the process, so the rule matters more here.
    let dir = TempDir::new("no-key");
    let key = "auth:super-secret-token:02";
    {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, key);
    }
    for entry in std::fs::read_dir(&dir.0).expect("read_dir").flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(!name.contains("super-secret-token"), "leaked in {name}");
        let body = std::fs::read_to_string(entry.path()).expect("read");
        assert!(!body.contains("super-secret-token"), "leaked in file body");
    }
}

#[test]
fn a_truncated_file_is_ignored_rather_than_trusted() {
    let dir = TempDir::new("truncated");
    let key = "auth:abc:02";
    {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, key);
    }
    let path = persisted_path(&dir.0, key);
    let bytes = std::fs::read(&path).expect("read");
    std::fs::write(&path, &bytes[..bytes.len() / 2]).expect("write");

    let restarted = SessionReplayStore::with_persistence(8, dir.0.clone());
    assert!(matches!(
        restarted.previous_turn_detailed(key),
        Err(PrefixMiss::NoTrackerForSession)
    ));
}

#[test]
fn a_persisted_prefix_that_does_not_lead_this_turn_still_declines() {
    // The guards do not weaken across a restart: a prefix from another
    // stream must fail `matches_canonical_prefix` exactly as it does in
    // memory, so a stale file can only cost a decline, never a wrong replay.
    let dir = TempDir::new("unrelated");
    let key = "auth:abc:02";
    {
        let store = SessionReplayStore::with_persistence(8, dir.0.clone());
        persisted_turn(&store, key);
    }
    let restarted = SessionReplayStore::with_persistence(8, dir.0.clone());
    let unrelated = vec![text_msg("user", "an entirely different conversation")];
    let (_, _, chain_id) = restarted
        .previous_turn_for(key, &unrelated, None)
        .expect("the prefix is returned for reporting");
    assert_eq!(chain_id, 0, "but not as a chain this turn continues");
}

#[test]
fn store_roundtrip_begin_complete_previous_turn() {
    let store = SessionReplayStore::new(8);
    let big = "x".repeat(7000);
    let orig = vec![text_msg("user", &big)];
    let fwd = vec![text_msg("user", "compressed")];
    store.begin_request("req-1", "sess-A", orig.clone(), fwd.clone(), String::new());
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
    store.begin_request("r", "S", orig, fwd, String::new());
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
            String::new(),
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
                if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
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
        let (forwarded, placed) = place_tail_cache_breakpoints(overlaid, 1, false);
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
        store.begin_request(
            &rid,
            session,
            originals.clone(),
            forwarded.clone(),
            String::new(),
        );
        // Claim a healthy cache read so the tracker keeps a live prefix.
        store.complete(&rid, 6000, 500);
    }
}
