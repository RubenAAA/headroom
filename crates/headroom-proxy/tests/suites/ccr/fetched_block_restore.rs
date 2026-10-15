//! A block the model fetched back is forwarded raw on the next turn.
//!
//! The fetch runs on a hidden round, so its answer never enters the client's
//! history; left alone, the next turn forwards the digest again and the model
//! fetches it again. On 2026-10-15 one session did that for six turns running.
//!
//! The upstream here behaves like a model that needs the file: whenever a
//! request carries a digest, it asks for it. Turn one forwards the fresh
//! `read_file` result as a digest and the proxy answers the fetch. Turn two
//! must forward the client's raw result, so the model has nothing to fetch.

use super::common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use headroom_core::transforms::live_zone::CTX_OFFLOAD_MARKER_PREFIX;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(blocks: &str, stop_reason: &str) -> String {
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-6\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\n",
            "{blocks}",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        blocks = blocks,
        stop_reason = stop_reason,
    )
}

fn text_reply(text: &str) -> String {
    sse(
        &format!(
            concat!(
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                "event: content_block_delta\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n",
                "event: content_block_stop\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            ),
            text = text
        ),
        "end_turn",
    )
}

fn fetch_reply(hash: &str) -> String {
    sse(
        &format!(
            concat!(
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_fetch\",\"name\":\"headroom_retrieve\",\"input\":{{}}}}}}\n\n",
                "event: content_block_delta\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"hash\\\":\\\"{hash}\\\"}}\"}}}}\n\n",
                "event: content_block_stop\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            ),
            hash = hash
        ),
        "tool_use",
    )
}

/// The hash of the first digest in a forwarded body, if there is one.
fn digest_hash(body: &str) -> Option<String> {
    let start = body.find(CTX_OFFLOAD_MARKER_PREFIX)? + CTX_OFFLOAD_MARKER_PREFIX.len();
    let rest = &body[start..];
    Some(rest[..rest.find(">>")?].to_string())
}

/// Answers a continuation, asks for any digest it is shown, else answers.
async fn mount_model(upstream: &MockServer) -> Arc<Mutex<Vec<String>>> {
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            let body = String::from_utf8_lossy(&req.body).to_string();
            sink.lock().unwrap().push(body.clone());
            let reply = if body.contains("\"name\":\"headroom_retrieve\",\"input\"") {
                text_reply("ANSWER")
            } else if let Some(hash) = digest_hash(&body) {
                fetch_reply(&hash)
            } else {
                text_reply("DONE")
            };
            ResponseTemplate::new(200).set_body_raw(reply, "text/event-stream")
        })
        .mount(upstream)
        .await;
    captured
}

fn request(messages: &[Value]) -> Value {
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "stream": true,
        "system": [{"type": "text", "text": "you are a helpful assistant"}],
        "tools": [{"name": "read_file", "description": "read a file",
                   "input_schema": {"type": "object", "properties": {}}}],
        "messages": messages
    })
}

#[tokio::test]
async fn a_fetched_block_goes_out_raw_on_the_next_turn() {
    let upstream = MockServer::start().await;
    let captured = mount_model(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.prefix_replay = true;
        c.ctx_offload = true;
        c.ctx_offload_min_bytes = 1000;
        c.ctx_store_dir = Some(store.path().to_path_buf());
        c.ccr_handle_responses = true;
    })
    .await;

    let file = "fn parse() -> Result<(), Error> { Ok(()) }\n".repeat(80);
    let mut messages = vec![
        json!({"role": "user", "content": "read the parser"}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "/src/parse.rs"}}]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": file}]}),
    ];
    let client = reqwest::Client::new();
    let send = |messages: Vec<Value>| {
        let client = client.clone();
        let url = format!("{}/v1/messages", proxy.url());
        async move {
            let resp = client
                .post(url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("authorization", "Bearer sk-ant-oat-fetched-restore")
                .body(serde_json::to_vec(&request(&messages)).unwrap())
                .send()
                .await
                .expect("proxy reachable");
            assert!(
                resp.status().is_success(),
                "proxy returned {}",
                resp.status()
            );
            String::from_utf8_lossy(&resp.bytes().await.unwrap()).to_string()
        }
    };

    let first = send(messages.clone()).await;
    assert!(
        first.contains("ANSWER"),
        "turn one did not resolve the fetch: {first}"
    );
    {
        let bodies = captured.lock().unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "turn one: the request and one hidden round"
        );
        assert!(
            digest_hash(&bodies[0]).is_some(),
            "turn one never offloaded the file, so nothing here was tested"
        );
    }

    // The turn closes on its own task; its fetch reaches the session there.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": "ANSWER"}]}));
    messages.push(json!({"role": "user", "content": "now fix it"}));
    let second = send(messages).await;
    assert!(second.contains("DONE"), "turn two: {second}");

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(
        bodies.len(),
        3,
        "turn two fetched the same block again: it was forwarded as a digest"
    );
    let forwarded: Value = serde_json::from_str(&bodies[2]).unwrap();
    assert_eq!(
        forwarded["messages"][2]["content"][0]["content"],
        json!(file),
        "turn two must carry the client's raw tool result"
    );

    proxy.shutdown().await;
}
