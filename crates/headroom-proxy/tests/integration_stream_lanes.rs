//! Same-opener subagent streams: per-lane prefix stability.
//!
//! The incident (2026-09-08, personal auth): one credential ran several Sonnet
//! subagent streams with a byte-identical opener on one model. The session key
//! folds only `(auth, model, first message)`, so every stream shared ONE drift
//! baseline, ONE replay tracker, and ONE observer conversation. Each system
//! flip (`line[0]:70<->91`, seconds apart) invalidated the shared tracker
//! (`no_previous_turn` 108 times in 13 minutes), replay never engaged, and
//! every turn re-emitted un-replayed bytes past the provider's stable entry
//! (`actual_cache_read` pinned at exactly 31,300 while expected grew 63k->92k).
//!
//! These tests drive TWO such streams (systems A/B, distinct histories past
//! the shared opener) interleaved X1,Y1,X2,Y2 through the full proxy and
//! assert each lane's forwarded prefix stays stable:
//!
//! - X2 must replay X1's FORWARDED (compressed) prefix, not re-emit the
//!   client's original bytes. Today Y1's flip wipes the shared tracker, so
//!   X2 finds nothing and the provider key busts.
//! - Y1 must carry its own bytes (no cross-stream splice), and first sights
//!   must strip thinking so a lineage is born stripped and stays stripped.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::start_proxy_with;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_lanes\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

async fn mount_capture(upstream: &MockServer) -> Arc<Mutex<Vec<Vec<u8>>>> {
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

/// 200 homogeneous dicts — SmartCrusher's bread-and-butter. `tag` keeps the
/// two streams' payloads distinct so a cross-stream splice is detectable.
fn big_tool_result(tag: &str) -> Value {
    let array_of_dicts: Vec<Value> = (0..200)
        .map(|i| {
            json!({
                "id": i,
                "status": "ok",
                "value": format!("{tag}-pattern-{}", i % 3),
            })
        })
        .collect();
    json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": format!("toolu_{tag}"),
            "content": serde_json::to_string(&array_of_dicts).unwrap(),
        }],
    })
}

/// Byte-identical opener across both streams: this is what merges them onto
/// one session key today.
fn opener() -> Value {
    json!({"role": "user", "content": "Investigate the auth module failure"})
}

fn thinking_assistant(tag: &str) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "thinking", "thinking": format!("{tag}: plan the investigation"),
         "signature": format!("sig-{tag}")},
        {"type": "text", "text": format!("{tag}: starting now")},
    ]})
}

/// System prompts differing the way the incident's did: identical but for
/// the first line (70 vs 91 chars there; lengths differ here too).
fn system_a() -> Value {
    json!("You are a focused subagent.\nTask: investigate the auth failure.")
}

fn system_b() -> Value {
    json!("You are a focused subagent with full repository context loaded.\nTask: investigate the auth failure.")
}

fn stream_x_turn1() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": system_a(),
        "messages": [
            opener(),
            thinking_assistant("stream-x-1"),
            {"role": "user", "content": "go on"},
            thinking_assistant("stream-x-2"),
            big_tool_result("x"),
        ],
    })
}

fn stream_y_turn1() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": system_b(),
        "messages": [
            opener(),
            thinking_assistant("stream-y-1"),
            {"role": "user", "content": "go on"},
            thinking_assistant("stream-y-2"),
            big_tool_result("y"),
        ],
    })
}

/// Append-only growth on X's own history: three more pairs on top, so the
/// big tool_result from turn 1 has left the live zone. Without replay the
/// dispatcher re-emits its ORIGINAL bytes and the provider key busts.
fn stream_x_turn2() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": system_a(),
        "messages": [
            opener(),
            thinking_assistant("stream-x-1"),
            {"role": "user", "content": "go on"},
            thinking_assistant("stream-x-2"),
            big_tool_result("x"),
            {"role": "assistant", "content": "found the suspect line"},
            {"role": "user", "content": "confirm it"},
            {"role": "assistant", "content": "confirmed"},
            {"role": "user", "content": "fix it"},
            {"role": "assistant", "content": "fixed"},
            {"role": "user", "content": "verify"},
        ],
    })
}

