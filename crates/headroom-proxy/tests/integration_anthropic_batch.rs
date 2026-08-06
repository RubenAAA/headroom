//! Integration tests for the Anthropic Batch API
//! (`/v1/messages/batches*`) — Rust port of the Python
//! `handle_anthropic_batch_*` handlers.
//!
//! Exercised through the real router against a wiremock upstream.

mod common;

use common::start_proxy_with;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mount a capture-on-path handler that records the request body and
/// returns `response_body` (with the given status + content type).
async fn mount_capture(
    upstream: &MockServer,
    method_name: &str,
    path_str: &str,
    status: u16,
    response_body: &'static str,
) -> Arc<Mutex<Option<Vec<u8>>>> {
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method(method_name))
        .and(path(path_str))
        .respond_with(move |req: &wiremock::Request| {
            *captured_clone.lock().unwrap() = Some(req.body.clone());
            ResponseTemplate::new(status).set_body_string(response_body)
        })
        .mount(upstream)
        .await;
    captured
}

/// A large JSON-array payload that reliably triggers SmartCrusher
/// live-zone compression (mirrors the core `live_zone_ccr` fixture).
fn large_json_array_payload() -> String {
    let items: Vec<Value> = (0..40)
        .map(|i| {
            json!({
                "id": i,
                "name": format!("entry_{i}"),
                "score": i * 7,
                "active": i % 2 == 0,
                "notes": "lorem ipsum dolor sit amet, consectetur adipiscing elit",
            })
        })
        .collect();
    serde_json::to_string(&Value::Array(items)).unwrap()
}

// ─── 400 on missing / empty requests ───────────────────────────────────

#[tokio::test]
async fn create_empty_requests_returns_400_anthropic_envelope() {
    let upstream = MockServer::start().await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.enable_batch_api = true).await;

    for payload in [json!({"requests": []}), json!({"model": "x"})] {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages/batches", proxy.url()))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&payload).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requests"));
    }
    proxy.shutdown().await;
}

// ─── Create: per-item compression + structure preservation ─────────────

#[tokio::test]
async fn create_compresses_messages_and_preserves_structure() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(
        &upstream,
        "POST",
        "/v1/messages/batches",
        200,
        r#"{"id":"batch_abc","type":"message_batch","processing_status":"in_progress"}"#,
    )
    .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.enable_batch_api = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    })
    .await;

    let payload = large_json_array_payload();
    let original_content_len = payload.len();
    let batch = json!({
        "requests": [{
            "custom_id": "req-1",
            "params": {
                "model": "claude-3-5-sonnet-20241022",
                "max_tokens": 1024,
                "system": "You are helpful.",
                "tools": [{
                    "name": "search",
                    "description": "search",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": payload}
                    ]
                }]
            }
        }]
    });

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages/batches", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-test")
        .body(serde_json::to_vec(&batch).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp_body: Value = resp.json().await.unwrap();
    assert_eq!(
        resp_body["id"], "batch_abc",
        "upstream response passed through"
    );

    // Inspect what upstream actually received.
    let got = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream body captured");
    let sent: Value = serde_json::from_slice(&got).unwrap();
    let req0 = &sent["requests"][0];

    // custom_id + system preserved verbatim.
    assert_eq!(req0["custom_id"], "req-1");
    assert_eq!(req0["params"]["system"], "You are helpful.");

    // Messages were compressed (content shorter than the original).
    let sent_content = serde_json::to_string(&req0["params"]["messages"]).unwrap();
    assert!(
        sent_content.len() < original_content_len,
        "expected compressed messages to be smaller than the {original_content_len}-byte original"
    );

    // Compression saved tokens → the CCR retrieval tool was injected
    // alongside the original tool (proves the per-item pipeline ran).
    let tool_names: Vec<&str> = req0["params"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"search"), "original tool preserved");
    assert!(
        tool_names.contains(&"headroom_retrieve"),
        "CCR retrieval tool injected; got {tool_names:?}"
    );

    proxy.shutdown().await;
}

// ─── Create: per-item failure isolation ────────────────────────────────

#[tokio::test]
async fn create_per_item_failure_isolation() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(
        &upstream,
        "POST",
        "/v1/messages/batches",
        200,
        r#"{"id":"batch_iso"}"#,
    )
    .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.enable_batch_api = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    })
    .await;

    let batch = json!({
        "requests": [
            {
                "custom_id": "good",
                "params": {
                    "model": "claude-3-5-sonnet-20241022",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }
            },
            // Malformed: params is a string, not an object.
            {"custom_id": "bad", "params": "not-an-object"}
        ]
    });

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages/batches", proxy.url()))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&batch).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let got = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream body captured");
    let sent: Value = serde_json::from_slice(&got).unwrap();
    // The malformed item is forwarded unchanged; the batch still succeeds.
    assert_eq!(sent["requests"][1]["custom_id"], "bad");
    assert_eq!(sent["requests"][1]["params"], "not-an-object");
    assert_eq!(sent["requests"][0]["custom_id"], "good");

    proxy.shutdown().await;
}

// ─── Passthrough: list / get / cancel forward verbatim ─────────────────

#[tokio::test]
async fn passthrough_list_forwards_method_path_query() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.enable_batch_api = true).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/messages/batches?limit=5", proxy.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["data"].is_array());
    proxy.shutdown().await;
}

