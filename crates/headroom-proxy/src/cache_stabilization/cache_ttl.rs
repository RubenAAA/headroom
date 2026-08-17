//! B1 — force a 1-hour prompt-cache TTL.
//!
//! # What it does
//!
//! Rewrites every `cache_control` marker in an Anthropic request body to carry
//! `"ttl": "1h"`, so the provider holds the cached prefix for an hour instead of
//! the five-minute default. Markers are found wherever Anthropic accepts them:
//! on `tools[]` entries, on `system[]` blocks, and on `messages[].content[]`
//! blocks.
//!
//! Nothing else about the marker changes — `type` stays `ephemeral`, and a
//! marker that already says `1h` is left alone so the common case moves no
//! bytes.
//!
//! # What this is worth, and when
//!
//! The win is only ever the turns that would have re-created the prefix because
//! the idle gap crossed five minutes but not an hour. Nothing else: within five
//! minutes both TTLs hit, past an hour both miss.
//!
//! Two documented facts set the economics, and they point opposite ways.
//!
//! For **rate limits** ([rate-limits]), cache writes count at their raw token
//! count with no TTL distinction — `cache_creation_input_tokens` counts toward
//! ITPM whether the entry lives 5 minutes or an hour. On that axis forcing 1h
//! costs nothing and can only help.
//!
//! For **dollars** ([prompt-caching]), a 1h write is priced at 2× base input
//! against 1.25× for 5m — 60% more per creation. This module used to conclude
//! that the premium is therefore free on a subscription, "where the usage window
//! is token-counted", and a real bill only on PAYG. Treat that as unproven:
//! `bench/fit_weights.py` scores an equal-weight window worst of the five
//! hypotheses it tests, at R² 0.097 against 0.436 for weights that keep the
//! published write multipliers. The PAYG gate at the call site stands either
//! way; what changed is that pinning the tail to 1h is no longer assumed
//! costless. See [`tail_5m_prefix_1h`].
//!
//! [rate-limits]: https://platform.claude.com/docs/en/api/rate-limits
//! [prompt-caching]: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
//!
//! # What it is worth here
//!
//! Less than it sounds, for two reasons the measurements turned up.
//!
//! The 5-minute cache **refreshes on every use, at no cost**, so during
//! sustained work it never lapses — B1 only ever rescues a gap since the *last
//! touch*, not since creation. On the 39-capture corpus no gap between
//! consecutive turns exceeded 231 seconds, so B1 would have rescued nothing.
//! That corpus is a single 13-minute burst though, which structurally cannot
//! contain the gaps B1 exists to survive; it is evidence that B1 does nothing
//! during sustained work, not evidence about resuming after lunch.
//!
//! And Claude Code already asks for `1h` itself on the main conversation. In the
//! corpus only subagent traffic used the 5m default, and subagents run to
//! completion in seconds. Against this client B1 mostly restates what the client
//! already does; it earns its keep against clients that never set a TTL.
//!
//! # One-time cost
//!
//! A 5m and a 1h breakpoint are different cache entries, so the turn that flips
//! a marker writes a fresh prefix. Turning B1 on costs one cache creation per
//! affected conversation.
//!
//! No beta header is needed: the 1-hour TTL went generally available on
//! 2025-08-13 and `extended-cache-ttl-2025-04-11` is no longer required.

use serde_json::Value;

/// The TTL string Anthropic accepts for the extended cache.
const ONE_HOUR: &str = "1h";

/// The default tier, and the cheap one: 1.25x base input against 1h's 2.0x.
const FIVE_MINUTES: &str = "5m";

/// Rewrite one marker in place to `ttl`. Returns `true` when bytes changed.
fn pin_marker_to(marker: &mut Value, ttl: &str) -> bool {
    let Some(obj) = marker.as_object_mut() else {
        return false;
    };
    // Only ephemeral markers carry a TTL. Anything else is a shape we do not
    // recognise and must not rewrite.
    if obj.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return false;
    }
    if obj.get("ttl").and_then(Value::as_str) == Some(ttl) {
        return false;
    }
    obj.insert("ttl".to_string(), Value::String(ttl.to_string()));
    true
}

/// Rewrite one marker in place. Returns `true` when bytes changed.
fn pin_marker(marker: &mut Value) -> bool {
    pin_marker_to(marker, ONE_HOUR)
}

