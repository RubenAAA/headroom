//! Proof that the provider-confirmed floor from upstream `aebe9895` holds in the
//! Rust overlay alongside the branch's withdrawal span alignment.
//!
//! Upstream split the non-inflation size bound at the provider-confirmed
//! prefix: inside the floor the replay is unconditional (those bytes are
//! exactly what the provider hashed), beyond it the bound still arbitrates —
//! a shrinking replay repairs drift, an inflating one lets the fresh
//! improvement reach the wire, and a collapsed floor (cold cache) lands every
//! accumulated improvement at once. This overlay never had the bound, so the
//! port adds both, reconciled with the withdrawal replay the branch added:
//! a withdrawal-shifted span is atomic (see
//! `split_inflated_replay_at_floor`), because resuming the tail mid-span
//! would read a shifted index and drop client content.

use headroom_proxy::cache_stabilization::prefix_replay::{
    overlay_cached_prefix, overlay_cached_prefix_reported, PrefixReplayTracker, ReplaySkip,
    SessionReplayStore,
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

/// Last turn forwarded the big forms; this turn's pipeline emits recompressed
/// tiny forms for the same history plus one fresh tail message.
fn recompressed_pair() -> (Vec<Value>, Vec<Value>, Vec<Value>, Vec<Value>) {
    let prev_orig = vec![
        user(&"old output\n".repeat(40)),
        user("newer output"),
        assistant("ok"),
    ];
    let prev_fwd = vec![
        user(&"F".repeat(500)),
        user(&"Z".repeat(300)),
        assistant("ok"),
    ];
    let mut current = prev_orig.clone();
    current.push(user("next"));
    let optimized = vec![user("[t0]"), user("[t1]"), assistant("ok"), user("next")];
    (prev_orig, prev_fwd, current, optimized)
}

#[test]
fn recompression_does_not_drop_the_confirmed_prefix() {
    // Background recompression landed a much smaller form of already-forwarded,
    // provider-confirmed history. The floor replays those bytes
    // unconditionally: forwarding the smaller fresh form instead would bust
    // the warm cache from the first changed byte.
    let big = "tool output line\n".repeat(120);
    let prev_orig = vec![user(&big), assistant("ok"), user("next question")];
    let prev_fwd = prev_orig.clone();
    let mut current = prev_orig.clone();
    current.push(assistant("new reply"));
    let optimized = vec![
        user("[t0]"),
        assistant("ok"),
        user("next question"),
        assistant("new reply"),
    ];

    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(3),
    );
    assert_eq!(skip, None);
    assert_eq!(
        out[..3],
        prev_fwd[..],
        "confirmed bytes win over the smaller fresh form"
    );
    assert_eq!(out[3], optimized[3], "this turn's tail is preserved");

    // The no-floor shim keeps the historical unbounded posture.
    let legacy = overlay_cached_prefix(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
    );
    assert_eq!(legacy, out);
}

#[test]
fn improvement_beyond_the_floor_lands_while_the_confirmed_head_replays() {
    let (prev_orig, prev_fwd, current, optimized) = recompressed_pair();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(1),
    );
    assert_eq!(skip, None);
    assert_eq!(
        out[0], prev_fwd[0],
        "confirmed region replays unconditionally"
    );
    assert_eq!(
        out[1], optimized[1],
        "improvement beyond the floor reaches the wire"
    );
    assert_eq!(out[2], optimized[2]);
    assert_eq!(out[3], optimized[3]);
}

#[test]
fn cold_cache_lets_the_accumulated_improvement_land_at_once() {
    // A collapsed floor (cold cache, TTL lapse) is the re-baseline: nothing is
    // confirmed, the inflated replay is declined whole, and the fresh pipeline
    // output — every improvement accumulated while warm — reaches the wire.
    let (prev_orig, prev_fwd, current, optimized) = recompressed_pair();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(0),
    );
    assert_eq!(out, optimized);
    assert_eq!(skip, Some(ReplaySkip::InflatedWithoutConfirmedFloor));
}

#[test]
fn alignment_guards_hold_regardless_of_the_floor() {
    // The floor relaxes ONLY the size bound; every alignment guard still bails.
    let prev_orig = vec![user("first"), assistant("reply"), user("second")];
    let prev_fwd = prev_orig.clone();
    let current = vec![
        user("TOTALLY DIFFERENT"),
        assistant("reply"),
        user("second"),
        user("more"),
    ];
    let optimized = current.clone();

    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(2),
    );
    assert_eq!(out, optimized);
    assert!(
        matches!(
            skip,
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0,
                ..
            })
        ),
        "the floor must not override a divergence: {skip:?}"
    );

    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd[..1]),
        true,
        Some(2),
    );
    assert_eq!(out, optimized);
    assert_eq!(skip, Some(ReplaySkip::ForwardedCountMismatch));
}

