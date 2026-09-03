//! The opening scaffolding is one cached prefix, shared by every session of a
//! project.
//!
//! Claude Code starts every session with the same `<system-reminder>` block —
//! the CLAUDE.md digest, tens of kilobytes of it — and it is identical across
//! sessions of the same working directory. Recall injection used to go in front
//! of it, which put session-specific bytes at the head of the prefix and made
//! every fresh session write those kilobytes again. Measured live: ~163 fresh
//! sessions a day, ~11k tokens each, about a tenth of the day's cache writes.
//!
//! This drives the first turn of two different sessions through a real proxy
//! and asserts that everything up to and including the breakpointed scaffolding
//! block goes on the wire byte-identical, so the second session reads what the
//! first one wrote.

mod common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_scaffold\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
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

/// The project-wide scaffolding, big enough that paying for it twice is the
/// thing worth a test.
fn claude_md_reminder() -> String {
    format!(
        "<system-reminder>\nAs you answer the user's questions, you can use \
         the following context:\n# claudeMd\n{}\n</system-reminder>",
        "project instructions. ".repeat(400)
    )
}

/// One session's first turn. Identical `system`, `tools` and scaffolding for
/// every session of the project; only the typed text differs.
fn first_turn(typed: &str) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        // Claude Code's own shape: two `system` blocks, both marked, both asking
        // for the 1h tier. The second is what pays for the scaffolding marker.
        "system": [
            {"type": "text", "text": "you are a helpful assistant",
             "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "text", "text": "answer concisely",
             "cache_control": {"type": "ephemeral", "ttl": "1h"}}
        ],
        "tools": [{
            "name": "read_file",
            "description": "read a file",
            "input_schema": {"type": "object", "properties": {}}
        }],
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": claude_md_reminder()},
                {"type": "text", "text": "<system-reminder>cwd is /home/dev/alpha</system-reminder>"},
                {"type": "text", "text": typed}
            ]
        }]
    })
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        // OAuth, not PAYG: the 1h TTL pin is skipped on pay-as-you-go.
        .header("authorization", "Bearer sk-ant-oat-shared-scaffolding")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert!(
        resp.status().is_success(),
        "proxy returned {}",
        resp.status()
    );
    let _ = resp.bytes().await;
}

fn message_zero(body: &[u8]) -> Value {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["messages"][0].clone()
}

/// Every `cache_control` the request carries, across `system`, `tools` and
/// `messages` — the sum Anthropic refuses past four.
fn marker_count(body: &[u8]) -> usize {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    let mut n = 0;
    for field in ["system", "tools"] {
        if let Some(items) = v.get(field).and_then(Value::as_array) {
            n += items
                .iter()
                .filter(|i| i.get("cache_control").is_some())
                .count();
        }
    }
    if let Some(messages) = v.get("messages").and_then(Value::as_array) {
        for m in messages {
            n += usize::from(m.get("cache_control").is_some());
            if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                n += blocks
                    .iter()
                    .filter(|b| b.get("cache_control").is_some())
                    .count();
            }
        }
    }
    n
}

/// The block index carrying the breakpoint, and the bytes of everything up to
/// and including it — the region the provider caches.
fn cached_region(msg: &Value) -> (usize, String) {
    let blocks = msg["content"]
        .as_array()
        .expect("message 0 is block content");
    let at = blocks
        .iter()
        .position(|b| b.get("cache_control").is_some())
        .expect("message 0 carries the scaffolding breakpoint");
    (at, serde_json::to_string(&blocks[..=at]).unwrap())
}

/// Where every `cache_control` sits on `messages`, as `(message, block)`.
fn message_markers(body: &[u8]) -> Vec<(usize, usize)> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    let mut out = Vec::new();
    for (i, m) in v["messages"].as_array().unwrap().iter().enumerate() {
        if let Some(blocks) = m.get("content").and_then(Value::as_array) {
            for (j, b) in blocks.iter().enumerate() {
                if b.get("cache_control").is_some() {
                    out.push((i, j));
                }
            }
        }
    }
    out
}

/// One turn of a growing session. `tail` is what the newest user message says,
/// so a turn can be re-sent with its tail edited.
fn nth_turn(typed: &str, replies: usize, tail: &str) -> Value {
    let mut body = first_turn(typed);
    let messages = body["messages"].as_array_mut().unwrap();
    for i in 0..replies {
        messages.push(json!({"role":"assistant","content":format!("reply {i}")}));
        messages.push(json!({"role":"user","content":[
            {"type":"text","text": if i + 1 == replies { tail.to_string() } else { format!("follow up {i}") }}
        ]}));
    }
    body
}