fn stream_y_turn2() -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": system_b(),
        "messages": [
            opener(),
            thinking_assistant("stream-y-1"),
            {"role": "user", "content": "go on"},
            thinking_assistant("stream-y-2"),
            big_tool_result("y"),
            {"role": "assistant", "content": "checking the other file"},
            {"role": "user", "content": "confirm it"},
            {"role": "assistant", "content": "confirmed"},
            {"role": "user", "content": "fix it"},
            {"role": "assistant", "content": "fixed"},
            {"role": "user", "content": "verify"},
        ],
    })
}

fn forwarded(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("upstream body is JSON")
}

/// The provider's prefix key ignores the cache directive itself.
fn without_cache_control(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache_control(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_cache_control).collect()),
        _ => value.clone(),
    }
}

/// Thinking blocks per assistant message, in order.
fn thinking_per_assistant(messages: &[Value]) -> Vec<usize> {
    messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| {
            m["content"].as_array().map_or(0, |c| {
                c.iter()
                    .filter(|b| {
                        b.get("type")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| t == "thinking" || t == "redacted_thinking")
                    })
                    .count()
            })
        })
        .collect()
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        // ONE credential for both streams: the subagent collision.
        .header("x-api-key", "sk-ant-stream-lanes-test")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await.expect("response body");
    // The replay store commits from a spawned task after the body drains;
    // the next turn must see the committed prefix, not race it.
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Interleaved same-opener streams must each keep a stable forwarded prefix:
/// X2 replays X1's forwarded bytes even though Y1 ran (and flipped the
/// system) in between.
#[tokio::test]
async fn interleaved_streams_keep_per_lane_forwarded_prefix() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
    })
    .await;
    let client = reqwest::Client::new();

    post_turn(&client, &proxy.url(), &stream_x_turn1()).await;
    post_turn(&client, &proxy.url(), &stream_y_turn1()).await;
    post_turn(&client, &proxy.url(), &stream_x_turn2()).await;
    post_turn(&client, &proxy.url(), &stream_y_turn2()).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4, "one upstream body per turn");
    let fwd: Vec<Value> = bodies.iter().map(|b| forwarded(b)).collect();
    let msgs = |i: usize| {
        without_cache_control(&fwd[i].get("messages").expect("messages present").clone())
    };

    // First sights strip every assistant but the last: the lineage is born
    // stripped (a first sight with history is a boundary: the provider has
    // never seen these bytes, so the strip is free).
    for (i, label) in [(0, "X1"), (1, "Y1")] {
        let m = fwd[i]
            .get("messages")
            .and_then(|m| m.as_array())
            .expect("messages array");
        assert_eq!(
            thinking_per_assistant(m),
            vec![0, 1],
            "{label} did not strip-then-keep thinking: the lineage is not born stripped"
        );
    }

    // Y1 carries its own history: no cross-stream splice from X1. The big
    // payload sits at index 4 in the five-message turn-1 histories.
    let y1 = msgs(1);
    let y1_arr = y1.as_array().expect("messages array");
    assert_eq!(y1_arr.len(), 5);
    assert!(
        serde_json::to_string(&y1_arr[4])
            .unwrap()
            .contains("y-pattern-"),
        "Y1's big payload is not Y's own bytes: X's prefix was spliced across streams"
    );

    // X2 replays X1's FORWARDED prefix byte-identical (modulo the cache
    // directive): the big tool_result stays in its turn-1 compressed form
    // even though Y1 flipped the shared session in between.
    let x1 = msgs(0);
    let x2 = msgs(2);
    let (x1_arr, x2_arr) = (
        x1.as_array().expect("messages array"),
        x2.as_array().expect("messages array"),
    );
    assert_eq!(x1_arr.len(), 5, "X1 has five messages");
    assert_eq!(x2_arr.len(), 11, "X2 appends six messages to X1's five");
    assert_eq!(
        x2_arr[..5],
        x1_arr[..],
        "X2 did not replay X1's forwarded prefix: same-lane replay died on Y1's flip"
    );
    assert_eq!(
        without_cache_control(&fwd[2].get("system").expect("system").clone()),
        without_cache_control(&fwd[0].get("system").expect("system").clone()),
        "X2's system differs from X1's: same-lane system must hold still"
    );

    // Same for Y2 over Y1.
    let y2 = msgs(3);
    let y2_arr = y2.as_array().expect("messages array");
    assert_eq!(y2_arr.len(), 11);
    assert_eq!(
        y2_arr[..5],
        y1_arr[..],
        "Y2 did not replay Y1's forwarded prefix"
    );

    proxy.shutdown().await;
}

