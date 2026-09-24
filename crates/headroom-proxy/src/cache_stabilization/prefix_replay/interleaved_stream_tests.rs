use super::*;
use serde_json::json;

fn msg(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

/// Build a conversation of `n` messages on branch `tag`, sharing an opener
/// so it collides on the session key exactly as a subagent does.
fn stream(tag: &str, n: usize) -> Vec<Value> {
    let mut v = vec![msg("shared opener")];
    for i in 1..n {
        v.push(msg(&format!("{tag}-{i}")));
    }
    v
}

fn turn(store: &SessionReplayStore, rid: &str, orig: &[Value]) {
    // Forward a marked copy so a replay is identifiable by content.
    let fwd: Vec<Value> = orig.iter().map(|_| msg("compressed")).collect();
    store.begin_request(rid, "S", orig.to_vec(), fwd, String::new());
    store.complete(rid, 5_000, 0);
}

/// The defect, stated directly: A, then B, then A again. Under one slot per
/// session, A's second turn is tested against B's prefix and declines.
#[test]
fn alternating_streams_can_both_still_replay() {
    let store = SessionReplayStore::new(8);
    let a1 = stream("a", 3);
    let b1 = stream("b", 5);
    turn(&store, "a1", &a1);
    turn(&store, "b1", &b1);

    // A continues its own history, which B's stored prefix does not lead.
    let a2 = stream("a", 4);
    let (orig, fwd, _) = store
        .previous_turn_for("S", &a2, None)
        .expect("A's own prefix is still held");
    assert_eq!(orig, a1, "A must be matched against A, not against B");
    assert_eq!(fwd.len(), a1.len());

    // And the overlay accepts it, which is the whole point.
    let (_, skip) =
        overlay_cached_prefix_reported(a2.clone(), &a2, Some(&orig), Some(&fwd), true, None);
    assert_eq!(skip, None, "A's turn must replay rather than decline");
}

/// Two interleaved streams must get two chain ids, and each must keep its
/// own as it grows.
///
/// This is the whole point of the id. `conversation_key` hashes `system`
/// plus the first message, which these two share; message counts cannot
/// separate them either, because A at 4 messages following B at 5 looks
/// exactly like a compaction. Three conclusions were drawn from that
/// ambiguity on 2026-08-09 and all three were wrong.
#[test]
fn interleaved_streams_get_distinct_chain_ids() {
    let store = SessionReplayStore::new(8);
    let a1 = stream("a", 3);
    let b1 = stream("b", 5);
    turn(&store, "a1", &a1);
    turn(&store, "b1", &b1);

    let a2 = stream("a", 4);
    let b2 = stream("b", 6);
    let (_, _, a_id) = store
        .previous_turn_for("S", &a2, None)
        .expect("A's prefix held");
    let (_, _, b_id) = store
        .previous_turn_for("S", &b2, None)
        .expect("B's prefix held");
    assert_ne!(a_id, 0, "a matched chain must be named");
    assert_ne!(b_id, 0);
    assert_ne!(a_id, b_id, "two streams must not share a chain id");

    // And the id survives the stream growing.
    turn(&store, "a2", &a2);
    let (_, _, a_id_again) = store
        .previous_turn_for("S", &stream("a", 5), None)
        .expect("A's prefix still held");
    assert_eq!(a_id_again, a_id, "a chain keeps its id as it grows");
}

#[test]
fn a_turn_continuing_nothing_reports_no_chain() {
    let store = SessionReplayStore::new(8);
    turn(&store, "a1", &stream("a", 3));
    let unrelated = stream("c", 6);
    let (_, _, id) = store
        .previous_turn_for("S", &unrelated, None)
        .expect("the fallback still returns a prefix to report against");
    assert_eq!(
        id, 0,
        "continuing nothing must not borrow another chain's id"
    );
}

/// The safety property. A candidate is only ever returned when it
/// canonically leads this turn, so a wrong stream's bytes cannot be
/// forwarded even though several are held.
#[test]
fn a_stream_never_receives_another_streams_prefix() {
    let store = SessionReplayStore::new(8);
    turn(&store, "a1", &stream("a", 3));
    turn(&store, "b1", &stream("b", 5));

    // A conversation that continues neither branch.
    let c = stream("c", 6);
    let got = store.previous_turn_for("S", &c, None);
    if let Ok((orig, fwd, chain_id)) = got {
        assert_eq!(chain_id, 0, "the fallback must admit it continues nothing");
        // The fallback may hand back the most recent prefix, but the
        // overlay must then refuse it — nothing wrong is forwarded. Every
        // stream here opens with the same message, so the divergence is at
        // index 1 and the splice would otherwise take the bait: `chain_id`
        // is what stops it, exactly as the proxy passes it.
        let (out, skip) = overlay_cached_prefix_reported(
            c.clone(),
            &c,
            Some(&orig),
            Some(&fwd),
            chain_id != 0,
            None,
        );
        assert!(skip.is_some(), "an unrelated stream must not replay");
        assert_eq!(out, c, "the turn's own bytes must be forwarded untouched");
    }
}

/// A client that edits a message it already sent is still the same stream,
/// and must be told so — otherwise the overlay refuses to splice on exactly
/// the turn where splicing is worth the most.
///
/// This is what a content divergence looks like from the store's side: the
/// stored prefix stops being a prefix of the turn, so the exact match finds
/// nothing. Live traffic on 2026-08-16 declined here with 309 of 311
/// messages agreeing and re-created the whole conversation.
#[test]
fn a_stream_that_edits_its_own_tail_is_still_recognised() {
    let store = SessionReplayStore::new(8);
    let mut original = stream("a", 9);
    turn(&store, "a1", &original);

    // The client rewrites its last message and adds a new one — the shape
    // `<system-reminder>` churn produces.
    let last = original.len() - 1;
    original[last] = msg("a-8 edited");
    original.push(msg("a-9"));

    let (orig, fwd, chain_id) = store
        .previous_turn_for("S", &original, None)
        .expect("a tail edit must still find its own stream");
    assert_ne!(chain_id, 0, "an edited tail is not a stranger");

    let (out, skip) = overlay_cached_prefix_reported(
        original.clone(),
        &original,
        Some(&orig),
        Some(&fwd),
        chain_id != 0,
        None,
    );
    assert_eq!(
        skip,
        Some(ReplaySkip::PrefixContentDiverged {
            first_diff_index: last,
            replayed_prefix_msgs: last,
        }),
        "everything ahead of the edit is replayed"
    );
    assert!(
        out[..last].iter().all(|m| m == &msg("compressed")),
        "the agreeing run must come from the stored prefix"
    );
    assert_eq!(
        out[last..],
        original[last..],
        "the edited tail is this turn's own"
    );
}

/// Growth on one stream must not accumulate stale copies of itself, or an
/// old short prefix could win a later match and replay less than it should.
#[test]
fn continuing_a_stream_does_not_hoard_stale_copies() {
    let store = SessionReplayStore::new(8);
    turn(&store, "a1", &stream("a", 3));
    turn(&store, "a2", &stream("a", 4));
    turn(&store, "a3", &stream("a", 5));
    let a4 = stream("a", 6);
    let (orig, _, _) = store
        .previous_turn_for("S", &a4, None)
        .expect("prefix held");
    assert_eq!(orig.len(), 5, "the longest matching prefix must win");
}

/// The bound that matters: a few very long streams are held by total
/// messages, not by count, so one session cannot pin an unbounded pile of
/// full conversations.
#[test]
fn long_streams_are_bounded_by_messages_not_count() {
    let store = SessionReplayStore::new(8);
    // Each stream alone is a quarter of the budget, so far fewer than the
    // count ceiling may be held.
    let per = MAX_ALTERNATE_MESSAGES / 4;
    for i in 0..8 {
        turn(&store, &format!("r{i}"), &stream(&format!("s{i}"), per));
    }
    let guard = store.trackers.lock().unwrap();
    let t = guard.peek("S").expect("tracker");
    let held: usize = t.alternates.iter().map(|(_, o, _, _)| o.len()).sum();
    assert!(
        held <= MAX_ALTERNATE_MESSAGES,
        "held {held} messages, budget is {MAX_ALTERNATE_MESSAGES}"
    );
    assert!(
        t.alternates.len() < MAX_ALTERNATE_PREFIXES,
        "the message budget must bite before the count ceiling here"
    );
}

/// A main conversation must survive a fan-out that outnumbers it.
///
/// The defect this pins, measured 2026-08-23: with the count ceiling at 16,
/// a long conversation was dropped after exactly 17 subagent turns while
/// the store held 480 messages against a 4,000 budget. Eviction takes the
/// least-recently-displaced entry and a parent waiting on its subagents is
/// exactly that, so the most expensive prefix held was the first discarded.
/// Its next turn then found nothing leading it and re-cached the lot —
/// worth 2.59M tokens across 51 turns in the 2026-08-20/22 logs.
///
/// The ceiling must not be what bites; the message budget must.
#[test]
fn a_parent_conversation_outlives_a_large_fan_out() {
    let store = SessionReplayStore::new(8);
    let main = stream("main", 300);
    turn(&store, "m1", &main);
    // 64 streams is 2.2x the busiest session measured over the 2026-08-20/22
    // logs (max 29 distinct streams, p99 11), each the size of a real
    // subagent, and 1,920 messages — well inside the 4,000 budget. Neither
    // bound has any business firing here.
    for i in 0..64 {
        turn(&store, &format!("s{i}"), &stream(&format!("sub{i}"), 30));
    }
    let mut next = main.clone();
    next.push(msg("main next turn"));
    let (_, _, chain_id) = store
        .previous_turn_for("S", &next, None)
        .expect("the parent's prefix must still be held");
    assert_ne!(
        chain_id, 0,
        "the parent was evicted by a fan-out that fits the message budget,              so its next turn re-caches the whole prefix"
    );
    let guard = store.trackers.lock().unwrap();
    let held: usize = guard
        .peek("S")
        .expect("tracker")
        .alternates
        .iter()
        .map(|(_, o, _, _)| o.len())
        .sum();
    assert!(
        held <= MAX_ALTERNATE_MESSAGES,
        "held {held} messages, budget is {MAX_ALTERNATE_MESSAGES}"
    );
    // The count is a memory backstop, not the thing that picks a winner.
    // If it ever starts binding before the budget, this is the defect
    // returning under a larger number rather than a fix.
    assert!(
        held < MAX_ALTERNATE_MESSAGES,
        "the message budget must be what bites, never the count ceiling"
    );
}

/// The case the raised ceiling does not reach: a session that genuinely
/// runs out of message budget must spend what is left on the stream most
/// likely to want it back.
///
/// Measured over the 2026-08-20/22 logs, a stream's chance of ever taking
/// another turn climbs with its size — 0% under 10k cached tokens, 92%
/// above 120k. Recency ordering spent the budget the other way round,
/// evicting the one entry almost certain to return.
#[test]
fn the_budget_goes_to_the_stream_most_likely_to_return() {
    let store = SessionReplayStore::new(8);
    let main = stream("main", 2_000);
    turn(&store, "m1", &main);
    // Enough large subagents to blow the budget several times over, each
    // more recently active than the parent.
    for i in 0..12 {
        turn(&store, &format!("s{i}"), &stream(&format!("sub{i}"), 600));
    }
    let mut next = main.clone();
    next.push(msg("main next turn"));
    let (_, _, chain_id) = store
        .previous_turn_for("S", &next, None)
        .expect("a prefix comes back either way");
    assert_ne!(
        chain_id, 0,
        "the parent was evicted to hold smaller, more recent streams that \
             are far less likely to take another turn"
    );
}

/// The other direction: many SHORT streams — the subagent fan-out case —
/// must all be kept, since that is what the store exists for.
#[test]
fn many_short_streams_are_all_retained() {
    let store = SessionReplayStore::new(8);
    for i in 0..MAX_ALTERNATE_PREFIXES {
        turn(&store, &format!("r{i}"), &stream(&format!("s{i}"), 12));
    }
    let guard = store.trackers.lock().unwrap();
    let t = guard.peek("S").expect("tracker");
    // All but the newest (which is the primary) stay available.
    assert_eq!(
        t.alternates.len(),
        MAX_ALTERNATE_PREFIXES - 1,
        "short subagent streams must not evict each other"
    );
}

/// The sizing requirement, stated as a test: a `capture-beta` fan-out puts 8
/// subagents on one session key. Every one of them must still be able to
/// replay its own prefix after all the others have taken turns, or the
/// busiest sessions are exactly the ones that bust on every turn.
#[test]
fn eight_concurrent_subagents_can_all_still_replay() {
    let store = SessionReplayStore::new(8);
    let agents: Vec<Vec<Value>> = (0..8).map(|i| stream(&format!("agent{i}"), 30)).collect();
    // Every agent takes a turn, round-robin, twice over.
    for round in 0..2 {
        for (i, a) in agents.iter().enumerate() {
            let mut convo = a.clone();
            for r in 0..round {
                convo.push(msg(&format!("agent{i}-round{r}")));
            }
            turn(&store, &format!("r{round}-{i}"), &convo);
        }
    }
    // Now each agent extends its own history and must find its own prefix.
    for (i, a) in agents.iter().enumerate() {
        let mut next = a.clone();
        next.push(msg(&format!("agent{i}-round0")));
        next.push(msg(&format!("agent{i}-next")));
        let (orig, fwd, _) = store
            .previous_turn_for("S", &next, None)
            .unwrap_or_else(|e| panic!("agent{i} lost its prefix: {e:?}"));
        let (_, skip) = overlay_cached_prefix_reported(
            next.clone(),
            &next,
            Some(&orig),
            Some(&fwd),
            true,
            None,
        );
        assert_eq!(skip, None, "agent{i} was forced to decline and would bust");
    }
}

/// Bounded memory: a session that keeps spawning streams must not grow.
#[test]
fn alternates_are_capped() {
    let store = SessionReplayStore::new(8);
    for i in 0..(MAX_ALTERNATE_PREFIXES + 3) {
        turn(&store, &format!("r{i}"), &stream(&format!("s{i}"), 3 + i));
    }
    let guard = store.trackers.lock().unwrap();
    let t = guard.peek("S").expect("tracker");
    assert!(
        t.alternates.len() <= MAX_ALTERNATE_PREFIXES,
        "alternates grew to {}",
        t.alternates.len()
    );
}

/// The trap behind the read-before-invalidate contract in
/// `prior_thinking::thinking_drop_is_free`: the same call that drops the
/// stored prefix also erases the figure a caller would use to judge whether
/// dropping was safe, and erasure is reported as `None` — "nothing cached
/// to lose" — which is the answer that permits the rewrite.
#[test]
fn invalidate_erases_the_forwarded_agreement_reading() {
    let store = SessionReplayStore::new(8);
    let history = stream("a", 6);
    // Forward the originals verbatim so the agreement is the whole history
    // rather than the marker copies `turn` sends.
    store.begin_request("r1", "S", history.clone(), history.clone(), String::new());
    store.complete("r1", 5_000, 0);

    assert_eq!(
        store.forwarded_agreement_len("S", &history),
        Some(history.len()),
        "the lane agrees with everything it forwarded"
    );

    store.invalidate("S");

    assert_eq!(
        store.forwarded_agreement_len("S", &history),
        None,
        "read after the invalidation and the history the provider still \
             holds reads as no history at all"
    );
}

/// A rebuild boundary kills the provider's cache, so every prefix held for
/// that session is dead — including the alternates.
#[test]
fn invalidate_clears_alternates_too() {
    let store = SessionReplayStore::new(8);
    turn(&store, "a1", &stream("a", 3));
    turn(&store, "b1", &stream("b", 5));
    store.invalidate("S");
    assert_eq!(
        store.previous_turn_for("S", &stream("a", 4), None),
        Err(PrefixMiss::NothingForwardedYet),
        "no stream may replay across an invalidation"
    );
}
