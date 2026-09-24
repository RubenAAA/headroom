use super::*;
use serde_json::json;

fn msg(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

#[test]
fn an_unknown_session_is_named_as_having_no_tracker() {
    let store = SessionReplayStore::new(8);
    assert_eq!(
        store.previous_turn_detailed("never-seen"),
        Err(PrefixMiss::NoTrackerForSession)
    );
}

#[test]
fn parking_a_turn_does_not_yet_create_a_replayable_prefix() {
    let store = SessionReplayStore::new(8);
    store.begin_request("r1", "S", vec![msg("a")], vec![msg("a")], String::new());
    // `begin_request` only fills the pending map; the tracker appears when
    // the response completes. Until then the session looks untracked.
    assert_eq!(
        store.previous_turn_detailed("S"),
        Err(PrefixMiss::NoTrackerForSession),
        "parking alone must not create a replayable prefix"
    );
}

#[test]
fn a_completed_turn_yields_its_prefix() {
    let store = SessionReplayStore::new(8);
    let orig = vec![msg("a")];
    let fwd = vec![msg("compressed-a")];
    store.begin_request("r1", "S", orig.clone(), fwd.clone(), String::new());
    store.complete("r1", 5_000, 0);
    assert_eq!(store.previous_turn_detailed("S"), Ok((orig, fwd)));
}

/// An idle gap past the TTL must stay distinguishable from a tracker that
/// went missing on a live session — the two have different causes and, as
/// [`SESSION_TTL`] records, very different costs.
#[test]
fn an_idle_session_is_named_as_ttl_rather_than_missing() {
    let mut store = SessionReplayStore::new(8);
    store.set_session_ttl_for_test(Duration::from_millis(1));
    store.begin_request("r1", "S", vec![msg("a")], vec![msg("a")], String::new());
    store.complete("r1", 5_000, 0);
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        store.previous_turn_detailed("S"),
        Err(PrefixMiss::IdlePastTtl)
    );
}

/// A tracker must outlive the provider's entry, not die before it. Holding a
/// prefix for less time than the cache it describes guarantees a rewrite for
/// any conversation resumed in the gap — 433,366 tokens on one such turn.
#[test]
fn a_tracker_outlives_the_cache_entry_it_describes() {
    assert_eq!(SessionReplayStore::new(8).session_ttl, SESSION_TTL);
    assert!(
        SESSION_TTL >= PERSIST_MAX_AGE,
        "memory must not forget a prefix the disk copy still considers fresh"
    );
    assert!(
        SESSION_TTL >= Duration::from_secs(3600),
        "the proxy forces a 1h provider TTL; a shorter tracker life is a rewrite"
    );
}

#[test]
fn miss_labels_are_stable_and_distinct() {
    let all = [
        PrefixMiss::NoTrackerForSession,
        PrefixMiss::IdlePastTtl,
        PrefixMiss::NothingForwardedYet,
        PrefixMiss::LockPoisoned,
    ];
    let labels: std::collections::HashSet<_> = all.iter().map(|m| m.as_str()).collect();
    assert_eq!(labels.len(), all.len());
}
