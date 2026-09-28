//! A buffered turn the proxy answers a `headroom_retrieve` call on goes back
//! to the client as the continuation's message, not the one upstream first
//! answered with. Fields the proxy does not model (Claude Code auto mode's
//! `safeguard_results`) belong to the exchange and must survive that swap
//! (upstream `12c15796`), whether the continuation came back as JSON or as a
//! stream the proxy folded.

use super::common;

use common::start_proxy_with_state;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "abcdef1234567890abcdef12";

/// Run one buffered turn whose first answer calls `headroom_retrieve` and
/// carries a `safeguard_results` attachment; `continuation` answers the
/// round the proxy runs. Returns what the client received.
async fn buffered_turn(continuation: ResponseTemplate) -> Value {
    let upstream = MockServer::start().await;
    // The continuation carries the retrieved content as a tool_result.
    Mock::given(method("POST"))
        .and(body_string_contains("tool_result"))
        .respond_with(continuation)
        .with_priority(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "headroom_retrieve",
                         "input": {"hash": HASH}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 5},
            "safeguard_results": {"decision": "FIRST_ROUND_VERDICT"}
        })))
        .with_priority(2)
        .mount(&upstream)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        move |c| {
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
            c.ctx_offload = true;
            c.ctx_store_dir = Some(store_dir);
            c.ccr_handle_responses = true;
        },
        |s| {
            s.ctx_offload
                .as_ref()
                .expect("ctx_offload runtime")
                .store
                .ccr()
                .put(HASH, "the original tool output");
            s
        },
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "k")
        .json(&json!({
            "model": "claude-3-haiku-20240307",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "what did that say"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    proxy.shutdown().await;
    body
}

#[tokio::test]
async fn a_json_continuation_keeps_the_first_answers_attachment() {
    let body = buffered_turn(ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": "claude",
        "content": [{"type": "text", "text": "ANSWER_AFTER_RETRIEVAL"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 90, "output_tokens": 7}
    })))
    .await;

    assert_eq!(
        body["content"][0]["text"], "ANSWER_AFTER_RETRIEVAL",
        "{body}"
    );
    assert_eq!(
        body["safeguard_results"]["decision"], "FIRST_ROUND_VERDICT",
        "the swap must not drop the first answer's attachment: {body}"
    );
}

#[tokio::test]
async fn a_streamed_continuation_keeps_its_own_attachment() {
    let sse = [
        json!({"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":90},"safeguard_results":{"decision":"CONTINUATION_VERDICT"}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ANSWER_AFTER_RETRIEVAL"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect::<String>();
    let body =
        buffered_turn(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")).await;

    assert_eq!(
        body["content"][0]["text"], "ANSWER_AFTER_RETRIEVAL",
        "{body}"
    );
    assert_eq!(
        body["safeguard_results"]["decision"], "CONTINUATION_VERDICT",
        "the fold keeps the continuation's attachment, and it wins: {body}"
    );
}
