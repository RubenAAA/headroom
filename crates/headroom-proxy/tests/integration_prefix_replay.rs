//! Freeze-replay integration — the cross-turn cache-stability guarantee.
//!
//! These tests boot a real Rust proxy in front of a wiremock upstream and
//! drive TWO turns of an Anthropic session through the full buffered
//! compression pipeline:
//!
//! - Turn 1: the latest user message carries a large JSON-array
//!   `tool_result`, so the live-zone dispatcher compresses it
//!   (SmartCrusher). The upstream caches whatever bytes we FORWARDED —
//!   the compressed form.
//! - Turn 2: the conversation grows (append-only), so the big message is
//!   no longer in the live zone and the dispatcher re-emits its ORIGINAL
//!   bytes. Without the freeze-replay overlay this mismatches the cached
//!   prefix and busts the prompt cache from that point (the Python
//!   reference's #1850 bug: `prefix_change` was 100% of observed misses).
//!
//! With `prefix_replay` on, the proxy must replay the turn-1 FORWARDED
//! prefix byte-identical on turn 2. The flag-off control test pins the
//! pre-replay behaviour (original bytes forwarded — the bust) so the
//! replay assertion is provably load-bearing.
//!
//! The response side is exercised for real too: the mock upstream
//! returns a clean Anthropic SSE session (`message_start` …
//! `message_stop` with cache-write usage), which is what commits the
//! turn into the `SessionReplayStore` via the spawned SSE state machine.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::start_proxy_with;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Clean Anthropic SSE session with cache-write usage. `message_stop`
/// is required: the replay store only commits a turn on clean
/// completion (same rationale as the H2 cache-hit-rate gate).
fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_replay\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

/// Mount a `/v1/messages` handler that captures EVERY upstream request
/// body (multi-turn test) and answers with the SSE session above.
async fn mount_anthropic_sse_capture(upstream: &MockServer) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(req.body.clone());
            ResponseTemplate::new(200).set_body_raw(sse_body(), "text/event-stream")
        })
        .mount(upstream)
        .await;
    captured
}

/// The original (client-side) big message: 200 homogeneous dicts in a
/// `tool_result` — SmartCrusher's bread-and-butter (same fixture shape
/// as `headroom-core/tests/live_zone_dispatch.rs`), guaranteed to
/// compress when it sits in the live zone.
fn big_tool_result_message() -> Value {
    let array_of_dicts: Vec<Value> = (0..200)
        .map(|i| {
            json!({
                "id": i,
                "status": "ok",
                "value": format!("repeat-pattern-{}", i % 3),
            })
        })
        .collect();
    let payload = serde_json::to_string(&array_of_dicts).unwrap();
    json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_replay_test",
            "content": payload,
        }],
    })
}

fn turn1_body(big: &Value) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "you are a helpful assistant",
        "messages": [big],
    })
}

/// Turn 2 append-only-extends turn 1. The appended messages use STRING
/// content on purpose: prefix replay makes them eligible breakpoint targets,
/// so this also exercises moving the marker past the previously cached
/// message without changing that message's provider cache key.
fn turn2_body(big: &Value) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "you are a helpful assistant",
        "messages": [
            big,
            {"role": "assistant", "content": "done."},
            {"role": "user", "content": "next step please"},
        ],
    })
}

fn messages_of(body: &[u8]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v.get("messages")
        .and_then(|m| m.as_array())
        .expect("messages array present")
        .clone()
}

/// The provider's prefix key ignores the cache directive itself. Keep every
/// other byte-level distinction, especially string content versus the
/// equivalent one-element text-block form: that is the stability property the
/// bare-string breakpoint path must preserve across turns.
fn without_cache_control(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache_control(value)))
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.iter().map(without_cache_control).collect())
        }
        _ => value.clone(),
    }
}

/// Extract the `tool_result.content` string of a message's first block.
fn tool_result_content(msg: &Value) -> String {
    msg["content"][0]["content"]
        .as_str()
        .expect("tool_result content is a string")
        .to_string()
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-ant-prefix-replay-test")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert_eq!(resp.status(), 200);
    // Drain the SSE body fully so the tee channel closes and the spawned
    // state machine reaches its completion hook (`SessionReplayStore::
    // complete`) promptly.
    let _ = resp.bytes().await.expect("response body");
}