#[test]
fn a_withdrawal_shifted_span_replays_whole_under_a_partial_floor() {
    // Stored pair AFTER an earlier turn replayed a withdrawn reminder: the
    // forwarded slice runs one longer than its originals, so the two no longer
    // share an index past the insertion point. Recompression additionally made
    // this turn's output much smaller, so the bound trips — and still the span
    // must replay whole: resuming the tail mid-span would read a shifted
    // current-space index and drop client content.
    let prev_orig = vec![
        user("the user's actual question"),
        assistant("first reply"),
        user("second question"),
        assistant("second reply"),
        user("third question"),
    ];
    let prev_fwd = vec![
        user(&"H".repeat(400)),
        scaffolding(),
        assistant(&"R".repeat(400)),
        user(&"H".repeat(400)),
        assistant(&"R".repeat(400)),
        user(&"H".repeat(400)),
    ];
    let mut current = prev_orig.clone();
    current.push(assistant("third reply"));
    current.push(user("fourth question"));
    let optimized = vec![
        user("[c0]"),
        assistant("[c1]"),
        user("[c2]"),
        assistant("[c3]"),
        user("[c4]"),
        assistant("[c5]"),
        user("[c6]"),
    ];

    // The splice the overlay computes without a bound: the whole forwarded
    // span, then this turn's tail from where the stored prefix stops covering
    // the current messages.
    let expected: Vec<Value> = prev_fwd.iter().chain(&optimized[5..]).cloned().collect();

    // Partial floor over a shifted span: the span is atomic, replayed whole.
    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(2),
    );
    assert_eq!(skip, None);
    assert_eq!(
        out, expected,
        "a partial floor must not split a withdrawal-shifted span"
    );
    assert_eq!(
        out[1],
        scaffolding(),
        "the replayed withdrawal is preserved"
    );

    // A floor covering the span skips the bound entirely.
    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(99),
    );
    assert_eq!(skip, None);
    assert_eq!(out, expected);

    // A zero floor still sends the turn out fresh: nothing confirmed, and a
    // fresh turn is always safe regardless of any shift.
    let (out, skip) = overlay_cached_prefix_reported(
        optimized.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        Some(0),
    );
    assert_eq!(out, optimized);
    assert_eq!(skip, Some(ReplaySkip::InflatedWithoutConfirmedFloor));
}

#[test]
fn the_tracker_confirmed_floor_preserves_the_withdrawal_span() {
    // The production wiring: the floor comes from the tracker's
    // provider-confirmed count, and the withdrawal replay survives it — the
    // branch invariant under the call the live path actually makes.
    let mut tracker = PrefixReplayTracker::default();
    let first = vec![
        user("the user's actual question"),
        scaffolding(),
        assistant("first reply"),
        user("second question"),
    ];
    tracker.update_from_response(20_000, 0, &first, Some(&first), String::new());
    assert_eq!(tracker.frozen_message_count(), 4);

    let current = vec![
        user("the user's actual question"),
        assistant("first reply"),
        user("second question"),
        assistant("second reply"),
        user("third question"),
    ];
    let (forwarded, skip) = overlay_cached_prefix_reported(
        current.clone(),
        &current,
        Some(tracker.last_original_messages()),
        Some(tracker.last_forwarded_messages()),
        true,
        Some(tracker.frozen_message_count()),
    );
    assert_eq!(skip, None);
    assert_eq!(
        forwarded.len(),
        current.len() + 1,
        "the replayed reminder must make the forwarded body one longer"
    );
    assert_eq!(forwarded[1], scaffolding());
}

#[test]
fn the_store_reports_the_provider_confirmed_floor() {
    let store = SessionReplayStore::new(4);
    assert_eq!(
        store.confirmed_frozen_count("sess-a"),
        0,
        "an unknown session confirms nothing"
    );

    let originals = vec![user("q1"), assistant("r1"), user("q2")];
    store.begin_request(
        "req-1",
        "sess-a",
        originals.clone(),
        originals.clone(),
        String::new(),
    );
    store.complete("req-1", 50_000, 0);
    assert_eq!(store.confirmed_frozen_count("sess-a"), 3);

    // A cold provider count collapses the floor with it, so the next turn
    // re-baselines instead of pinning.
    store.begin_request(
        "req-2",
        "sess-a",
        originals.clone(),
        originals.clone(),
        String::new(),
    );
    store.complete("req-2", 0, 0);
    assert_eq!(store.confirmed_frozen_count("sess-a"), 0);
}
