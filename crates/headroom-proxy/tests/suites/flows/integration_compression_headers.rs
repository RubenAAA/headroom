//! Responses on the main forward path say what compression did, buffered
//! and streamed alike (upstream `2c4dc446`), through the `x-headroom-*`
//! headers the Gemini handler already sends.

use super::common;

use common::start_proxy_with;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4\",\"content\":[],\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// A turn whose newest message is a large, repetitive JSON tool result: the
/// live zone compresses it. The tool is not named `read`, which is kept
/// byte-exact.
fn body(stream: bool) -> Value {
    let rows: Vec<Value> = (0..1500)
        .map(|i| json!({"id": i, "kind": "row", "value": format!("repeat-{}", i % 5), "status": "ok"}))
        .collect();
    json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "stream": stream,
        "messages": [
            {"role": "user", "content": "list the rows"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "list_rows", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": serde_json::to_string(&rows).unwrap()}
            ]}
        ]
    })
}

async fn headers_for(stream: bool) -> reqwest::header::HeaderMap {
    let upstream = MockServer::start().await;
    let reply = if stream {
        ResponseTemplate::new(200).set_body_raw(SSE, "text/event-stream")
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 50, "output_tokens": 2}
        }))
    };
    Mock::given(method("POST"))
        .respond_with(reply)
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    })
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "k")
        .json(&body(stream))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let headers = resp.headers().clone();
    let _ = resp.bytes().await;
    proxy.shutdown().await;
    headers
}

fn number(headers: &reqwest::header::HeaderMap, name: &str) -> i64 {
    headers
        .get(name)
        .unwrap_or_else(|| panic!("{name} missing: {headers:?}"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn buffered_and_streamed_turns_report_their_compression() {
    for stream in [false, true] {
        let h = headers_for(stream).await;
        let (before, after, saved) = (
            number(&h, "x-headroom-tokens-before"),
            number(&h, "x-headroom-tokens-after"),
            number(&h, "x-headroom-tokens-saved"),
        );
        assert!(saved > 0, "stream={stream}: nothing saved: {h:?}");
        assert_eq!(before - saved, after, "stream={stream}");
        assert_eq!(h["x-headroom-model"], "claude-sonnet-4");
        assert!(
            !h["x-headroom-transforms"].is_empty(),
            "stream={stream}: no transforms"
        );
    }
}
