//! Integration tests for B1: forced 1-hour prompt-cache TTL.
//!
//! Drives real requests through a proxy in front of a wiremock upstream and
//! asserts on the bytes the upstream receives, so what is under test is the
//! wiring rather than the rewrite (which has its own unit tests).
//!
//! Like B2, every test sets `compression = true` — that is what makes the proxy
//! buffer and parse the body. A pure-passthrough proxy rewrites nothing.

mod common;

use common::start_proxy_with;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_recording_upstream(upstream: &MockServer) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = bodies.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(req.body.clone());
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(upstream)
        .await;
    bodies
}

/// A body carrying one 5m-default marker on `system` and one on the last
/// message — the shape Claude Code sends for subagent traffic.
fn payload() -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 32,
        "system": [
            {"type": "text", "text": "preamble"},
            {"type": "text", "text": "tools doc", "cache_control": {"type": "ephemeral"}}
        ],
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
            ]
        }]
    })
}

async fn post(proxy_url: &str, auth: (&str, &str), body: &Value) {
    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header(auth.0, auth.1)
        .json(body)
        .send()
        .await
        .expect("proxy responds");
}

const OAUTH: (&str, &str) = ("authorization", "Bearer sk-ant-oat-b1");
const PAYG: (&str, &str) = ("x-api-key", "sk-ant-api03-b1");

fn ttls(body: &[u8]) -> Vec<Option<String>> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is json");
    let mut out = Vec::new();
    let mut push = |cc: Option<&Value>| {
        if let Some(cc) = cc {
            out.push(cc.get("ttl").and_then(Value::as_str).map(str::to_string));
        }
    };
    for block in v["system"].as_array().into_iter().flatten() {
        push(block.get("cache_control"));
    }
    for msg in v["messages"].as_array().into_iter().flatten() {
        for block in msg["content"].as_array().into_iter().flatten() {
            push(block.get("cache_control"));
        }
    }
    out
}

/// Every marker the client sent at the 5m default must reach the upstream
/// pinned to 1h.
#[tokio::test]
async fn markers_reach_the_upstream_pinned_to_1h() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.force_1h_cache_ttl = true;
        c.compression = true;
    })
    .await;

    post(&proxy.url(), OAUTH, &payload()).await;

    let seen = bodies.lock().unwrap();
    assert_eq!(
        ttls(&seen[0]),
        vec![Some("1h".to_string()), Some("1h".to_string())],
        "both markers must be pinned"
    );
}

/// A 1h cache write costs 2x base input against 1.25x for 5m. On PAYG the
/// operator pays that in dollars, so B1 must not fire however the flag is set.
#[tokio::test]
async fn payg_is_never_rewritten() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.force_1h_cache_ttl = true;
        c.compression = true;
    })
    .await;

    post(&proxy.url(), PAYG, &payload()).await;

    let seen = bodies.lock().unwrap();
    assert_eq!(ttls(&seen[0]), vec![None, None], "PAYG must pass through");
}

/// Flag off is a byte-identical passthrough.
#[tokio::test]
async fn disabled_leaves_the_ttl_alone() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.force_1h_cache_ttl = false;
        c.compression = true;
    })
    .await;

    post(&proxy.url(), OAUTH, &payload()).await;

    let seen = bodies.lock().unwrap();
    assert_eq!(ttls(&seen[0]), vec![None, None]);
}

/// B1 changes a marker's duration, never its placement — creating a breakpoint
/// would cost a full cache write.
#[tokio::test]
async fn unmarked_blocks_do_not_gain_a_marker() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.force_1h_cache_ttl = true;
        c.compression = true;
    })
    .await;

    post(&proxy.url(), OAUTH, &payload()).await;

    let seen = bodies.lock().unwrap();
    let v: Value = serde_json::from_slice(&seen[0]).unwrap();
    assert!(
        v["system"][0].get("cache_control").is_none(),
        "an unmarked system block must stay unmarked"
    );
}