/// 1h on the tools and system prefix, 5m on every message marker.
///
/// The docstring above prices a 1h write at 2.0x base input against 5m's 1.25x
/// and then argues the difference is free on a subscription, "where the usage
/// window is token-counted". `bench/fit_weights.py` scores that hypothesis —
/// "every token equal" — worst of the five it tests, at R² 0.097 against 0.436
/// for weights that keep the published write multipliers. So the 60% premium is
/// most likely being paid, and it is being paid on the wrong thing.
///
/// Nearly every created token is the moving tail: content one turn appends and
/// the next turn's marker supersedes seconds later. Turns arrive a median of 9
/// seconds apart, and a cache read refreshes its entry for free, so the tail
/// never needs an hour of retention — it needs to survive until the next turn.
/// The tools and system prefix is the opposite case: written once, read for the
/// life of the session, and the thing that has to survive a lunch break. It
/// keeps the long TTL, and it is what a conversation falls back to when the
/// tail does lapse.
///
/// Anthropic requires longer TTLs to appear before shorter ones, which this
/// ordering satisfies: tools, then system, then messages.
///
/// The exposure is the gap longer than five minutes: the tail lapses, the read
/// falls back to the system marker, and the whole message history is rewritten.
/// On its own that costs more than it saves — priced on the 2026-08-16 capture,
/// an all-5m tail scores -9.5% against an all-1h tail's -16.9%.
///
/// So the tail takes the long TTL every [`ANCHOR_EVERY_TURNS`] turns, leaving a
/// 1h entry a bounded distance behind. A lapse then rewrites only the turns
/// since that anchor instead of the conversation. Anthropic bills three
/// positions — read to the highest hit `A`, 1h-write for `(B - A)`, 5m-write
/// for `(C - B)` — so an anchor turn writes its own span once at 2.0x and
/// nothing twice. That is -32.0% on the same capture, against -16.9% for the
/// all-1h tail, and the curve is flat from every 5 turns to every 20.
///
/// The anchor is chosen from the message count rather than a counter so it is a
/// pure function of the body: a retried turn picks the same TTL, and a TTL that
/// changed under retry would write a second entry for bytes already cached.
pub fn tail_5m_prefix_1h(body: &mut Value) -> bool {
    let anchor = is_anchor_turn(body);
    let tail_ttl = if anchor { ONE_HOUR } else { FIVE_MINUTES };
    let mut changed = false;

    for key in ["tools", "system"] {
        if let Some(entries) = body.get_mut(key).and_then(Value::as_array_mut) {
            for entry in entries {
                if let Some(marker) = entry.get_mut("cache_control") {
                    changed |= pin_marker_to(marker, ONE_HOUR);
                }
            }
        }
    }

    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(marker) = message.get_mut("cache_control") {
                changed |= pin_marker_to(marker, tail_ttl);
            }
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    if let Some(marker) = block.get_mut("cache_control") {
                        changed |= pin_marker_to(marker, tail_ttl);
                    }
                }
            }
        }
    }

    changed
}

/// How often the message tail takes the 1-hour tier instead of the 5-minute one.
///
/// Every anchor turn costs its own span at 2.0x rather than 1.25x, and bounds
/// what a lapse can destroy to the turns since the last one. Ten is the minimum
/// of a flat curve: every 5 scores -30.9%, every 10 -32.0%, every 20 -30.7%.
const ANCHOR_EVERY_TURNS: usize = 10;

/// Whether this turn should leave a 1-hour anchor behind.
///
/// Claude Code adds two messages a turn — the assistant's reply and the user's
/// next input or tool result — so the message count is a turn counter that
/// lives in the request rather than in the proxy. Growth that is not exactly
/// two only moves an anchor a turn early or late, which the flat curve absorbs.
fn is_anchor_turn(body: &Value) -> bool {
    let count = body
        .get("messages")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    (count / 2) % ANCHOR_EVERY_TURNS == 0
}

