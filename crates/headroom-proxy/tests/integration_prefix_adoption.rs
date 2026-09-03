//! Cross-session prefix adoption.
//!
//! A conversation can arrive under a session key the proxy has never seen: a
//! fork subagent, a resumed transcript, or, measured on 2026-09-02 as the
//! common case, the same conversation after its key changed because the
//! `<system-reminder>` in message 0 was edited or the OAuth token rotated.
//! Rebuilding it forwards different bytes from the ones the original key
//! forwarded, so the provider re-caches the whole conversation. Adoption
//! replays the original key's bytes instead.

mod common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use headroom_core::transforms::live_zone::CTX_OFFLOAD_MARKER_PREFIX;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_adopt\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":9000,\"cache_read_input_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
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

/// The messages of a session's `tool_turns`-th tool turn: an opening exchange,
/// then, per turn, one completed tool call with a result far above the offload
/// floor and the exchange that followed it. A result therefore arrives two
/// messages back from the tail, where a first conversion waits for a rebuild
/// boundary: the donor forwards it raw, and a rebuild would digest it.
fn tool_history(tool_turns: usize, reminder: &str) -> Vec<Value> {
    let mut messages = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": format!("<system-reminder>{reminder}</system-reminder>")},
            {"type": "text", "text": "build a parser"}
        ]}),
        json!({"role": "assistant", "content": "I will start by reading the sources."}),
        json!({"role": "user", "content": [{"type": "text", "text": "go ahead"}]}),
    ];
    for i in 0..tool_turns {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": format!("toolu_{i}"), "name": "read_file",
             "input": {"path": format!("/src/{i}.rs")}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{i}"),
             "content": format!("line {i}\n").repeat(600)}
        ]}));
        messages.push(json!({"role": "assistant", "content": format!("noted file {i}")}));
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": "continue"}]}));
    }
    messages
}

fn request_with(messages: Vec<Value>) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": [{"type": "text", "text": "you are a helpful assistant",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
        "tools": [{"name": "read_file", "description": "read a file",
                   "input_schema": {"type": "object", "properties": {}}}],
        "messages": messages
    })
}

/// One client as the proxy's session key sees it: the bearer token, an
/// optional explicit session id, and the reminder text opening message 0.
#[derive(Clone, Copy)]
struct Peer {
    auth: &'static str,
    session: Option<&'static str>,
    reminder: &'static str,
}

const REMINDER: &str = "cwd is /home/dev/alpha";

const DONOR: Peer = Peer {
    auth: "Bearer sk-ant-oat-shared-tenant",
    session: Some("session-a"),
    reminder: REMINDER,
};

const ADOPTER: Peer = Peer {
    auth: "Bearer sk-ant-oat-shared-tenant",
    session: Some("session-b"),
    reminder: REMINDER,
};

async fn post_turn(client: &reqwest::Client, proxy_url: &str, peer: Peer, body: &Value) {
    let mut request = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", peer.auth);
    if let Some(session) = peer.session {
        request = request.header("x-headroom-session-id", session);
    }
    let resp = request
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

/// The forwarded `messages` with every `cache_control` removed. Breakpoints
/// move with the tail; the bytes under them are what the provider caches.
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
    let mut messages = v["messages"].as_array().cloned().unwrap();
    messages.iter_mut().for_each(strip);
    messages
}

fn offloaded_tool_results(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .filter(|b| {
            let text = match &b["content"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<String>(),
                _ => String::new(),
            };
            text.contains(CTX_OFFLOAD_MARKER_PREFIX)
        })
        .count()
}

fn assert_prefix_of(shorter: &[Value], longer: &[Value], what: &str) {
    assert!(
        longer.len() >= shorter.len(),
        "{what}: {} forwarded messages cannot extend {}",
        longer.len(),
        shorter.len()
    );
    for (i, (a, b)) in shorter.iter().zip(longer).enumerate() {
        assert_eq!(
            serde_json::to_string(a).unwrap(),
            serde_json::to_string(b).unwrap(),
            "{what}: forwarded message {i} differs"
        );
    }
}

/// The donor's six tool turns.
async fn run_donor_turns(client: &reqwest::Client, proxy_url: &str, peer: Peer) {
    for turn in 1..=6 {
        let body = request_with(tool_history(turn, peer.reminder));
        post_turn(client, proxy_url, peer, &body).await;
    }
}

