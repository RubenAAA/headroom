use super::*;
use serde_json::json;

fn msg(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn diverge_at(prev: &[Value], current: &[Value]) -> Option<usize> {
    match overlay_cached_prefix_reported(
        current.to_vec(),
        current,
        Some(prev),
        Some(prev),
        true,
        None,
    )
    .1
    {
        Some(ReplaySkip::PrefixContentDiverged {
            first_diff_index, ..
        }) => Some(first_diff_index),
        _ => None,
    }
}

/// The live shape: something in the opener churns per request, so every
/// turn declines at index 0 however long the conversation grows.
#[test]
fn churn_in_the_opener_is_reported_at_index_zero() {
    let prev = vec![msg("opener @ 10:00"), msg("a"), msg("b")];
    let current = vec![msg("opener @ 10:05"), msg("a"), msg("b"), msg("c")];
    assert_eq!(diverge_at(&prev, &current), Some(0));
}

/// A real edit deeper in the history must be reported where it happened,
/// because that calls for the opposite response — refusing is right.
#[test]
fn an_edit_deeper_in_the_history_reports_its_own_index() {
    let prev = vec![msg("a"), msg("b"), msg("c"), msg("d")];
    let current = vec![msg("a"), msg("b"), msg("EDITED"), msg("d"), msg("e")];
    assert_eq!(diverge_at(&prev, &current), Some(2));
}

/// Transport churn the canonicalizer already neutralises must not be
/// reported as a divergence at all — otherwise the index would point at
/// noise and send the reader chasing the wrong thing.
#[test]
fn annotation_churn_is_not_a_divergence() {
    let prev = vec![json!({
        "role": "user",
        "content": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]
    })];
    let current = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
        msg("b"),
    ];
    assert_eq!(
        diverge_at(&prev, &current),
        None,
        "a moved cache_control marker is not a content change"
    );
}
