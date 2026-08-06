//! Integration tests for B2: tool-order stabilization.
//!
//! Boots a real Rust proxy in front of a wiremock upstream and drives two
//! turns of one session through it, so what is under test is the wiring — that
//! the stabilizer is reached on the live Anthropic path, sees the same session
//! across turns, and that its output is what actually goes on the wire.
//!
//! The scenario is the one measured on a real capture corpus: an MCP server
//! finishes its handshake mid-session and the client splices two tool
//! definitions into the middle of `tools[]`, invalidating every tool behind
//! them plus the whole system prompt and message history. B2 pushes them to the
//! tail so the divergence point lands past the cached prefix.
//!
//! Every test here sets `compression = true`, because that is what makes the
//! proxy buffer and parse the body. B2 lives on the buffered branch alongside
//! every other tool mutation; a pure-passthrough proxy forwards the client's
//! bytes untouched and stabilizes nothing.

mod common;

use common::start_proxy_with;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Records every body the upstream receives, in order.
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

fn tool_names(body: &[u8]) -> Vec<String> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is json");
    v["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name").to_string())
        .collect()
}

fn payload(tools: Value) -> Value {
    json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
    })
}

async fn post(proxy_url: &str, body: &Value) {
    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat-b2")
        .header("x-headroom-session-id", "b2-session")
        .json(body)
        .send()
        .await
        .expect("proxy responds");
}

/// The measured failure, end to end: turn two splices two tools into the middle
/// of the array; the upstream must receive them at the tail instead.
#[tokio::test]
async fn late_mcp_tools_reach_the_upstream_at_the_tail() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.cache_stable_tool_order = true;
        c.compression = true;
    })
    .await;

    let first = payload(json!([
        {"name": "Read", "input_schema": {"type": "object"}},
        {"name": "Bash", "input_schema": {"type": "object"}},
        {"name": "Write", "input_schema": {"type": "object"}}
    ]));
    post(&proxy.url(), &first).await;

    let second = payload(json!([
        {"name": "Read", "input_schema": {"type": "object"}},
        {"name": "mcp__lens__authenticate", "input_schema": {"type": "object"}},
        {"name": "Bash", "input_schema": {"type": "object"}},
        {"name": "mcp__lens__complete", "input_schema": {"type": "object"}},
        {"name": "Write", "input_schema": {"type": "object"}}
    ]));
    post(&proxy.url(), &second).await;

    let seen = bodies.lock().unwrap();
    assert_eq!(seen.len(), 2, "upstream saw both turns");
    assert_eq!(
        tool_names(&seen[0]),
        ["Read", "Bash", "Write"],
        "first turn only records the order; it must not move bytes"
    );
    assert_eq!(
        tool_names(&seen[1]),
        [
            "Read",
            "Bash",
            "Write",
            "mcp__lens__authenticate",
            "mcp__lens__complete"
        ],
        "late tools must land past the cached prefix"
    );

    // Lossless: same definitions, byte for byte, only reordered.
    let sent: Value = serde_json::from_value(second["tools"].clone()).unwrap();
    let got: Value = serde_json::from_slice::<Value>(&seen[1]).unwrap()["tools"].clone();
    let canon = |v: &Value| {
        let mut xs: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|t| serde_json::to_string(t).unwrap())
            .collect();
        xs.sort();
        xs
    };
    assert_eq!(canon(&sent), canon(&got));
}

/// With the flag off the request path is byte-for-byte what the client sent,
/// however the client orders its tools.
#[tokio::test]
async fn disabled_leaves_the_client_order_alone() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.cache_stable_tool_order = false;
        c.compression = true;
    })
    .await;

    post(
        &proxy.url(),
        &payload(json!([{"name": "Read"}, {"name": "Bash"}])),
    )
    .await;
    post(
        &proxy.url(),
        &payload(json!([{"name": "Bash"}, {"name": "Read"}])),
    )
    .await;

    let seen = bodies.lock().unwrap();
    assert_eq!(tool_names(&seen[1]), ["Bash", "Read"]);
}

/// A tool carrying a `cache_control` marker owns its position — moving it moves
/// the provider's breakpoint, which is the bust B2 exists to prevent.
#[tokio::test]
async fn a_marked_tool_pins_the_order() {
    let upstream = MockServer::start().await;
    let bodies = mount_recording_upstream(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.cache_stable_tool_order = true;
        c.compression = true;
    })
    .await;

    post(
        &proxy.url(),
        &payload(json!([{"name": "Read"}, {"name": "Bash"}])),
    )
    .await;
    post(
        &proxy.url(),
        &payload(json!([
            {"name": "Bash"},
            {"name": "Read", "cache_control": {"type": "ephemeral"}}
        ])),
    )
    .await;

    let seen = bodies.lock().unwrap();
    assert_eq!(tool_names(&seen[1]), ["Bash", "Read"]);
}