#[tokio::test]
async fn two_sessions_share_the_opening_scaffolding_prefix() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();

    // The live shape: recall injection on, replay on, two message breakpoints,
    // 1h TTL forced.
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.cache_tail_breakpoints = 2;
        c.force_1h_cache_ttl = true;
        c.ctx_capture = true;
        c.ctx_inject = true;
        c.ctx_store_dir = Some(store.path().to_path_buf());
    })
    .await;

    let client = reqwest::Client::new();
    post_turn(&client, &proxy.url(), &first_turn("build a parser")).await;
    post_turn(
        &client,
        &proxy.url(),
        &first_turn("write the release notes"),
    )
    .await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "both turns reached the upstream");

    let m0_a = message_zero(&bodies[0]);
    let m0_b = message_zero(&bodies[1]);
    let (at_a, region_a) = cached_region(&m0_a);
    let (at_b, region_b) = cached_region(&m0_b);

    assert_eq!(
        at_a, at_b,
        "the breakpoint must land on the same block in both sessions"
    );
    assert_eq!(
        region_a, region_b,
        "the cached region of message 0 must be byte-identical across sessions"
    );

    // And it must be the scaffolding it names, not an accident of both sessions
    // being short.
    let head = m0_a["content"][0]["text"].as_str().unwrap();
    assert!(head.starts_with("<system-reminder>"), "head: {head:.40}");
    assert!(
        m0_a["content"][at_a]["text"]
            .as_str()
            .unwrap()
            .starts_with("<system-reminder>"),
        "the breakpoint sits on the last scaffolding block"
    );

    // The recall is session-specific and sits behind the shared region, so the
    // two messages must diverge after the breakpoint.
    assert_ne!(
        serde_json::to_string(&m0_a).unwrap(),
        serde_json::to_string(&m0_b).unwrap(),
        "precondition: the sessions must differ somewhere, or this proves nothing"
    );

    // The breakpoint the shared prefix ends on asks for the 1h tier, which is
    // what makes it worth a later session's while to come looking for it.
    assert_eq!(
        m0_a["content"][at_a]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    for (i, body) in bodies.iter().enumerate() {
        let n = marker_count(body);
        assert!(
            n <= 4,
            "session {i} sent {n} cache_control markers, limit is 4"
        );
        // Turn one has only message 0, so the tail can place just one marker.
        // That plus the scaffolding one, alongside the client's two on `system`,
        // is the full four. The yield is exercised in the three-turn test below,
        // where the tail really has two messages to sit on.
        assert_eq!(
            message_markers(body).len(),
            2,
            "session {i}: the scaffolding marker and the tail one"
        );
    }

    proxy.shutdown().await;
}

/// Three turns of one session, the last of them re-sent with its tail edited.
///
/// The tail pair is what makes that survivable: Anthropic writes a cache entry
/// only where a breakpoint says to, so the older of the two tail markers is what
/// an edited turn reads back from. The scaffolding breakpoint must not have cost
/// either of them — the second `system` marker pays for it instead.
#[tokio::test]
async fn an_edited_tail_keeps_both_tail_breakpoints_within_budget() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.cache_tail_breakpoints = 2;
        c.force_1h_cache_ttl = true;
        c.ctx_capture = true;
        c.ctx_inject = true;
        c.ctx_store_dir = Some(store.path().to_path_buf());
    })
    .await;

    let client = reqwest::Client::new();
    post_turn(&client, &proxy.url(), &nth_turn("build a parser", 0, "")).await;
    post_turn(
        &client,
        &proxy.url(),
        &nth_turn("build a parser", 1, "keep going"),
    )
    .await;
    // Turn 3 edits the tail the client had already sent, which is the case the
    // second tail breakpoint exists for.
    post_turn(
        &client,
        &proxy.url(),
        &nth_turn("build a parser", 2, "actually, do it the other way"),
    )
    .await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);

    for (turn, body) in bodies.iter().enumerate() {
        let markers = message_markers(body);
        let total = marker_count(body);
        assert!(
            total <= 4,
            "turn {turn} sent {total} cache_control markers, limit is 4: {markers:?}"
        );
        let past_message_zero = markers.iter().filter(|(m, _)| *m > 0).count();
        // Turn 1 has only message 0, so both tail markers land on it there.
        let tail_markers = if turn == 0 {
            markers.iter().filter(|(_, b)| *b > 0).count()
        } else {
            past_message_zero
        };
        assert!(
            tail_markers >= 2,
            "turn {turn} kept only {tail_markers} tail breakpoints: {markers:?}"
        );

        // Past turn one the tail pair plus the scaffolding marker are three, so
        // `system` has to give one up to stay inside four. Exactly one: going to
        // zero would leave `system` and `tools` with no checkpoint of their own
        // ahead of message 0.
        if turn > 0 {
            let v: Value = serde_json::from_slice(body).unwrap();
            let system = v["system"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b.get("cache_control").is_some())
                .count();
            assert_eq!(
                system, 1,
                "turn {turn} should have yielded one system marker"
            );
            assert_eq!(
                markers.len(),
                3,
                "turn {turn}: scaffolding plus the tail pair"
            );
        }
    }

    // Message 0's text never moves, on any turn.
    let stripped: Vec<String> = bodies
        .iter()
        .map(|b| {
            let mut m = message_zero(b);
            for block in m["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
            serde_json::to_string(&m).unwrap()
        })
        .collect();
    assert_eq!(
        stripped[0], stripped[1],
        "message 0 text drifted, turns 1 to 2"
    );
    assert_eq!(
        stripped[1], stripped[2],
        "message 0 text drifted, turns 2 to 3"
    );

    // The scaffolding breakpoint holds its block and its exact JSON, key order
    // included — a key that reorders is a new provider cache key even though
    // every character of the text still matches.
    let scaffold: Vec<(usize, String)> = bodies
        .iter()
        .map(|b| {
            let m = message_zero(b);
            let blocks = m["content"].as_array().unwrap().clone();
            let at = blocks
                .iter()
                .position(|blk| blk.get("cache_control").is_some())
                .expect("message 0 keeps its scaffolding breakpoint");
            (
                at,
                serde_json::to_string(&blocks[at]["cache_control"]).unwrap(),
            )
        })
        .collect();
    assert_eq!(scaffold[0], scaffold[1]);
    assert_eq!(scaffold[1], scaffold[2]);
    assert_eq!(scaffold[0].1, r#"{"type":"ephemeral","ttl":"1h"}"#);

    // From turn 2 the whole of message 0 is settled: the tail marker has moved
    // off it and nothing else on it may move again.
    assert_eq!(
        serde_json::to_string(&message_zero(&bodies[1])).unwrap(),
        serde_json::to_string(&message_zero(&bodies[2])).unwrap(),
        "message 0 drifted between turns 2 and 3"
    );

    proxy.shutdown().await;
}
