//! Where hidden continuation rounds leave the cache marker. Split out of
//! `proxy/tests.rs` to keep it under the file-size limit.

use super::*;

/// Where every `cache_control` sits on `messages`, as `(message, block)`.
fn ccr_marker_positions(body: &[u8]) -> Vec<(usize, usize)> {
    let v: serde_json::Value = serde_json::from_slice(body).expect("continuation body is JSON");
    let mut out = Vec::new();
    for (i, m) in v["messages"].as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
            if b.get("cache_control").is_some() {
                out.push((i, j));
            }
        }
    }
    out
}

/// The first hidden CCR round leaves the newest marker on the client's own
/// last block, so the provider's newest entry is one the next client turn
/// can read. A later round still follows the tail, and `0` restores the old
/// placement. Message content is identical across all three.
#[test]
fn ccr_continuation_keeps_the_client_boundary_for_the_first_round() {
    let request = || {
        serde_json::json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]},
            {"role": "assistant", "content": [{"type": "text", "text": "b"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}}]},
        ]})
    };
    let assistant = || {
        serde_json::json!({"role": "assistant", "content": [
        {"type": "tool_use", "id": "t1", "name": "headroom_retrieve", "input": {}}]})
    };
    let results = || {
        serde_json::json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": "full text"}]})
    };
    let mut config =
        crate::config::Config::for_test("https://api.anthropic.com".parse().expect("upstream URL"));
    config.cache_tail_breakpoint = true;
    let run = |config: &crate::config::Config, round: usize| {
        let mut req = request();
        // A second hidden round starts from the body the first one sent.
        let rounds = (0..=round).map(|r| {
            let body = ccr_response::build_ccr_continuation(
                &mut req,
                "messages",
                assistant(),
                results(),
                "anthropic",
                config,
                "ccr-marker",
                r,
            )
            .expect("continuation builds");
            ccr_marker_positions(&body)
        });
        rounds.last().unwrap()
    };

    config.ccr_keep_client_boundary_rounds = 1;
    assert_eq!(
        run(&config, 0),
        vec![(0, 0), (2, 0)],
        "round 1 keeps the boundary"
    );
    assert_eq!(
        run(&config, 1),
        vec![(0, 0), (6, 0)],
        "round 2 follows the tail"
    );

    config.ccr_keep_client_boundary_rounds = 0;
    assert_eq!(
        run(&config, 0),
        vec![(0, 0), (4, 0)],
        "0 restores the old placement"
    );
}

/// The memory-tool continuation follows the same policy under its own flag:
/// 15 of 16 aftershock recaches from 2026-09-30 to 10-02 came after a memory
/// round that moved the marker onto the hidden tool result.
#[test]
fn memory_continuation_keeps_the_client_boundary_for_the_first_round() {
    let request = || {
        serde_json::json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}]},
            {"role": "assistant", "content": [{"type": "text", "text": "b"}]},
            {"role": "user", "content": [
                {"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}}]},
        ]})
    };
    let assistant = || {
        serde_json::json!({"role": "assistant", "content": [
        {"type": "tool_use", "id": "t1", "name": "memory_search", "input": {}}]})
    };
    let results = || {
        serde_json::json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": "a memory"}]})
    };
    let upstream: url::Url = "https://api.anthropic.com".parse().expect("upstream URL");
    let mut config = crate::config::Config::for_test(upstream.clone());
    config.cache_tail_breakpoint = true;
    let run = |config: &crate::config::Config, round: usize| {
        let mut req = request();
        let rounds = (0..=round).map(|r| {
            let body = memory_continuation::append_round_messages(
                &mut req,
                "messages",
                assistant(),
                results(),
                "anthropic",
                config,
                "memory-marker",
                r,
                &upstream,
            )
            .expect("continuation builds");
            ccr_marker_positions(&body)
        });
        rounds.last().unwrap()
    };

    config.memory_keep_client_boundary_rounds = 1;
    assert_eq!(
        run(&config, 0),
        vec![(0, 0), (2, 0)],
        "round 1 keeps the boundary"
    );
    assert_eq!(
        run(&config, 1),
        vec![(0, 0), (6, 0)],
        "round 2 follows the tail"
    );

    config.memory_keep_client_boundary_rounds = 0;
    assert_eq!(
        run(&config, 0),
        vec![(0, 0), (4, 0)],
        "0 keeps the old placement"
    );
}