#[tokio::test]
async fn passthrough_get_and_cancel_forward() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/batch_xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"batch_xyz"}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches/batch_xyz/cancel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"id":"batch_xyz","processing_status":"canceling"}"#),
        )
        .mount(&upstream)
        .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.enable_batch_api = true).await;

    let client = reqwest::Client::new();
    let get = client
        .get(format!("{}/v1/messages/batches/batch_xyz", proxy.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 200);
    assert_eq!(get.json::<Value>().await.unwrap()["id"], "batch_xyz");

    let cancel = client
        .post(format!(
            "{}/v1/messages/batches/batch_xyz/cancel",
            proxy.url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    assert_eq!(
        cancel.json::<Value>().await.unwrap()["processing_status"],
        "canceling"
    );
    proxy.shutdown().await;
}

// ─── Results: no context → byte-identical passthrough ──────────────────

#[tokio::test]
async fn results_no_context_passthrough_byte_identical() {
    let upstream = MockServer::start().await;
    let jsonl = "{\"custom_id\":\"a\",\"result\":{\"type\":\"succeeded\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}}";
    mount_capture(
        &upstream,
        "GET",
        "/v1/messages/batches/unknown_batch/results",
        200,
        jsonl,
    )
    .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.enable_batch_api = true).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "{}/v1/messages/batches/unknown_batch/results",
            proxy.url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, jsonl, "no stored context → verbatim passthrough");
    proxy.shutdown().await;
}

// ─── Results: non-200 upstream → verbatim passthrough ──────────────────

#[tokio::test]
async fn results_non_200_passthrough() {
    let upstream = MockServer::start().await;
    mount_capture(
        &upstream,
        "GET",
        "/v1/messages/batches/err_batch/results",
        404,
        r#"{"type":"error","error":{"type":"not_found_error","message":"nope"}}"#,
    )
    .await;
    let proxy = start_proxy_with(&upstream.uri(), |c| c.enable_batch_api = true).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "{}/v1/messages/batches/err_batch/results",
            proxy.url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");
    proxy.shutdown().await;
}

// ─── Results: stored context + CCR tool call → continuation ────────────

#[tokio::test]
async fn results_with_ccr_tool_call_runs_continuation() {
    let upstream = MockServer::start().await;

    // 1. Batch create → returns an id so the proxy stores CCR context.
    Mock::given(method("POST"))
        .and(path("/v1/messages/batches"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"batch_ccr"}"#))
        .mount(&upstream)
        .await;

    // 2. Continuation call to /v1/messages → final answer (no tool calls).
    let continuation_hits: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let hits = continuation_hits.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_req: &wiremock::Request| {
            *hits.lock().unwrap() += 1;
            ResponseTemplate::new(200).set_body_string(
                r#"{"id":"msg_final","type":"message","role":"assistant","content":[{"type":"text","text":"final answer"}],"stop_reason":"end_turn"}"#,
            )
        })
        .mount(&upstream)
        .await;

    // 3. Results → one line whose message contains a CCR tool_use.
    let results_line = concat!(
        "{\"custom_id\":\"req-ccr\",\"result\":{\"type\":\"succeeded\",\"message\":",
        "{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[",
        "{\"type\":\"text\",\"text\":\"let me retrieve\"},",
        "{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"headroom_retrieve\",\"input\":{\"hash\":\"abc123def456abc123def456\"}}",
        "]}}}"
    );
    Mock::given(method("GET"))
        .and(path("/v1/messages/batches/batch_ccr/results"))
        .respond_with(ResponseTemplate::new(200).set_body_string(results_line))
        .mount(&upstream)
        .await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.enable_batch_api = true;
        // ccr_inject_tool defaults on; keep compression off so the stored
        // context messages are deterministic.
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
    })
    .await;

    let client = reqwest::Client::new();

    // Create (populates the batch context store under "batch_ccr").
    let create = client
        .post(format!("{}/v1/messages/batches", proxy.url()))
        .header("content-type", "application/json")
        .header("x-api-key", "sk-test")
        .body(
            serde_json::to_vec(&json!({
                "requests": [{
                    "custom_id": "req-ccr",
                    "params": {
                        "model": "claude-3-5-sonnet-20241022",
                        "max_tokens": 256,
                        "messages": [{"role": "user", "content": "question"}]
                    }
                }]
            }))
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 200);

    // Results → triggers CCR continuation.
    let resp = client
        .get(format!(
            "{}/v1/messages/batches/batch_ccr/results",
            proxy.url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/jsonl")
    );
    let body = resp.text().await.unwrap();

    // A continuation POST to /v1/messages happened.
    assert_eq!(
        *continuation_hits.lock().unwrap(),
        1,
        "one continuation round"
    );

    // No trailing newline; single line.
    assert!(!body.ends_with('\n'), "no trailing newline");
    assert_eq!(body.lines().count(), 1);

    let line: Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(line["custom_id"], "req-ccr");
    assert_eq!(line["result"]["type"], "succeeded");
    assert_eq!(
        line["result"]["message"]["content"][0]["text"], "final answer",
        "message replaced with the continued response"
    );

    proxy.shutdown().await;
}
