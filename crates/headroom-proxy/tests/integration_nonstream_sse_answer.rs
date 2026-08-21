//! A `stream: false` request must never be answered with an event stream.
//!
//! Upstream can answer a non-streaming turn with `text/event-stream` — a CCR
//! flip changes the body without touching `Accept`. Forwarding that verbatim
//! hands an SSE body to a client that never opted into streaming and cannot
//! parse it. Ports upstream's `0e26fb80` to the Rust proxy.

mod common;

use common::start_proxy_with;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn non_streaming_body() -> serde_json::Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 16,
        "stream": false,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// A complete stream is rebuilt into the single JSON reply that was asked for.
#[tokio::test]
async fn complete_event_stream_is_rebuilt_as_json() {
    let upstream = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-3-5-sonnet-20241022","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .json(&non_streaming_body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ctype.starts_with("application/json"),
        "a non-streaming caller must not get an event stream, got {ctype}"
    );

    let body: serde_json::Value = resp.json().await.expect("body parses as json");
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "hello");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["output_tokens"], 5);

    proxy.shutdown().await;
}

/// A stream that stops early cannot be rebuilt honestly, so it is a 502 rather
/// than a turn that looks whole and is not.
#[tokio::test]
async fn truncated_event_stream_becomes_502() {
    let upstream = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-3-5-sonnet-20241022","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}"#,
        "\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
    })
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .json(&non_streaming_body())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        502,
        "an incomplete stream must not be dressed up as a complete turn"
    );
    let body: serde_json::Value = resp.json().await.expect("body parses as json");
    assert_eq!(body["error"]["type"], "upstream_protocol_error");

    proxy.shutdown().await;
}

/// A client that did ask to stream still gets the stream, untouched.
#[tokio::test]
async fn streaming_client_still_gets_the_event_stream() {
    let upstream = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-3-5-sonnet-20241022","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
    })
    .await;

    let mut body = non_streaming_body();
    body["stream"] = json!(true);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ctype.starts_with("text/event-stream"),
        "a streaming caller must still get a stream, got {ctype}"
    );
    let text = resp.text().await.unwrap();
    assert!(text.contains("message_stop"), "stream body reached client");

    proxy.shutdown().await;
}
