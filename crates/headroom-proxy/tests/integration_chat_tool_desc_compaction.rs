//! `HEADROOM_TOOL_DESC_MAX_CHARS` must actually bite on `/v1/chat/completions`.
//!
//! The compaction pass in `tool_schema_compaction` was fully implemented but
//! reachable from no handler, so the env var was a silent no-op for every
//! OpenAI chat client (opencode, Cline, Aider, Roo, LiteLLM-routed).
//!
//! Its own test binary on purpose: the knob is process-global, and the
//! byte-equality assertions in `integration_chat_completions.rs` would race it
//! if both ran in one process.

mod common;

use common::start_proxy_with;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LONG_DESC: &str = "Search the web and return the ten most relevant results \
                         together with a short summary of each one.";

#[tokio::test]
async fn tool_descriptions_are_truncated_before_the_upstream_sees_them() {
    std::env::set_var("HEADROOM_TOOL_DESC_MAX_CHARS", "20");

    let upstream = MockServer::start().await;
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            *sink.lock().unwrap() = Some(req.body.clone());
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = false;
    })
    .await;

    let payload = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "search",
                "description": LONG_DESC,
                "parameters": {"type": "object", "properties": {}}
            }
        }]
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url()))
        .header("content-type", "application/json")
        .header(
            "authorization",
            "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.signature_bytes",
        )
        .body(serde_json::to_vec(&payload).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let got = captured.lock().unwrap().clone().expect("upstream got body");
    let sent: Value = serde_json::from_slice(&got).unwrap();
    let desc = sent["tools"][0]["function"]["description"]
        .as_str()
        .expect("description survives as a string");
    assert!(
        desc.chars().count() < LONG_DESC.chars().count(),
        "description reached the upstream untruncated: {desc}"
    );
    // Only the description is touched.
    assert_eq!(sent["tools"][0]["function"]["name"], json!("search"));
    assert_eq!(sent["messages"], payload["messages"]);

    proxy.shutdown().await;
    std::env::remove_var("HEADROOM_TOOL_DESC_MAX_CHARS");
}
