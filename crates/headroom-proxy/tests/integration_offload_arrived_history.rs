//! A session that reaches the proxy mid-conversation has nothing the provider
//! can read back, so its history is a fresh write whatever the proxy sends.
//! Old tool results in that history must be offloaded on the first sighting;
//! otherwise they stay verbatim for every later turn of the session.

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
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_hist\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":9000,\"cache_read_input_tokens\":0}}}\n\n",
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

/// `turns` completed tool calls, each with a tool result far above the offload
/// floor, then the user's next message.
fn history_of(turns: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>cwd is /home/dev/alpha</system-reminder>"},
        {"type": "text", "text": "build a parser"}
    ]})];
    for i in 0..turns {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": format!("toolu_{i}"), "name": "read_file",
             "input": {"path": format!("/src/{i}.rs")}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{i}"),
             "content": format!("line {i}\n").repeat(600)}
        ]}));
    }
    messages.push(json!({"role": "assistant", "content": "done reading"}));
    messages.push(json!({"role": "user", "content": [{"type": "text", "text": "now write it"}]}));
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

fn offloaded_tool_results(body: &[u8]) -> usize {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .filter(|b| {
            serde_json::to_string(b)
                .unwrap()
                .contains(CTX_OFFLOAD_MARKER_PREFIX)
        })
        .count()
}

#[tokio::test]
async fn a_session_arriving_with_history_offloads_it_on_first_sight() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.ctx_offload = true;
        c.ctx_offload_min_bytes = 1000;
        c.ctx_offload_stale_messages = 4;
        c.ctx_offload_stale_window = 4;
        c.ctx_store_dir = Some(store.path().to_path_buf());
    })
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat-arrived-history")
        .body(serde_json::to_vec(&history_of(8)).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert!(
        resp.status().is_success(),
        "proxy returned {}",
        resp.status()
    );
    let _ = resp.bytes().await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let offloaded = offloaded_tool_results(&bodies[0]);
    assert!(
        offloaded >= 6,
        "only {offloaded} of 8 old tool results were offloaded on a history the \
         provider has never seen"
    );

    proxy.shutdown().await;
}