/// A `cd` mid-conversation mints a new lane (the lane folds the system
/// digest in) but continues the same message lineage. The fresh lane must
/// inherit the lineage's hold pins and adopt its replay baseline, so the
/// turn forwards the pinned directory and replays the stored prefix — the
/// zero-cost turn the holds exist to produce — instead of latching the
/// live directory and re-caching the whole preamble.
#[tokio::test]
async fn a_cd_turn_replays_across_the_lane_switch() {
    fn system(dir: &str) -> Value {
        json!(format!(
            "You are a focused subagent.\n - Primary working directory: {dir}\n"
        ))
    }
    fn history(extra: &[(&str, &str)]) -> Value {
        let mut messages = vec![opener()];
        for i in 0..9 {
            let (role, text) = if i % 2 == 0 {
                ("assistant", format!("note {i}"))
            } else {
                ("user", format!("go on {i}"))
            };
            messages.push(json!({"role": role, "content": text}));
        }
        for (role, text) in extra {
            messages.push(json!({"role": role, "content": *text}));
        }
        json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64,
            "messages": messages,
        })
    }

    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.hold_working_directory = true;
    })
    .await;
    let client = reqwest::Client::new();

    // Turn 1 in /repo: latches the lane-A pin and stores the prefix. Its
    // message 4 carries a `<system-reminder>` span the cd turn withdraws:
    // canonicalization masks it for agreement, so replay still applies —
    // but the replayed bytes keep the donor's copy, which a fresh forward
    // would lack. That difference is what proves replay ran, rather than a
    // byte-identical fresh compression.
    let mut turn1 = history(&[]);
    turn1["system"] = system("/repo");
    turn1["messages"][4]["content"] =
        json!("go on 3\n<system-reminder>remember the api</system-reminder>");
    post_turn(&client, &proxy.url(), &turn1).await;

    // Turn 2 after `cd /repo/sub`: same lineage, new lane. The last
    // message carries block content so the hold's tail note — which states
    // the live directory for the model — has a block to attach to.
    let mut turn2 = history(&[("assistant", "noted")]);
    turn2["system"] = system("/repo/sub");
    turn2["messages"]
        .as_array_mut()
        .expect("messages array")
        .push(json!({"role": "user", "content": [{"type": "text", "text": "continue"}]}));
    post_turn(&client, &proxy.url(), &turn2).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one upstream body per turn");
    let fwd: Vec<Value> = bodies.iter().map(|b| forwarded(b)).collect();

    // The hold travelled with the lineage: the live directory was replaced
    // by the pinned one, plus the tail note naming where the client is.
    let sys1 = without_cache_control(&fwd[0].get("system").expect("system").clone());
    let sys2 = without_cache_control(&fwd[1].get("system").expect("system").clone());
    assert_eq!(
        sys2, sys1,
        "the cd turn forwarded the live directory: the inherited pin did not travel"
    );

    // And the stored prefix replayed under it: the first ten forwarded
    // messages are turn 1's bytes, not a fresh compression.
    let m1 = msgs_of(&fwd[0]);
    let m2 = msgs_of(&fwd[1]);
    assert_eq!(m1.len(), 10, "turn 1 has ten messages");
    assert_eq!(m2.len(), 12, "turn 2 appends two messages");
    // Premise for the comparison below: turn 1 forwarded the reminder
    // in place (compression preserves it), so only a replay carries it
    // into turn 2 — a fresh forward would lack it and fail the equality.
    assert!(
        serde_json::to_string(&m1[4])
            .unwrap()
            .contains("system-reminder"),
        "turn 1 must forward the reminder in place for this test to mean anything"
    );
    assert_eq!(
        m2[..10],
        m1[..],
        "the cd turn did not replay across the lane switch: full preamble rewrite"
    );
    // The live directory still reaches the model, in the hold's tail note on
    // the last user message — new tail, outside the replayed prefix.
    let tail_text = m2[11]["content"]
        .as_array()
        .expect("block content")
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tail_text.contains("/repo/sub"),
        "the tail note must name the live directory: {tail_text}"
    );

    proxy.shutdown().await;
}

fn msgs_of(fwd: &Value) -> Vec<Value> {
    without_cache_control(&fwd.get("messages").expect("messages").clone())
        .as_array()
        .expect("messages array")
        .clone()
}