/// The adopter's three turns: the donor's turn-4 history plus its own tail,
/// then two more exchanges.
async fn run_adopter_turns(client: &reqwest::Client, proxy_url: &str, peer: Peer) {
    let mut messages = tool_history(4, peer.reminder);
    messages.push(json!({"role": "assistant", "content": "done reading"}));
    messages.push(json!({"role": "user", "content": [{"type": "text", "text": "now write it"}]}));
    post_turn(client, proxy_url, peer, &request_with(messages.clone())).await;
    for i in 0..2 {
        messages.push(json!({"role": "assistant", "content": format!("reply {i}")}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("follow up {i}")}
        ]}));
        post_turn(client, proxy_url, peer, &request_with(messages.clone())).await;
    }
}

fn assert_adopted(bodies: &[Vec<u8>]) {
    assert_eq!(bodies.len(), 9, "six donor turns and three adopter turns");
    let donor_turn_4 = forwarded_messages(&bodies[3]);
    assert!(
        offloaded_tool_results(&donor_turn_4) >= 1,
        "the donor's turn 4 carries no offload digest, so adoption has nothing to prove"
    );
    let adopter = [
        forwarded_messages(&bodies[6]),
        forwarded_messages(&bodies[7]),
        forwarded_messages(&bodies[8]),
    ];
    assert_prefix_of(
        &donor_turn_4,
        &adopter[0],
        "adopter's first turn vs donor's turn 4",
    );
    assert_prefix_of(
        &adopter[0],
        &adopter[1],
        "adopter's second turn vs its first",
    );
    assert_prefix_of(
        &adopter[1],
        &adopter[2],
        "adopter's third turn vs its second",
    );
}

fn configure(c: &mut headroom_proxy::config::Config, store: &std::path::Path) {
    c.compression = true;
    c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    c.prefix_replay = true;
    c.ctx_offload = true;
    c.ctx_offload_min_bytes = 1000;
    c.exclude_tools = vec!["read_file".to_string()];
    c.ctx_offload_stale_messages = 4;
    c.ctx_offload_stale_window = 0;
    c.ctx_store_dir = Some(store.to_path_buf());
}

/// Donor and adopter through one proxy process with an in-memory replay store.
async fn adopts_in_process(donor: Peer, adopter: Peer) {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let proxy = start_proxy_with(&upstream.uri(), |c| configure(c, store.path())).await;

    let client = reqwest::Client::new();
    run_donor_turns(&client, &proxy.url(), donor).await;
    run_adopter_turns(&client, &proxy.url(), adopter).await;

    let bodies = captured.lock().unwrap().clone();
    assert_adopted(&bodies);
    proxy.shutdown().await;
}

#[tokio::test]
async fn a_new_session_carrying_another_sessions_history_forwards_that_sessions_bytes() {
    adopts_in_process(DONOR, ADOPTER).await;
}

/// A CLAUDE.md edit changes the `<system-reminder>` inside message 0. The
/// session key hashes message 0 with that text kept, so the same conversation
/// continues under a new key: 16 of the 22 key changes measured on 2026-09-02.
#[tokio::test]
async fn a_conversation_whose_opening_reminder_changed_keeps_its_forwarded_prefix() {
    let donor = Peer {
        auth: "Bearer sk-ant-oat-one-user",
        session: None,
        reminder: REMINDER,
    };
    let adopter = Peer {
        reminder: "cwd is /home/dev/alpha\nCLAUDE.md: prefer tabs",
        ..donor
    };
    adopts_in_process(donor, adopter).await;
}

/// An OAuth token rotation changes the bearer the key is hashed from: the
/// other 7 of the 22.
#[tokio::test]
async fn a_conversation_whose_token_rotated_keeps_its_forwarded_prefix() {
    let donor = Peer {
        auth: "Bearer sk-ant-oat-before-rotation",
        session: None,
        reminder: REMINDER,
    };
    let adopter = Peer {
        auth: "Bearer sk-ant-oat-after-rotation",
        ..donor
    };
    adopts_in_process(donor, adopter).await;
}

#[tokio::test]
async fn adoption_survives_a_proxy_restart_between_the_two_sessions() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let replay = tempfile::tempdir().unwrap();

    let persistent = {
        let store = store.path().to_path_buf();
        let replay = replay.path().to_string_lossy().into_owned();
        move |c: &mut headroom_proxy::config::Config| {
            configure(c, &store);
            c.replay_store_dir = replay.clone();
        }
    };

    let client = reqwest::Client::new();
    let proxy = start_proxy_with(&upstream.uri(), persistent.clone()).await;
    run_donor_turns(&client, &proxy.url(), DONOR).await;
    proxy.shutdown().await;

    let proxy = start_proxy_with(&upstream.uri(), persistent).await;
    run_adopter_turns(&client, &proxy.url(), ADOPTER).await;

    let bodies = captured.lock().unwrap().clone();
    assert_adopted(&bodies);
    proxy.shutdown().await;
}