#[tokio::test]
async fn replay_preserves_previous_compressed_provider_prefix() {
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_sse_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();
    let big = big_tool_result_message();
    let original_payload = tool_result_content(&big);

    // ── turn 1: big tool_result is the live zone → compressed ────────
    post_turn(&client, &proxy.url(), &turn1_body(&big)).await;
    let fwd1 = messages_of(&captured.lock().unwrap()[0]);
    let fwd1_payload = tool_result_content(&fwd1[0]);
    assert_ne!(
        fwd1_payload, original_payload,
        "precondition: turn 1 must actually compress the tool_result \
         (fixture no longer triggers SmartCrusher?)"
    );

    // ── turn 2: append-only growth; big message left the live zone ───
    // The dispatcher re-emits the ORIGINAL bytes for it; the replay
    // overlay must substitute the turn-1 FORWARDED (compressed) bytes.
    //
    // The store commits turn 1 from a spawned task after the response
    // body is drained; poll instead of racing it. Re-sending turn 2 is
    // harmless — the overlay is append-only-guarded and idempotent.
    let turn2 = turn2_body(&big);
    let mut fwd2: Option<Vec<Value>> = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        post_turn(&client, &proxy.url(), &turn2).await;
        let got = messages_of(captured.lock().unwrap().last().unwrap());
        if without_cache_control(&got[0]) == without_cache_control(&fwd1[0]) {
            fwd2 = Some(got);
            break;
        }
    }
    let fwd2 = fwd2.expect(
        "turn 2 never replayed the previously-forwarded prefix: message 0 \
         should preserve the provider key from turn 1 (compressed), not the \
         original client bytes",
    );

    // The replayed prefix is the turn-1 forwarded provider key — including the
    // compressed payload, NOT the client's original. The cache directive may
    // move to a newer message and is not itself part of that key.
    assert_eq!(
        without_cache_control(&fwd2[0]),
        without_cache_control(&fwd1[0]),
        "frozen prefix must preserve its provider cache key"
    );
    assert_ne!(
        tool_result_content(&fwd2[0]),
        original_payload,
        "turn 2 forwarded the ORIGINAL big payload — the replay overlay \
         did not run and the prompt cache would bust (prefix_change)"
    );
    // The appended suffix is this turn's fresh content. Every eligible string
    // is wrapped to block form so its shape cannot change when the marker
    // moves on; the available tail slot lands on the newest one.
    assert_eq!(fwd2.len(), 3);
    assert_eq!(fwd2[1]["content"], json!([{"type": "text", "text": "done."}]));
    assert_eq!(
        fwd2[2]["content"],
        json!([{
            "type": "text",
            "text": "next step please",
            "cache_control": {"type": "ephemeral"},
        }])
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn without_flag_turn2_forwards_original_bytes_the_bust_this_feature_fixes() {
    // Control: flag OFF pins the pre-replay behaviour. Turn 2 forwards
    // the ORIGINAL big message (a guaranteed prefix_change bust) because
    // the live-zone dispatcher only rewrites the latest user message.
    // If this test ever starts failing, the dispatcher began re-emitting
    // compressed history on its own and the replay test above must be
    // re-examined rather than trusted blindly.
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_sse_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = false;
    })
    .await;
    let client = reqwest::Client::new();
    let big = big_tool_result_message();
    let original_payload = tool_result_content(&big);

    post_turn(&client, &proxy.url(), &turn1_body(&big)).await;
    let fwd1 = messages_of(&captured.lock().unwrap()[0]);
    assert_ne!(
        tool_result_content(&fwd1[0]),
        original_payload,
        "precondition: turn 1 must compress"
    );

    // Give the (absent) response-side hook the same grace the replay
    // test gets, so the control is apples-to-apples.
    tokio::time::sleep(Duration::from_millis(200)).await;
    post_turn(&client, &proxy.url(), &turn2_body(&big)).await;
    let fwd2 = messages_of(captured.lock().unwrap().last().unwrap());
    assert_eq!(
        tool_result_content(&fwd2[0]),
        original_payload,
        "flag off: turn 2 must forward the original bytes (the bust this \
         feature exists to prevent)"
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn first_turn_cold_start_only_normalizes_cache_control() {
    // Cold start (no previous turn): the overlay has nothing to replay.
    // The replay stage still owns message-level cache_control placement
    // (#1852): exactly one ephemeral breakpoint on the last block-style
    // message, so replayed markers can never accumulate past Anthropic's
    // 4-marker limit on later turns.
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_sse_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();
    let big = big_tool_result_message();

    post_turn(&client, &proxy.url(), &turn1_body(&big)).await;
    let fwd1 = messages_of(&captured.lock().unwrap()[0]);
    let marker_count: usize = fwd1
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b.get("cache_control").is_some())
        .count();
    assert_eq!(
        marker_count, 1,
        "replay stage must place exactly one message-level cache_control breakpoint"
    );
    let last_block = fwd1[0]["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        last_block["cache_control"],
        json!({"type": "ephemeral"}),
        "breakpoint sits on the last block of the last block-style message"
    );

    proxy.shutdown().await;
}