/// A `cd` under ctx recall injection: the recall block rides every
/// forwarded turn, but lineage (discovery, replay, stored histories) is
/// computed on client bytes throughout — injection mutates only downstream
/// copies. So the fresh lane still inherits its pin and adopts.
///
/// The block assertion is the premise, not decoration: if the engine ever
/// stops firing here, this test must fail loudly rather than pass
/// vacuously on an injection-free replay.
#[tokio::test]
async fn a_cd_turn_replays_across_the_lane_switch_under_injection() {
    fn system(dir: &str) -> Value {
        json!(format!(
            "You are a focused subagent.\n - Primary working directory: {dir}\n"
        ))
    }
    fn history(n_notes: usize) -> Value {
        let mut messages = vec![opener()];
        for i in 0..n_notes {
            let (role, text) = if i % 2 == 0 {
                ("assistant", format!("note {i}"))
            } else {
                ("user", format!("go on {i}"))
            };
            messages.push(json!({"role": role, "content": text}));
        }
        json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64,
            "messages": messages,
        })
    }

    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.hold_working_directory = true;
        c.ctx_capture = true;
        c.ctx_inject = true;
        c.ctx_store_dir = Some(store.path().to_path_buf());
    })
    .await;
    let client = reqwest::Client::new();

    let mut t1 = history(5);
    t1["system"] = system("/repo");
    t1["messages"][4]["content"] =
        json!("go on 3\n<system-reminder>remember the api</system-reminder>");
    post_turn(&client, &proxy.url(), &t1).await;
    let mut t2 = history(9);
    t2["system"] = system("/repo");
    t2["messages"][4]["content"] =
        json!("go on 3\n<system-reminder>remember the api</system-reminder>");
    post_turn(&client, &proxy.url(), &t2).await;
    let mut t3 = history(9);
    t3["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "assistant", "content": "noted"}));
    t3["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "user", "content": [{"type": "text", "text": "continue"}]}));
    t3["system"] = system("/repo/sub");
    post_turn(&client, &proxy.url(), &t3).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    let fwd: Vec<Value> = bodies.iter().map(|b| forwarded(b)).collect();

    // Premise: recall fired on every turn, identically. Without the block
    // in play this test would prove nothing about injection.
    for (i, f) in fwd.iter().enumerate() {
        let m0 = serde_json::to_string(&f["messages"][0]).unwrap();
        assert!(
            m0.contains("<!--ctx:injected-->"),
            "turn {i} forwarded without its recall block: the premise is gone"
        );
    }
    let m0_first = serde_json::to_string(&fwd[0]["messages"][0]).unwrap();
    assert!(
        [1, 2]
            .iter()
            .all(|i| serde_json::to_string(&fwd[*i]["messages"][0]).unwrap() == m0_first),
        "the recall block must replay verbatim once decided"
    );

    // The cd turn (index 2) holds the pinned directory despite the live one
    // having moved, on a fresh lane minted for the new system.
    let sys: Vec<Value> = fwd
        .iter()
        .map(|f| without_cache_control(&f.get("system").expect("system").clone()))
        .collect();
    assert_eq!(
        sys[2], sys[1],
        "the cd turn forwarded the live directory under injection"
    );
    assert!(
        sys[2].to_string().contains("/repo"),
        "the pinned directory must survive: {sys2}",
        sys2 = sys[2]
    );

    // And it replays the stored prefix under that system: the first ten
    // forwarded messages are turn 2's bytes, not a fresh compression.
    let m: Vec<Vec<Value>> = fwd
        .iter()
        .map(|f| {
            without_cache_control(&f.get("messages").expect("messages").clone())
                .as_array()
                .expect("messages array")
                .clone()
        })
        .collect();
    assert_eq!(m[1].len(), 10, "turn 2 has ten messages");
    assert_eq!(m[2].len(), 12, "the cd turn appends two messages");
    // Same premise as the plain cd test: turn 2 forwarded the reminder in
    // place, so only a replay carries it into the cd turn.
    assert!(
        serde_json::to_string(&m[1][4])
            .unwrap()
            .contains("system-reminder"),
        "turn 2 must forward the reminder in place for this test to mean anything"
    );
    assert_eq!(
        m[2][..10],
        m[1][..],
        "the cd turn did not replay across the lane switch under injection"
    );

    proxy.shutdown().await;
}
