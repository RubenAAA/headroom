//! CTX-3/CTX-4 acceptance harness — prefix-stability of the offload + injection
//! transforms.
//!
//! This is the acceptance gate for the whole context-mode-in-headroom project
//! (`docs/ctx-mode-in-headroom-plan.md`, invariants I1–I6). It simulates a
//! growing Anthropic conversation exactly like Claude Code drives it: every
//! turn the client resends the FULL original history (raw `tool_result`s and
//! all) and appends new messages. It runs the request-path transform pipeline
//! on each turn and proves the **upstream-visible prefix never drifts**: for
//! consecutive turns N and N+1, the transformed messages array of turn N is a
//! byte-identical prefix of turn N+1's.
//!
//! If this held only for ctx_offload in isolation it would be weak — so the
//! harness also runs a scenario with the live-zone compressor enabled on top,
//! covers a `tool_result` crossing from the live zone into the frozen zone, and
//! the same content appearing twice, and asserts our own digests trip zero
//! findings in the volatile-content detector.

use headroom_proxy::cache_stabilization::volatile_detector::{
    detect_volatile_content, ApiKind,
};
use headroom_proxy::compression::ctx_offload::{offload_anthropic_request, CtxOffloadConfig};
use headroom_proxy::compression::{compress_anthropic_request, Outcome};
use headroom_proxy::config::{CacheControlAutoFrozen, CompressionMode};
use headroom_proxy::ctx::inject::InjectEngine;

use headroom_core::auth_mode::AuthMode;
use headroom_core::ctx::{CtxStore, SessionsStore};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

const MIN_BYTES: usize = 2_000;

fn cfg() -> CtxOffloadConfig {
    CtxOffloadConfig {
        min_bytes: MIN_BYTES,
    }
}

/// A large, well-compressing tool_result body (> MIN_BYTES).
fn big_log(tag: &str) -> String {
    format!("[{tag}] ERROR: disk full while writing chunk\nretrying...\n").repeat(80)
}

/// One assistant tool_use + user tool_result turn pair.
fn tool_turn(id: &str, cmd: &str, output: &str) -> Vec<Value> {
    vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":id,"name":"Bash","input":{"command":cmd}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":id,"content":output}
        ]}),
    ]
}

/// Build a request body from a message list.
fn body(messages: &[Value]) -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "system": "You are a helpful assistant.",
        "messages": messages,
    })
}

/// Apply ctx_offload only. Returns the transformed body Value.
fn transform_offload_only(request: &Value) -> Value {
    let mut v = request.clone();
    offload_anthropic_request(&mut v, &cfg());
    v
}

/// Apply the full request-path pipeline: ctx_offload, then the live-zone
/// Anthropic compressor. Returns the final transformed body bytes as a Value.
fn transform_full_pipeline(request: &Value) -> Value {
    let mut v = request.clone();
    offload_anthropic_request(&mut v, &cfg());
    let offloaded = serde_json::to_vec(&v).unwrap();
    let outcome = compress_anthropic_request(
        &offloaded.clone().into(),
        CompressionMode::LiveZone,
        CacheControlAutoFrozen::Enabled,
        AuthMode::Payg,
        "test-ctx-stability",
    );
    let final_bytes = match outcome {
        Outcome::Compressed { body, .. } => body.to_vec(),
        _ => offloaded,
    };
    serde_json::from_slice(&final_bytes).unwrap()
}

/// Serialize each message individually so we can compare arrays element-wise
/// as opaque bytes (matching how the cache keys on the byte prefix).
fn message_bytes(request: &Value) -> Vec<Vec<u8>> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| serde_json::to_vec(m).unwrap())
        .collect()
}

/// Assert `prev` is a byte-identical prefix of `next` (message-by-message).
fn assert_prefix(prev: &[Vec<u8>], next: &[Vec<u8>], turn: usize) {
    assert!(
        prev.len() <= next.len(),
        "turn {turn}: history must only grow"
    );
    for (i, (a, b)) in prev.iter().zip(next.iter()).enumerate() {
        assert_eq!(
            a, b,
            "turn {turn}: message[{i}] drifted between consecutive turns \
             — cached prefix would be invalidated"
        );
    }
}

/// Simulate `turns` turns of a growing conversation, each turn appending a new
/// large tool_result turn pair. Returns the per-turn message lists.
fn growing_conversation(turns: usize) -> Vec<Vec<Value>> {
    let mut history: Vec<Value> = vec![json!({"role":"user","content":"start the task"})];
    let mut snapshots = Vec::new();
    for t in 0..turns {
        history.extend(tool_turn(
            &format!("tu_{t}"),
            &format!("run step {t}"),
            &big_log(&format!("step{t}")),
        ));
        snapshots.push(history.clone());
    }
    snapshots
}

/// Apply the FULL context-mode pipeline: CTX-4 injection, then CTX-3 offload,
/// then the live-zone compressor — exactly the request-path order in proxy.rs.
fn transform_all(engine: &InjectEngine, request: &Value, session_key: &str) -> Value {
    let mut v = request.clone();
    engine.maybe_inject(&mut v, session_key);
    offload_anthropic_request(&mut v, &cfg());
    let bytes = serde_json::to_vec(&v).unwrap();
    let outcome = compress_anthropic_request(
        &bytes.clone().into(),
        CompressionMode::LiveZone,
        CacheControlAutoFrozen::Enabled,
        AuthMode::Payg,
        "test-ctx-all",
    );
    let final_bytes = match outcome {
        Outcome::Compressed { body, .. } => body.to_vec(),
        _ => bytes,
    };
    serde_json::from_slice(&final_bytes).unwrap()
}

