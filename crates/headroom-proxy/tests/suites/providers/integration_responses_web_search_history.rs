//! A Codex local compaction resends the whole history with `tools: []`. When
//! that history holds a hosted `web_search_call`, the ChatGPT backend answers
//! 200 then an in-band error and the session never answers again. The proxy
//! drops those items from such a request (upstream `2a70d1281`), and leaves a
//! request that declares a web_search tool, or asked for bypass, alone. With
//! `--compression false` such requests are read whole before forwarding;
//! every other request still streams through byte for byte.

use super::common;

use common::start_proxy_with;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn compaction_body(tools: Value) -> Value {
    json!({
        "model": "gpt-5.5",
        "instructions": "Summarize the conversation.",
        "input": [
            {"type": "message", "role": "user", "content": "look this up"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed",
             "action": {"type": "search", "query": "query 1"}},
            {"type": "message", "role": "assistant", "content": "found it"},
            {"type": "message", "role": "user", "content": "Summarize."}
        ],
        "tools": tools
    })
}

/// Send `body` to `/v1/responses` through the proxy and return the bytes the
/// upstream received.
async fn forwarded_bytes(
    body: impl Into<reqwest::Body>,
    bypass: bool,
    configure: impl FnOnce(&mut headroom_proxy::Config),
) -> Vec<u8> {
    let upstream = MockServer::start().await;
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            *sink.lock().unwrap() = Some(request.body.clone());
            ResponseTemplate::new(200).set_body_json(json!({"id": "resp_1", "output": []}))
        })
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), configure).await;
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/responses", proxy.url()))
        .header("content-type", "application/json");
    if bypass {
        request = request.header("x-headroom-bypass", "true");
    }
    let response = request.body(body).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body = captured.lock().unwrap().clone().expect("upstream body");
    proxy.shutdown().await;
    body
}

/// Send `payload` through the proxy and return the forwarded body's item types.
async fn forwarded_item_types(payload: &Value, bypass: bool, compression: bool) -> Vec<String> {
    let body = forwarded_bytes(serde_json::to_vec(payload).unwrap(), bypass, |config| {
        config.compression = compression;
    })
    .await;
    let forwarded: Value = serde_json::from_slice(&body).unwrap();
    forwarded["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn compaction_without_web_search_tool_drops_web_search_calls() {
    let types = forwarded_item_types(&compaction_body(json!([])), false, true).await;
    assert_eq!(types, ["message", "message", "message"]);
}

#[tokio::test]
async fn request_declaring_web_search_keeps_web_search_calls() {
    let types = forwarded_item_types(
        &compaction_body(json!([{"type": "web_search"}])),
        false,
        true,
    )
    .await;
    assert_eq!(types, ["message", "web_search_call", "message", "message"]);
}

#[tokio::test]
async fn bypass_request_keeps_web_search_calls() {
    let types = forwarded_item_types(&compaction_body(json!([])), true, true).await;
    assert_eq!(types, ["message", "web_search_call", "message", "message"]);
}

#[tokio::test]
async fn compaction_without_web_search_tool_drops_web_search_calls_with_compression_off() {
    let types = forwarded_item_types(&compaction_body(json!([])), false, false).await;
    assert_eq!(types, ["message", "message", "message"]);
}

#[tokio::test]
async fn bypass_request_keeps_web_search_calls_with_compression_off() {
    let types = forwarded_item_types(&compaction_body(json!([])), true, false).await;
    assert_eq!(types, ["message", "web_search_call", "message", "message"]);
}

#[tokio::test]
async fn untouched_request_is_byte_identical_with_compression_off() {
    // Odd spacing and key order survive only if nothing re-serialises. The
    // second body is parsed (it names `web_search_call`) but declares the
    // tool, so it must come out as it went in.
    for body in [
        &br#"{"model":"gpt-5.5",  "input":[{"type":"message","role":"user","content":"hi"}],"tools":[]}"#[..],
        &br#"{ "tools":[{"type":"web_search"}], "input":[{"type":"web_search_call","id":"ws_1"}] }"#[..],
    ] {
        let forwarded = forwarded_bytes(body.to_vec(), false, |config| {
            config.compression = false;
        })
        .await;
        assert_eq!(forwarded, body);
    }
}

#[tokio::test]
async fn oversize_request_streams_through_unchanged_with_compression_off() {
    // Past the compression buffer limit the passthrough arm must not 413 the
    // way the compression arm does: it forwards the request as it came, both
    // when Content-Length announces the size and when a chunked body crosses
    // the limit mid-read.
    let body = serde_json::to_vec(&compaction_body(json!([]))).unwrap();
    let limit = |config: &mut headroom_proxy::Config| {
        config.compression = false;
        config.compression_max_body_bytes = 64;
    };
    assert_eq!(forwarded_bytes(body.clone(), false, limit).await, body);
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        body.chunks(40).map(|chunk| Ok(chunk.to_vec())).collect();
    let chunked = reqwest::Body::wrap_stream(futures_util::stream::iter(chunks));
    assert_eq!(forwarded_bytes(chunked, false, limit).await, body);
}
