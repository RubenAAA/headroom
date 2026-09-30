//! Verbosity steering reaches OpenAI-shaped bodies: `/v1/responses` and
//! `/v1/chat/completions`.
//!
//! The shaper used to run on Anthropic bodies only, so a Codex or Copilot
//! client talking to the proxy directly got no steering. The block is new
//! bytes in a cached prefix, so the check that matters is stability: turn N
//! and turn N+1 of one conversation must send identical system bytes, and a
//! conversation in the holdout's control arm must send none.

use super::common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SENTINEL: &str = "<headroom_output_shaping>";

async fn mount_capture(upstream: &MockServer, route: &'static str) -> Arc<Mutex<Vec<Value>>> {
    let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock()
                .unwrap()
                .push(serde_json::from_slice(&req.body).unwrap());
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(upstream)
        .await;
    captured
}

async fn post(client: &reqwest::Client, url: &str, route: &str, body: &Value) {
    let resp = client
        .post(format!("{url}{route}"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-test-payg")
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

/// Turn `replies + 1` of a Codex-style session. Every session opens with the
/// same text, so only `prompt_cache_key` tells them apart.
fn responses_turn(session: usize, replies: usize) -> Value {
    let mut input = vec![json!({"type": "message", "role": "user", "content": [
        {"type": "input_text", "text": "shared opening instructions"}
    ]})];
    for i in 0..replies {
        input.push(json!({
            "type": "function_call", "name": "shell", "arguments": "{}", "call_id": format!("c{i}")
        }));
        input.push(json!({
            "type": "function_call_output", "call_id": format!("c{i}"), "output": "ok"
        }));
    }
    json!({
        "model": "gpt-5.4-codex",
        "instructions": "You are Codex.",
        "prompt_cache_key": format!("session-{session}"),
        // Codex streams; a non-streaming body could be answered from the
        // semantic cache, which keys on `input` and ignores the session.
        "stream": true,
        "input": input,
    })
}

fn chat_turn(replies: usize) -> Value {
    let mut messages = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "fix the parser"}),
    ];
    for i in 0..replies {
        messages.push(json!({"role": "assistant", "content": format!("reply {i}")}));
        messages.push(json!({"role": "user", "content": format!("follow up {i}")}));
    }
    json!({"model": "gpt-4o", "messages": messages})
}

fn steered(config: &mut headroom_proxy::Config) {
    // Buffered requests only: with compression off the body streams through.
    config.compression = true;
    config.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    config.output_shaper_enabled = true;
    config.verbosity_level = 2;
}

#[tokio::test]
async fn responses_instructions_get_a_byte_stable_block() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream, "/v1/responses").await;
    let proxy = start_proxy_with(&upstream.uri(), steered).await;
    let client = reqwest::Client::new();

    for replies in 0..3 {
        let body = responses_turn(0, replies);
        post(&client, &proxy.url(), "/v1/responses", &body).await;
    }
    // No `instructions` at all: the block becomes the whole field.
    let mut bare = responses_turn(0, 0);
    bare.as_object_mut().unwrap().remove("instructions");
    post(&client, &proxy.url(), "/v1/responses", &bare).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);
    let first = bodies[0]["instructions"].as_str().unwrap();
    assert!(first.starts_with("You are Codex.\n\n"), "{first}");
    assert!(first.contains(SENTINEL), "{first}");
    for b in &bodies[1..3] {
        assert_eq!(b["instructions"].as_str().unwrap(), first);
    }
    assert!(
        bodies[3]["instructions"]
            .as_str()
            .unwrap()
            .starts_with(SENTINEL)
    );
    // Everything but `instructions` is what the client sent.
    assert_eq!(bodies[2]["input"], responses_turn(0, 2)["input"]);

    proxy.shutdown().await;
}

#[tokio::test]
async fn chat_last_system_message_gets_a_byte_stable_block() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream, "/v1/chat/completions").await;
    let proxy = start_proxy_with(&upstream.uri(), steered).await;
    let client = reqwest::Client::new();

    for replies in 0..3 {
        let body = chat_turn(replies);
        post(&client, &proxy.url(), "/v1/chat/completions", &body).await;
    }
    // No system message: one is inserted at the front.
    let mut bare = chat_turn(0);
    bare["messages"].as_array_mut().unwrap().remove(0);
    post(&client, &proxy.url(), "/v1/chat/completions", &bare).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);
    let first = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert!(first.starts_with("You are helpful.\n\n"), "{first}");
    assert!(first.contains(SENTINEL), "{first}");
    for b in &bodies[1..3] {
        assert_eq!(b["messages"][0]["content"].as_str().unwrap(), first);
        assert_eq!(b["messages"][1]["role"], "user");
    }
    assert_eq!(bodies[3]["messages"][0]["role"], "system");
    assert!(
        bodies[3]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with(SENTINEL)
    );

    proxy.shutdown().await;
}

/// The arm follows `prompt_cache_key`, not the shared opening text: every
/// session here opens the same way, and both arms must still appear.
#[tokio::test]
async fn responses_holdout_splits_sessions_and_keeps_each_stable() {
    const SESSIONS: usize = 10;
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream, "/v1/responses").await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        steered(c);
        c.output_holdout = 0.5;
    })
    .await;
    let client = reqwest::Client::new();

    for replies in 0..2 {
        for session in 0..SESSIONS {
            let body = responses_turn(session, replies);
            post(&client, &proxy.url(), "/v1/responses", &body).await;
        }
    }

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2 * SESSIONS);
    let mut steered_count = 0;
    for session in 0..SESSIONS {
        let first = &bodies[session]["instructions"];
        assert_eq!(
            first,
            &bodies[SESSIONS + session]["instructions"],
            "session {session} changed its instructions between turns"
        );
        if first.as_str().unwrap().contains(SENTINEL) {
            steered_count += 1;
        } else {
            assert_eq!(first, "You are Codex.");
        }
    }
    assert!(
        steered_count > 0 && steered_count < SESSIONS,
        "{steered_count} of {SESSIONS} sessions steered; both arms should be present"
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn shaper_off_leaves_openai_bodies_alone() {
    let upstream = MockServer::start().await;
    let responses = mount_capture(&upstream, "/v1/responses").await;
    let chat = mount_capture(&upstream, "/v1/chat/completions").await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        steered(c);
        c.output_shaper_enabled = false;
    })
    .await;
    let client = reqwest::Client::new();

    post(
        &client,
        &proxy.url(),
        "/v1/responses",
        &responses_turn(0, 1),
    )
    .await;
    post(&client, &proxy.url(), "/v1/chat/completions", &chat_turn(1)).await;

    assert_eq!(
        responses.lock().unwrap()[0]["instructions"],
        "You are Codex."
    );
    assert_eq!(
        chat.lock().unwrap()[0]["messages"][0]["content"],
        "You are helpful."
    );

    proxy.shutdown().await;
}
