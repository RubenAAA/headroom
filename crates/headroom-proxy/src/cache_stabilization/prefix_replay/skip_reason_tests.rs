use super::*;
use serde_json::json;

fn msg(role: &str, text: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": text}]})
}

fn skip(
    optimized: Vec<Value>,
    current: &[Value],
    prev_orig: Option<&[Value]>,
    prev_fwd: Option<&[Value]>,
) -> Option<ReplaySkip> {
    overlay_cached_prefix_reported(optimized, current, prev_orig, prev_fwd, true, None).1
}

#[test]
fn a_replayed_turn_reports_no_reason() {
    let prev_orig = vec![msg("user", "one")];
    let prev_fwd = vec![msg("user", "compressed-one")];
    let current = vec![msg("user", "one"), msg("assistant", "two")];
    let (out, reason) = overlay_cached_prefix_reported(
        current.clone(),
        &current,
        Some(&prev_orig),
        Some(&prev_fwd),
        true,
        None,
    );
    assert_eq!(reason, None, "an append-only turn must replay");
    assert_eq!(out[0], prev_fwd[0], "the cached bytes must be forwarded");
}

#[test]
fn nothing_stored_is_named_rather_than_guessed() {
    let current = vec![msg("user", "one")];
    assert_eq!(
        skip(current.clone(), &current, None, None),
        Some(ReplaySkip::NoPreviousTurn)
    );
    // An empty stored prefix is the same situation, not a different one.
    assert_eq!(
        skip(current.clone(), &current, Some(&[]), Some(&[])),
        Some(ReplaySkip::NoPreviousTurn)
    );
}

/// The interleaved-stream fingerprint. One session slot holds one prefix, so
/// when a shorter stream's turn arrives after a longer stream's, it cannot
/// replay — and a conversation never shrinks on its own. This reason
/// appearing in production is what would confirm item 11's merge is costing
/// real tokens here, not just mis-reporting them.
#[test]
fn a_turn_shorter_than_the_stored_prefix_is_named_as_such() {
    let prev_orig = vec![
        msg("user", "one"),
        msg("assistant", "two"),
        msg("user", "three"),
    ];
    let prev_fwd = prev_orig.clone();
    let current = vec![msg("user", "one")];
    assert_eq!(
        skip(current.clone(), &current, Some(&prev_orig), Some(&prev_fwd)),
        Some(ReplaySkip::ShorterThanStoredPrefix)
    );
}

#[test]
fn a_diverged_client_prefix_is_named_as_such() {
    let prev_orig = vec![msg("user", "one")];
    let prev_fwd = vec![msg("user", "compressed-one")];
    // Same length, different content: the client rewrote its own history.
    let current = vec![msg("user", "one-EDITED"), msg("assistant", "two")];
    assert_eq!(
        skip(current.clone(), &current, Some(&prev_orig), Some(&prev_fwd)),
        Some(ReplaySkip::PrefixContentDiverged {
            first_diff_index: 0,
            replayed_prefix_msgs: 0,
        })
    );
}

#[test]
fn a_forwarded_count_mismatch_is_named_as_such() {
    let prev_orig = vec![msg("user", "one"), msg("assistant", "two")];
    let prev_fwd = vec![msg("user", "compressed-one")];
    let current = prev_orig.clone();
    assert_eq!(
        skip(current.clone(), &current, Some(&prev_orig), Some(&prev_fwd)),
        Some(ReplaySkip::ForwardedCountMismatch)
    );
}

#[test]
fn a_short_optimized_output_is_named_as_such() {
    let prev_orig = vec![msg("user", "one"), msg("assistant", "two")];
    let prev_fwd = prev_orig.clone();
    let current = vec![
        msg("user", "one"),
        msg("assistant", "two"),
        msg("user", "3"),
    ];
    // The pipeline collapsed the prefix away, so there is nothing to overlay.
    let optimized = vec![msg("user", "one")];
    assert_eq!(
        skip(optimized, &current, Some(&prev_orig), Some(&prev_fwd)),
        Some(ReplaySkip::OptimizedShorterThanPrefix)
    );
}

/// The labels reach dashboards, so they must not drift silently.
#[test]
fn reason_labels_are_stable_and_distinct() {
    let all = [
        ReplaySkip::NoPreviousTurn,
        ReplaySkip::ForwardedCountMismatch,
        ReplaySkip::ShorterThanStoredPrefix,
        ReplaySkip::OptimizedShorterThanPrefix,
        ReplaySkip::PrefixContentDiverged {
            first_diff_index: 0,
            replayed_prefix_msgs: 0,
        },
    ];
    let labels: std::collections::HashSet<_> = all.iter().map(|r| r.as_str()).collect();
    assert_eq!(labels.len(), all.len(), "labels must be distinct");
    assert_eq!(
        ReplaySkip::ShorterThanStoredPrefix.as_str(),
        "shorter_than_stored_prefix"
    );
}
