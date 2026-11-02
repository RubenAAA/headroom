//! A Codex local compaction resends the whole history with `tools: []`. When
//! that history holds a hosted `web_search_call`, the ChatGPT backend answers
//! 200 then an in-band error and the session never answers again. The proxy
//! drops those items from such a request (upstream `2a70d1281`), and leaves a
//! request that declares a web_search tool, or asked for bypass, alone.

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

/// Send `payload` through the proxy and return the forwarded body's item types.
async fn forwarded_item_types(payload: &Value, bypass: bool) -> Vec<String> {
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
    let proxy = start_proxy_with(&upstream.uri(), |config| {
        config.compression = true;
    })
    .await;
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/responses", proxy.url()))
        .header("content-type", "application/json");
    if bypass {
        request = request.header("x-headroom-bypass", "true");
    }
    let response = request
        .body(serde_json::to_vec(payload).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = captured.lock().unwrap().clone().expect("upstream body");
    proxy.shutdown().await;
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
    let types = forwarded_item_types(&compaction_body(json!([])), false).await;
    assert_eq!(types, ["message", "message", "message"]);
}

#[tokio::test]
async fn request_declaring_web_search_keeps_web_search_calls() {
    let types =
        forwarded_item_types(&compaction_body(json!([{"type": "web_search"}])), false).await;
    assert_eq!(types, ["message", "web_search_call", "message", "message"]);
}

#[tokio::test]
async fn bypass_request_keeps_web_search_calls() {
    let types = forwarded_item_types(&compaction_body(json!([])), true).await;
    assert_eq!(types, ["message", "web_search_call", "message", "message"]);
}
