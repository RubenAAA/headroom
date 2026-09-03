//! Large `tool_use` input strings (a Write's `content`) are offloaded the way
//! tool results are, and the forwarded prefix never changes underneath the
//! provider's cache because of it.

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
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tu\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":9000,\"cache_read_input_tokens\":0}}}\n\n",
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

fn file_content() -> String {
    "fn parse(line: &str) -> Option<Token> { line.split_once(':') }\n".repeat(100)
}

/// Turn `n` of one session. Turn 2's assistant message carries the Write.
fn turn(n: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "build a parser"}
    ]})];
    if n >= 2 {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": "Writing the parser."},
            {"type": "tool_use", "id": "toolu_write", "name": "Write",
             "input": {"file_path": "/src/parser.rs", "content": file_content()}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_write",
             "content": "File created successfully at: /src/parser.rs"}
        ]}));
    }
    for i in 3..=n {
        messages.push(json!({"role": "assistant", "content": format!("reply {i}")}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("follow up {i}")}
        ]}));
    }
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "stream": true,
        "system": [{"type": "text", "text": "you are a helpful assistant",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
        "tools": [{"name": "Write", "description": "write a file",
                   "input_schema": {"type": "object", "properties": {}}}],
        "messages": messages
    })
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, n: usize) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat-tool-use-offload")
        .body(serde_json::to_vec(&turn(n)).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert!(resp.status().is_success(), "turn {n}: {}", resp.status());
    // Drain the stream so the proxy has recorded the turn before the next one.
    let _ = resp.bytes().await.unwrap();
}

fn messages_of(body: &[u8]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["messages"].as_array().cloned().unwrap()
}

/// Messages with every `cache_control` removed: breakpoints move by design.
fn without_cache_control(messages: &[Value]) -> Vec<Value> {
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
    let mut out = messages.to_vec();
    out.iter_mut().for_each(strip);
    out
}

fn write_content(body: &[u8]) -> String {
    messages_of(body)[1]["content"][1]["input"]["content"]
        .as_str()
        .expect("Write content is a string")
        .to_string()
}

fn marker_hash(text: &str) -> String {
    let start = text
        .find(CTX_OFFLOAD_MARKER_PREFIX)
        .expect("digest carries the marker")
        + CTX_OFFLOAD_MARKER_PREFIX.len();
    let rest = &text[start..];
    rest[..rest.find(">>").unwrap()].to_string()
}

/// Every turn after the first re-sends the previous turn's forwarded messages
/// byte for byte, `cache_control` aside.
fn assert_prefix_stable(bodies: &[Vec<u8>]) {
    for k in 1..bodies.len() {
        let prev = without_cache_control(&messages_of(&bodies[k - 1]));
        let cur = without_cache_control(&messages_of(&bodies[k]));
        assert!(cur.len() >= prev.len(), "turn {} shrank", k + 1);
        for (i, (p, c)) in prev.iter().zip(cur.iter()).enumerate() {
            assert_eq!(
                serde_json::to_string(p).unwrap(),
                serde_json::to_string(c).unwrap(),
                "message {i} changed between turn {} and turn {}",
                k,
                k + 1
            );
        }
    }
}

fn stored_original(store: &std::path::Path, hash: &str) -> Option<String> {
    let ccr = headroom_core::ccr::from_config(&headroom_core::ccr::CcrBackendConfig::Sqlite {
        path: store.join("ccr.db"),
        ttl_seconds: 600,
    })
    .expect("open the ccr store");
    ccr.get(hash)
}

fn configure(store: &std::path::Path) -> impl FnOnce(&mut headroom_proxy::config::Config) + '_ {
    move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.ctx_offload = true;
        c.ctx_offload_tool_use = true;
        c.ctx_offload_min_bytes = 1000;
        c.ctx_store_dir = Some(store.to_path_buf());
    }
}

#[tokio::test]
async fn tool_use_offload_keeps_the_forwarded_prefix_byte_identical() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    let client = reqwest::Client::new();

    for n in 1..=4 {
        post_turn(&client, &proxy.url(), n).await;
    }
    proxy.shutdown().await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);

    // (i) The Write's content went out as a digest the turn it first appeared.
    let digest = write_content(&bodies[1]);
    assert!(
        digest.contains(CTX_OFFLOAD_MARKER_PREFIX),
        "turn 2 forwarded the Write content raw"
    );
    assert!(digest.len() < file_content().len() / 2);

    // (ii) Nothing already forwarded changed afterwards.
    assert_prefix_stable(&bodies);

    // (iii) The digest points at the original.
    assert_eq!(
        stored_original(store.path(), &marker_hash(&digest)).as_deref(),
        Some(file_content().as_str())
    );
}

#[tokio::test]
async fn tool_use_offload_survives_a_proxy_restart() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    for n in 1..=3 {
        post_turn(&client, &proxy.url(), n).await;
    }
    proxy.shutdown().await;

    // A fresh process knows nothing of the session but what the store kept.
    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    post_turn(&client, &proxy.url(), 4).await;
    proxy.shutdown().await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);
    assert!(write_content(&bodies[1]).contains(CTX_OFFLOAD_MARKER_PREFIX));
    assert!(
        write_content(&bodies[3]).contains(CTX_OFFLOAD_MARKER_PREFIX),
        "the restarted proxy forwarded the Write content raw"
    );
    assert_prefix_stable(&bodies);
}
