//! Codex's `additional_tools` encoding is internalized for tool processing but
//! must be restored before the upstream sees the request.

mod common;

use common::start_proxy_with;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn responses_tools_are_processed_then_restored_to_the_transcript() {
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
        config.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    })
    .await;
    let payload = json!({
        "model": "gpt-5.6-sol",
        "input": [
            {"type": "message", "role": "user", "content": "before"},
            {"type": "additional_tools", "id": "carrier", "tools": [
                {
                    "type": "function",
                    "name": "shell",
                    "function": {"parameters": {"type": "object", "properties": {}}}
                },
                {
                    "type": "function",
                    "name": "read_file",
                    "function": {"parameters": {"type": "object", "properties": {}}}
                }
            ]},
            {"type": "message", "role": "user", "content": "after"}
        ]
    });
    let response = reqwest::Client::new()
        .post(format!("{}/v1/responses", proxy.url()))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&payload).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let body = captured.lock().unwrap().clone().expect("upstream body");
    let forwarded: Value = serde_json::from_slice(&body).unwrap();
    assert!(forwarded.get("tools").is_none());
    let input = forwarded["input"].as_array().unwrap();
    assert_eq!(input[0]["content"], "before");
    assert_eq!(input[1]["type"], "additional_tools");
    assert_eq!(input[1]["id"], "carrier");
    assert_eq!(input[2]["content"], "after");
    assert_eq!(input[1]["tools"][0]["name"], "read_file");
    assert_eq!(input[1]["tools"][1]["name"], "shell");

    proxy.shutdown().await;
}
