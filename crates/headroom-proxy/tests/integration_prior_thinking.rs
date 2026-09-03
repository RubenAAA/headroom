//! Prior-turn thinking drop: strips on a boundary, replays on steady turns,
//! and never re-strips an adopted prefix differently from its donor.

mod common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":9000,\"cache_read_input_tokens\":0}}}\n\n",
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

/// A tool loop `assistant_turns` deep, each assistant turn opening with a
/// signed thinking block. The tail is the newest tool_result.
fn history_of(assistant_turns: usize) -> Vec<Value> {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>cwd is /home/dev/alpha</system-reminder>"},
        {"type": "text", "text": "what time is it, check twice"}
    ]})];
    for i in 0..assistant_turns {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": format!("I should call get_time (step {i})."),
             "signature": format!("sig-{i}")},
            {"type": "tool_use", "id": format!("toolu_{i}"), "name": "get_time", "input": {}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": "12:00"}
        ]}));
    }
    messages
}

fn request_with(messages: Vec<Value>) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 256,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "messages": messages
    })
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, session: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat-thinking-tenant")
        .header("x-headroom-session-id", session)
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

fn forwarded_messages(body: &[u8]) -> Vec<Value> {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    let mut out = v["messages"].as_array().cloned().unwrap();
    out.iter_mut().for_each(strip);
    out
}

/// Thinking blocks per assistant message, in order.
fn thinking_per_assistant(messages: &[Value]) -> Vec<usize> {
    messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| {
            m["content"].as_array().map_or(0, |c| {
                c.iter()
                    .filter(|b| b["type"] == "thinking" || b["type"] == "redacted_thinking")
                    .count()
            })
        })
        .collect()
}

fn configure(store: &std::path::Path) -> impl FnOnce(&mut headroom_proxy::config::Config) + '_ {
    move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.ctx_drop_prior_thinking = true;
        c.ctx_store_dir = Some(store.to_path_buf());
    }
}

#[tokio::test]
async fn a_boundary_strips_and_every_steady_turn_replays_the_stripped_prefix() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    let client = reqwest::Client::new();

    // Turn 1 arrives with history the provider has never seen: a boundary.
    post_turn(
        &client,
        &proxy.url(),
        "session-a",
        &request_with(history_of(3)),
    )
    .await;
    // Turn 2 is steady: the client appended one assistant turn and its result.
    post_turn(
        &client,
        &proxy.url(),
        "session-a",
        &request_with(history_of(4)),
    )
    .await;
    // Turn 3 adds two more messages on top.
    post_turn(
        &client,
        &proxy.url(),
        "session-a",
        &request_with(history_of(5)),
    )
    .await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    let turns: Vec<Vec<Value>> = bodies.iter().map(|b| forwarded_messages(b)).collect();

    assert_eq!(
        thinking_per_assistant(&turns[0]),
        vec![0, 0, 1],
        "the boundary turn strips every assistant but the last"
    );
    assert_eq!(
        thinking_per_assistant(&turns[1]),
        vec![0, 0, 1, 1],
        "a steady turn does not run the pass; the replay store carries the stripped bytes"
    );
    assert_eq!(thinking_per_assistant(&turns[2]), vec![0, 0, 1, 1, 1]);

    let n1 = turns[0].len();
    assert_eq!(
        turns[0][..n1],
        turns[1][..n1],
        "turn 2 rewrote a forwarded message"
    );
    let n2 = turns[1].len();
    assert_eq!(
        turns[1][..n2],
        turns[2][..n2],
        "turn 3 rewrote a forwarded message"
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn an_adopted_prefix_is_replayed_from_the_donor_not_re_stripped() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    let client = reqwest::Client::new();

    for turns in 5..=7 {
        post_turn(
            &client,
            &proxy.url(),
            "session-a",
            &request_with(history_of(turns)),
        )
        .await;
    }
    // A second session shows up with the donor's turn-2 history plus its own
    // tail — deep enough to adopt (`CROSS_SESSION_ADOPT_MIN_MESSAGES`). Run
    // raw, the pass would strip assistants 4 and 5, which are no longer last
    // here; the donor's forwarded bytes keep their thinking.
    let mut adopter = history_of(6);
    adopter.push(json!({"role": "assistant", "content": "It is noon."}));
    adopter.push(json!({"role": "user", "content": "thanks"}));
    post_turn(&client, &proxy.url(), "session-b", &request_with(adopter)).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);
    let donor = forwarded_messages(&bodies[2]);
    let adopted = forwarded_messages(&bodies[3]);
    assert_eq!(thinking_per_assistant(&donor), vec![0, 0, 0, 0, 1, 1, 1]);
    assert_eq!(
        thinking_per_assistant(&adopted),
        vec![0, 0, 0, 0, 1, 1, 0],
        "the adopter re-stripped what the donor had forwarded"
    );
    let shared = history_of(6).len();
    assert_eq!(
        adopted[..shared],
        donor[..shared],
        "the adopter's forwarded prefix differs from the donor's"
    );

    proxy.shutdown().await;
}
