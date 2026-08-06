//! Phase 3 integration: `CompressionDecision` gate wired into `proxy.rs`.
//!
//! The gate's contract: when the input-side decision says "do not compress"
//! (bypass header, master switch off, or no messages), the request takes the
//! byte-faithful passthrough arm and the proxy NEVER mutates the request
//! bytes. We prove this with a discriminating fixture — a PAYG Anthropic
//! request whose `tools` array is in reverse-alphabetical order. Without a
//! passthrough reason the live-zone E1 tool-sort reorders it (bytes change);
//! with one, the bytes the upstream receives are identical to what the client
//! sent (asserted via SHA-256).

mod common;

use common::start_proxy_with;
use headroom_proxy::config::CompressionMode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn mount_anthropic_capture(upstream: &MockServer) -> Arc<Mutex<Option<Vec<u8>>>> {
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            *captured_clone.lock().unwrap() = Some(req.body.clone());
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(upstream)
        .await;
    captured
}

/// PAYG payload with tools in reverse-alphabetical order — the discriminator.
fn unsorted_tools_payload(messages: Value) -> Vec<u8> {
    let payload = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 32,
        "messages": messages,
        "tools": [
            {"name": "zebra", "description": "z"},
            {"name": "apple", "description": "a"},
            {"name": "mango", "description": "m"},
        ],
    });
    serde_json::to_vec(&payload).unwrap()
}

fn tool_names(body: &[u8]) -> Vec<String> {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// Control: without any passthrough reason, PAYG tools ARE sorted — confirms
/// the fixture actually discriminates (guards the other tests from silently
/// passing because mutation never happens).
#[tokio::test]
async fn control_payg_tools_are_sorted_when_gate_open() {
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = CompressionMode::LiveZone;
    })
    .await;

    let body = unsorted_tools_payload(json!([{"role": "user", "content": "hi"}]));
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-api03-abc")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream_body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(tool_names(&upstream_body), vec!["apple", "mango", "zebra"]);
    proxy.shutdown().await;
}

/// `x-headroom-bypass: true` → byte-identical passthrough (no tool sort).
#[tokio::test]
async fn bypass_header_forwards_byte_identical() {
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = CompressionMode::LiveZone;
    })
    .await;

    let body = unsorted_tools_payload(json!([{"role": "user", "content": "hi"}]));
    let sent_sha = sha256_hex(&body);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-api03-abc")
        .header("content-type", "application/json")
        .header("x-headroom-bypass", "true")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream_body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        sha256_hex(&upstream_body),
        sent_sha,
        "bypass must be byte-faithful"
    );
    assert_eq!(tool_names(&upstream_body), vec!["zebra", "apple", "mango"]);
    proxy.shutdown().await;
}

/// `x-headroom-mode: passthrough` → same byte-identical passthrough.
#[tokio::test]
async fn passthrough_mode_header_forwards_byte_identical() {
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = CompressionMode::LiveZone;
    })
    .await;

    let body = unsorted_tools_payload(json!([{"role": "user", "content": "hi"}]));
    let sent_sha = sha256_hex(&body);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-api03-abc")
        .header("content-type", "application/json")
        .header("x-headroom-mode", "passthrough")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream_body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sha256_hex(&upstream_body), sent_sha);
    proxy.shutdown().await;
}

/// Master switch off (`config.compression=false`) → byte-identical passthrough.
#[tokio::test]
async fn compression_disabled_forwards_byte_identical() {
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = false;
    })
    .await;

    let body = unsorted_tools_payload(json!([{"role": "user", "content": "hi"}]));
    let sent_sha = sha256_hex(&body);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-api03-abc")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream_body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(sha256_hex(&upstream_body), sent_sha);
    proxy.shutdown().await;
}

/// Empty `messages` array → `no_messages` passthrough: even with the gate and
/// master switch open, the tool sort is skipped because the body carries no
/// messages, so bytes are forwarded unchanged.
#[tokio::test]
async fn no_messages_forwards_byte_identical() {
    let upstream = MockServer::start().await;
    let captured = mount_anthropic_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = CompressionMode::LiveZone;
    })
    .await;

    let body = unsorted_tools_payload(json!([]));
    let sent_sha = sha256_hex(&body);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "sk-ant-api03-abc")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upstream_body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        sha256_hex(&upstream_body),
        sent_sha,
        "no_messages must be byte-faithful (tool sort skipped)"
    );
    assert_eq!(tool_names(&upstream_body), vec!["zebra", "apple", "mango"]);
    proxy.shutdown().await;
}
