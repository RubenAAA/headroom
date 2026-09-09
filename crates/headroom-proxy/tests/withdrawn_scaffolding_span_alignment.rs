//! Proof that a replayed scaffolding message keeps the stored pair covering
//! one span of the conversation, and that the next splice is therefore gapless.
//!
//! Shape taken from a live persisted prefix
//! (`~/.local/state/headroom/replay-prefixes`, 2026-09-02). Of the 195 stored
//! there, 15 held a forwarded slice whose role sequence disagreed with its
//! originals, and in every one the counts matched exactly: 27 messages inserted
//! by the overlay, 27 client messages dropped. `b5ad94921c` (163 messages, 2
//! inserted, 2 dropped) and `cbc4e45dc3` (77 messages, 1 and 1) had both drawn
//! `messages.163: role 'system' must follow a 'user' message …` and
//! `messages.77: …` from the API, at exactly the index where their stored
//! forwarded slice ran out.

use headroom_proxy::cache_stabilization::prefix_replay::{
    overlay_cached_prefix, PrefixReplayTracker,
};
use serde_json::{json, Value};

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

/// The standalone bare-string reminder Claude Code withdraws and re-adds.
fn scaffolding() -> Value {
    json!({
        "role": "system",
        "content": "<system-reminder>\nThe following skills are available.\n</system-reminder>"
    })
}

/// Every `role: "system"` message must follow a `user` message; the API rejects
/// the request at the first that does not.
fn first_illegal_system(messages: &[Value]) -> Option<usize> {
    messages.iter().enumerate().position(|(index, message)| {
        message["role"] == "system"
            && index
                .checked_sub(1)
                .is_none_or(|before| messages[before]["role"] != "user")
    })
}

/// Turn 1: the client sends the reminder, and it is forwarded untouched.
fn turn_one() -> Vec<Value> {
    vec![
        user("the user's actual question"),
        scaffolding(),
        assistant("first reply"),
        user("second question"),
    ]
}

/// Turn 2: the reminder is gone from the middle of history and two messages
/// arrived at the end, so the count is unchanged — the shape that hid this for
/// so long.
fn turn_two() -> Vec<Value> {
    vec![
        user("the user's actual question"),
        assistant("first reply"),
        user("second question"),
        assistant("second reply"),
        user("third question"),
    ]
}

#[test]
fn the_stored_pair_covers_one_span_when_the_overlay_replays_a_withdrawn_reminder() {
    let mut tracker = PrefixReplayTracker::default();
    let first = turn_one();
    tracker.update_from_response(20_000, 0, &first, Some(&first), String::new());

    // The overlay puts the withdrawn reminder back: those are the bytes the
    // provider cached, and the client dropping the message does not change
    // what is sitting in the cache.
    let current = turn_two();
    let forwarded = overlay_cached_prefix(
        current.clone(),
        &current,
        Some(tracker.last_original_messages()),
        Some(tracker.last_forwarded_messages()),
    );
    assert_eq!(
        forwarded.len(),
        current.len() + 1,
        "the replayed reminder should make the forwarded body one longer"
    );
    assert_eq!(forwarded[1], scaffolding());

    tracker.update_from_response(20_000, 0, &forwarded, Some(&current), String::new());

    // The stored pair is the point. Both slices must end on the same message of
    // the conversation; storing a forwarded slice that stops earlier is what
    // opened the gap.
    let stored_original = tracker.last_original_messages().to_vec();
    let stored_forwarded = tracker.last_forwarded_messages().to_vec();
    assert_eq!(
        stored_forwarded.len(),
        stored_original.len() + 1,
        "the stored forwarded slice keeps the replayed message"
    );
    assert_eq!(
        stored_forwarded.last(),
        stored_original.last(),
        "both slices must end on the same message"
    );
}

#[test]
fn the_next_splice_drops_no_message_and_leaves_no_system_message_stranded() {
    let mut tracker = PrefixReplayTracker::default();
    let first = turn_one();
    tracker.update_from_response(20_000, 0, &first, Some(&first), String::new());

    let second = turn_two();
    let forwarded = overlay_cached_prefix(
        second.clone(),
        &second,
        Some(tracker.last_original_messages()),
        Some(tracker.last_forwarded_messages()),
    );
    tracker.update_from_response(20_000, 0, &forwarded, Some(&second), String::new());

    // Turn 3 appends a reply, a message, and a hook reminder behind it — the
    // tail Claude Code writes when a teammate wakes a long-lived agent.
    let mut third = second.clone();
    third.push(assistant("third reply"));
    third.push(user("<teammate-message>ship it</teammate-message>"));
    third.push(json!({
        "role": "system",
        "content": "<system-reminder>\nSubagentStart hook additional context\n</system-reminder>"
    }));
    assert_eq!(
        first_illegal_system(&third),
        None,
        "the client's own body is legal"
    );

    let out = overlay_cached_prefix(
        third.clone(),
        &third,
        Some(tracker.last_original_messages()),
        Some(tracker.last_forwarded_messages()),
    );

    assert_eq!(
        first_illegal_system(&out),
        None,
        "a system message ended up where the API refuses one: {out:#?}"
    );
    for (index, message) in third.iter().enumerate() {
        assert!(
            out.contains(message),
            "client message {index} never reached the wire: {message:#?}"
        );
    }
    assert_eq!(
        out.len(),
        third.len() + 1,
        "exactly the replayed reminder is added, and nothing is lost"
    );
}

#[test]
fn a_stranded_system_message_declines_the_replay_rather_than_forwarding_it() {
    // The net under the splice. Hand it a stored pair whose forwarded slice
    // ends one message early — the state this fix stops the store from
    // reaching — and it must forward this turn's own bytes instead of a body
    // the API would refuse.
    let previous_originals = vec![
        user("first question"),
        assistant("first reply"),
        user("second question"),
    ];
    let previous_forwarded = vec![user("first question"), assistant("first reply")];

    let mut current = previous_originals.clone();
    current.push(json!({
        "role": "system",
        "content": "<system-reminder>\nhook additional context\n</system-reminder>"
    }));

    let out = overlay_cached_prefix(
        current.clone(),
        &current,
        Some(&previous_originals),
        Some(&previous_forwarded),
    );
    assert_eq!(out, current, "declined, not spliced into an illegal body");
    assert_eq!(first_illegal_system(&out), None);
}