/// Pin `cache_control.ttl` to `1h` on every marker in `body`.
///
/// Returns `true` when any marker changed. A `false` return leaves `body`
/// untouched, byte for byte.
pub fn force_1h_ttl(body: &mut Value) -> bool {
    let mut changed = false;

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(marker) = tool.get_mut("cache_control") {
                changed |= pin_marker(marker);
            }
        }
    }

    if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
        for block in blocks {
            if let Some(marker) = block.get_mut("cache_control") {
                changed |= pin_marker(marker);
            }
        }
    }

    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            // A message-level marker is legal alongside block-level ones.
            if let Some(marker) = message.get_mut("cache_control") {
                changed |= pin_marker(marker);
            }
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    if let Some(marker) = block.get_mut("cache_control") {
                        changed |= pin_marker(marker);
                    }
                }
            }
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pins_markers_in_all_three_positions() {
        let mut body = json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral"}}],
            "system": [
                {"type": "text", "text": "s"},
                {"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}]
            }]
        });
        assert!(force_1h_ttl(&mut body));
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
        // The block without a marker must not grow one — placement is the
        // client's call, we only change the duration.
        assert!(body["system"][0].get("cache_control").is_none());
    }

    /// The split has to land on the right side of every marker: the prefix that
    /// survives an idle gap keeps the hour, the tail that is superseded in
    /// seconds takes the cheap tier.
    #[test]
    fn split_gives_the_prefix_an_hour_and_the_tail_five_minutes() {
        let mut body = json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
            "system": [
                {"type": "text", "text": "s"},
                {"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}}
            ],
            // Two messages is turn one, and 1 % 10 is not 0, so this is an
            // ordinary turn and the tail takes the cheap tier.
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "u"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "v", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}
            ]
        });
        assert!(tail_5m_prefix_1h(&mut body));
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["ttl"],
            "5m",
            "the moving tail is what the cheap tier is for"
        );
        assert!(
            body["system"][0].get("cache_control").is_none(),
            "placement stays the client's call"
        );
    }

    /// Anthropic requires longer TTLs before shorter ones, and the split relies
    /// on that ordering holding for the body it is handed: tools, then system,
    /// then messages. A body already in the target shape must not move bytes.
    #[test]
    fn split_is_a_no_op_once_applied() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "u"}]},
                {"role": "assistant", "content": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]}
            ]
        });
        let before = body.clone();
        assert!(!tail_5m_prefix_1h(&mut body));
        assert_eq!(body, before);
    }

    /// Every tenth turn leaves a 1-hour entry behind, so a lapse rewrites the
    /// turns since it rather than the whole conversation.
    #[test]
    fn an_anchor_turn_gives_the_tail_the_long_ttl() {
        for (messages, want) in [(20usize, "1h"), (22, "5m"), (40, "1h"), (24, "5m")] {
            let mut content: Vec<Value> = (0..messages - 1)
                .map(|_| json!({"role": "user", "content": [{"type": "text", "text": "x"}]}))
                .collect();
            content.push(json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "y", "cache_control": {"type": "ephemeral"}}]
            }));
            let mut body = json!({"messages": content});
            tail_5m_prefix_1h(&mut body);
            assert_eq!(
                body["messages"][messages - 1]["content"][0]["cache_control"]["ttl"],
                want,
                "{messages} messages should pick {want}"
            );
        }
    }

    /// The choice has to come from the body alone. A retry that picked a
    /// different TTL would write a second entry for bytes already cached.
    #[test]
    fn the_anchor_choice_is_a_pure_function_of_the_body() {
        let body = json!({
            "messages": (0..21)
                .map(|_| json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]
                }))
                .collect::<Vec<_>>()
        });
        let (mut a, mut b) = (body.clone(), body.clone());
        tail_5m_prefix_1h(&mut a);
        tail_5m_prefix_1h(&mut b);
        assert_eq!(a, b);
    }

    /// Claude Code already sends `1h` on the main conversation. That has to be a
    /// byte-identical no-op or B1 would bust the very cache it protects.
    #[test]
    fn already_1h_is_a_no_op() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]
        });
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// An explicit 5m is the client asking for the default; B1's whole job is to
    /// override it.
    #[test]
    fn explicit_5m_is_upgraded() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]
        });
        assert!(force_1h_ttl(&mut body));
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    }

    /// A body with no markers at all is untouched — B1 never creates a
    /// breakpoint, because creating one costs a full cache write.
    #[test]
    fn no_markers_is_a_no_op() {
        let mut body = json!({
            "system": [{"type": "text", "text": "s"}],
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "a"}]
        });
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// An unrecognised marker shape is left alone rather than guessed at.
    #[test]
    fn non_ephemeral_marker_is_left_alone() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "persistent"}}]
        });
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// String content (rather than a block array) has nowhere to carry a marker.
    #[test]
    fn string_content_messages_are_skipped() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// Running twice must not change anything the second time.
    #[test]
    fn is_idempotent() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral"}}]
        });
        assert!(force_1h_ttl(&mut body));
        let once = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, once);
    }
}