#[test]
fn inject_offload_compression_all_on_prefix_is_stable() {
    // The full stack: injection + offload + live-zone compression, replayed
    // over a growing conversation. The injected block must appear from turn 1
    // and be byte-stable, and every consecutive prefix must be byte-identical.
    let dir = TempDir::new().unwrap();
    let sessions = Arc::new(SessionsStore::open(dir.path().join("s.db")).unwrap());
    let content = CtxStore::open(dir.path().join("content.db")).unwrap();
    let engine = InjectEngine::new(sessions, content);

    let turns = growing_conversation(6);
    let transformed: Vec<Value> = turns
        .iter()
        .map(|msgs| transform_all(&engine, &body(msgs), "session-A"))
        .collect();

    // Injection present from turn 1.
    let first_user = &transformed[0]["messages"][0];
    let has_inject = serde_json::to_string(first_user)
        .unwrap()
        .contains("ctx:injected");
    assert!(has_inject, "injection must be present from turn 1");

    for n in 0..transformed.len() - 1 {
        let prev = message_bytes(&transformed[n]);
        let next = message_bytes(&transformed[n + 1]);
        assert_prefix(&prev, &next, n);
    }

    // The injected first message is byte-identical across all turns.
    let m0_first = serde_json::to_vec(&transformed[0]["messages"][0]).unwrap();
    for (n, t) in transformed.iter().enumerate() {
        let m0 = serde_json::to_vec(&t["messages"][0]).unwrap();
        assert_eq!(m0, m0_first, "turn {n}: injected first message drifted");
    }
}

#[test]
fn offload_prefix_is_stable_across_six_turns() {
    let turns = growing_conversation(6);
    let transformed: Vec<Value> = turns
        .iter()
        .map(|msgs| transform_offload_only(&body(msgs)))
        .collect();

    for n in 0..transformed.len() - 1 {
        let prev = message_bytes(&transformed[n]);
        let next = message_bytes(&transformed[n + 1]);
        assert_prefix(&prev, &next, n);
    }

    // Sanity: offload actually fired (digests present), else the test is vacuous.
    let last = serde_json::to_string(transformed.last().unwrap()).unwrap();
    assert!(last.contains("<<ctx:"), "expected offload digests in the body");
}

#[test]
fn tool_result_stable_crossing_live_into_frozen_zone_full_pipeline() {
    // With the live-zone compressor also on, the transformed prefix must still
    // be stable turn-to-turn: a tool_result offloaded to a small digest is
    // below the live-zone threshold, so the live-zone compressor no-ops on it
    // and the block that was "live" in turn N stays byte-identical once it is
    // frozen in turn N+1.
    let turns = growing_conversation(6);
    let transformed: Vec<Value> = turns
        .iter()
        .map(|msgs| transform_full_pipeline(&body(msgs)))
        .collect();

    for n in 0..transformed.len() - 1 {
        let prev = message_bytes(&transformed[n]);
        let next = message_bytes(&transformed[n + 1]);
        assert_prefix(&prev, &next, n);
    }
}

#[test]
fn identical_content_offloads_to_identical_digest() {
    // The same tool_result body appearing in two different turns/ids must
    // produce byte-identical replacement content (pure function of bytes).
    let shared = big_log("shared");
    let msgs = vec![
        json!({"role":"user","content":"go"}),
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"a","name":"Bash","input":{"command":"cmd-a"}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"a","content": shared}
        ]}),
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"b","name":"Bash","input":{"command":"cmd-b"}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"b","content": shared}
        ]}),
    ];
    let out = transform_offload_only(&body(&msgs));
    let first = out["messages"][2]["content"][0]["content"].as_str().unwrap();
    let second = out["messages"][4]["content"][0]["content"].as_str().unwrap();
    // The digest content (hash + compressed body) is identical; only the
    // paired command title differs, and that is not part of the wire digest.
    assert_eq!(first, second);
    assert!(first.contains("<<ctx:"));
}

#[test]
fn our_digests_trip_no_volatile_findings() {
    let turns = growing_conversation(4);
    let transformed = transform_offload_only(&body(turns.last().unwrap()));
    let findings = detect_volatile_content(&transformed, ApiKind::Anthropic);
    assert!(
        findings.is_empty(),
        "ctx_offload digests must not look volatile to the cache-bust detector: {findings:?}"
    );
}

#[test]
fn digest_is_deterministic_same_bytes_twice_and_fresh() {
    // Determinism across independent transform runs (I1). A "fresh store" is
    // implicit — the transform never reads any store on the request path.
    let msgs = growing_conversation(3);
    let a = transform_offload_only(&body(msgs.last().unwrap()));
    let b = transform_offload_only(&body(msgs.last().unwrap()));
    assert_eq!(
        serde_json::to_vec(&a).unwrap(),
        serde_json::to_vec(&b).unwrap(),
    );
}
