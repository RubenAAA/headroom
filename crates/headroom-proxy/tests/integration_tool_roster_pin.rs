//! B3 (`--cache-pin-tool-roster`), end to end: a session whose client drops a
//! tool for one turn and adds a new one on the next still sends a `tools`
//! prefix the provider can keep serving from cache.

mod common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_scaffold\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":2048,\"cache_read_input_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

async fn mount_capture(upstream: &MockServer) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(req.body.clone());
            ResponseTemplate::new(200).set_body_raw(sse_body(), "text/event-stream")
        })
        .mount(upstream)
        .await;
    captured
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        // OAuth, not PAYG: the 1h TTL pin is skipped on pay-as-you-go.
        .header("authorization", "Bearer sk-ant-oat-shared-scaffolding")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .expect("proxy reachable");
    assert!(
        resp.status().is_success(),
        "proxy returned {}",
        resp.status()
    );
    let _ = resp.bytes().await;
}

fn tool(name: &str) -> Value {
    json!({"name": name, "description": format!("the {name} tool"), "input_schema": {"type": "object"}})
}

fn turn(tools: &[&str], replies: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "build a parser"})];
    for i in 0..replies {
        messages.push(json!({"role": "assistant", "content": format!("reply {i}")}));
        messages.push(json!({"role": "user", "content": format!("follow up {i}")}));
    }
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "stream": true,
        "system": "You are a careful engineer.",
        "tools": tools.iter().map(|t| tool(t)).collect::<Vec<_>>(),
        "messages": messages,
    })
}

fn forwarded_tools(body: &[u8]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    v["tools"].as_array().cloned().unwrap_or_default()
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_flapping_tool_is_put_back_and_a_new_one_goes_to_the_tail() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.cache_pin_tool_roster = true;
        c.cache_stable_tool_order = true;
    })
    .await;

    let client = reqwest::Client::new();
    post_turn(
        &client,
        &proxy.url(),
        &turn(&["Bash", "SendUserFile", "Read"], 0),
    )
    .await;
    // Turn 2: the client drops SendUserFile, as Claude Code does.
    post_turn(&client, &proxy.url(), &turn(&["Bash", "Read"], 1)).await;
    // Turn 3: it is back, and WaitForMcpServers shows up in the middle.
    post_turn(
        &client,
        &proxy.url(),
        &turn(&["Bash", "WaitForMcpServers", "SendUserFile", "Read"], 2),
    )
    .await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    let t: Vec<Vec<Value>> = bodies.iter().map(|b| forwarded_tools(b)).collect();

    assert_eq!(names(&t[0]), ["Bash", "SendUserFile", "Read"]);
    assert_eq!(t[1], t[0], "turn 2's tools must match turn 1 byte for byte");
    assert_eq!(
        names(&t[2]),
        ["Bash", "SendUserFile", "Read", "WaitForMcpServers"],
        "the new tool must not move the prefix before it"
    );
    assert_eq!(&t[2][..3], &t[0][..], "the remembered block is unchanged");

    proxy.shutdown().await;
}

#[tokio::test]
async fn off_by_default_forwards_the_roster_as_sent() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
    })
    .await;

    let client = reqwest::Client::new();
    post_turn(
        &client,
        &proxy.url(),
        &turn(&["Bash", "SendUserFile", "Read"], 0),
    )
    .await;
    post_turn(&client, &proxy.url(), &turn(&["Bash", "Read"], 1)).await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(names(&forwarded_tools(&bodies[1])), ["Bash", "Read"]);
    proxy.shutdown().await;
}
