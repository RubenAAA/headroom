use super::*;
use serde_json::json;

fn client_msg(text: &str) -> Value {
    json!({"role": "user", "content": [
        {"type": "tool_result", "content": [{"type": "text", "text": text}]}
    ]})
}

/// What `ctx_offload` does to a message: collapse the block it holds.
fn as_offloaded(m: &Value) -> Value {
    let mut out = m.clone();
    out["content"] = json!([{"type": "text", "text": "<ctx-ref/>"}]);
    out
}

/// The bug, reproduced. Feed the guard OUR rewritten bodies and it declines
/// an append-only turn — the client never changed a thing.
#[test]
fn comparing_our_own_rewrites_wrongly_declines() {
    let client_turn1 = [client_msg("a")];
    // Last turn we offloaded it; this turn we did not (or vice versa).
    let ours_prev = vec![as_offloaded(&client_turn1[0])];
    let ours_now = vec![client_msg("a"), client_msg("b")];
    let (_, skip) = overlay_cached_prefix_reported(
        ours_now.clone(),
        &ours_now,
        Some(&ours_prev),
        Some(&ours_prev),
        true,
        None,
    );
    assert!(
        matches!(skip, Some(ReplaySkip::PrefixContentDiverged { .. })),
        "this is the defect: our own offload churn reads as a client edit"
    );
}

/// The fix. Hand the guard the CLIENT's messages on both sides and the same
/// turn replays cleanly, however we chose to rewrite the wire bytes.
#[test]
fn comparing_the_clients_own_messages_replays_cleanly() {
    let client_prev = vec![client_msg("a")];
    let client_now = vec![client_msg("a"), client_msg("b")];
    // Forwarded bytes may be anything — offloaded, compressed, whatever.
    let forwarded_prev = vec![as_offloaded(&client_prev[0])];
    let (out, skip) = overlay_cached_prefix_reported(
        client_now.clone(),
        &client_now,
        Some(&client_prev),
        Some(&forwarded_prev),
        true,
        None,
    );
    assert_eq!(skip, None, "an append-only client turn must replay");
    assert_eq!(
        out[0], forwarded_prev[0],
        "and it must replay the bytes we actually forwarded last turn"
    );
}

/// The guard must still catch a real client edit, or the fix has simply
/// traded a false decline for forwarding content the client did not send.
#[test]
fn a_real_client_edit_is_still_caught() {
    let client_prev = vec![client_msg("a")];
    let client_now = vec![client_msg("EDITED"), client_msg("b")];
    let forwarded_prev = vec![as_offloaded(&client_prev[0])];
    let (out, skip) = overlay_cached_prefix_reported(
        client_now.clone(),
        &client_now,
        Some(&client_prev),
        Some(&forwarded_prev),
        true,
        None,
    );
    assert!(skip.is_some(), "a genuine edit must still decline");
    assert_eq!(out, client_now, "and forward the client's own bytes");
}
