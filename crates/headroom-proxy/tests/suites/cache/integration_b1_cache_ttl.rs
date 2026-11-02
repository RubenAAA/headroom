//! Integration tests for B1: forced 1-hour prompt-cache TTL.
//!
//! Drives real requests through a proxy in front of a wiremock upstream and
//! asserts on the bytes the upstream receives, so what is under test is the
//! wiring rather than the rewrite (which has its own unit tests).
//!
//! Like B2, every test sets `compression = true` — that is what makes the proxy
//! buffer and parse the body. A pure-passthrough proxy rewrites nothing.

use super::common;

use common::start_proxy_with;
use serde_json::{Value, json};
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

/// `payload()` as a Claude Code subagent sends it: the billing header in
/// `system[0]` carries `cc_is_subagent=true`, and no marker names a TTL.
fn subagent_payload(header: &str) -> Value {
    let mut body = payload();
    body["system"][0]["text"] = Value::String(header.to_string());
    body
}

const SUBAGENT_HEADER: &str =
    "x-anthropic-billing-header: cc_version=2.1.316.ea8; cc_entrypoint=cli; cc_is_subagent=true;";
const MAIN_HEADER: &str = "x-anthropic-billing-header: cc_version=2.1.316.ea8; cc_entrypoint=cli;";

async fn ttls_with_subagent_flag(on: bool, body: &Value) -> Vec<Option<String>> {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.force_1h_cache_ttl = true;
        c.subagent_5m_ttl = on;
        c.compression = true;
    })
    .await;
    post(&proxy.url(), OAUTH, body).await;
    let seen = bodies.lock().unwrap();
    ttls(&seen[0])
}

/// Subagents run start to finish in minutes, so an hour entry bought at
/// 2x base input mostly dies unread. With the flag their markers stay on
/// the 5-minute default.
#[tokio::test]
async fn subagent_flag_leaves_subagent_markers_on_5m() {
    let got = ttls_with_subagent_flag(true, &subagent_payload(SUBAGENT_HEADER)).await;
    assert_eq!(got, vec![None, None], "subagent markers must not be pinned");
}

/// Off by default: the same subagent body is pinned as before.
#[tokio::test]
async fn subagent_body_is_pinned_without_the_flag() {
    let got = ttls_with_subagent_flag(false, &subagent_payload(SUBAGENT_HEADER)).await;
    assert_eq!(got, vec![Some("1h".to_string()), Some("1h".to_string())]);
}

/// The main conversation's last request before compaction is also
/// unmarked. It is not a subagent, and its prefix is read again after
/// gaps of any length, so it keeps the pin.
#[tokio::test]
async fn subagent_flag_still_pins_unmarked_main_bodies() {
    let got = ttls_with_subagent_flag(true, &subagent_payload(MAIN_HEADER)).await;
    assert_eq!(got, vec![Some("1h".to_string()), Some("1h".to_string())]);
}

/// Only the billing header counts. A conversation that merely quotes the
/// field name is not a subagent.
#[tokio::test]
async fn subagent_flag_ignores_the_field_outside_the_header() {
    let mut body = subagent_payload(MAIN_HEADER);
    body["messages"][0]["content"][0]["text"] =
        Value::String("grep for cc_is_subagent=true; in the logs".to_string());
    let got = ttls_with_subagent_flag(true, &body).await;
    assert_eq!(got, vec![Some("1h".to_string()), Some("1h".to_string())]);
}
