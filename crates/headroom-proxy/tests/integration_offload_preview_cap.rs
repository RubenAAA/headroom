//! The 512-byte preview cap applies to blocks first converted after it
//! shipped. A block recorded in the gate under the legacy, un-namespaced entry
//! keeps regenerating its legacy-budget digest — across turns and a restart —
//! because those bytes are what the provider has cached.

mod common;

use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use headroom_core::transforms::live_zone::CTX_OFFLOAD_MARKER_PREFIX;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cap\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_creation_input_tokens\":9000,\"cache_read_input_tokens\":0}}}\n\n",
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

/// Prose no compressor claims, distinct per read, so each digest takes the
/// preview fallback.
fn prose(n: usize) -> String {
    (0..80)
        .map(|i| {
            format!(
                "Read {n}, paragraph {i}: the quick brown fox jumps over the lazy dog {}.\n",
                i * n
            )
        })
        .collect()
}

/// `reads` tool loops deep; the newest tool_result is the live tail.
fn turn(reads: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "summarise these files"}
    ]})];
    for n in 1..=reads {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": format!("toolu_{n}"), "name": "Bash",
             "input": {"command": format!("cat file{n}.txt")}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{n}"), "content": prose(n)}
        ]}));
    }
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "stream": true,
        "system": [{"type": "text", "text": "you are a helpful assistant"}],
        "messages": messages
    })
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, reads: usize) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat-preview-cap")
        .header("x-headroom-session-id", "session-cap")
        .body(serde_json::to_vec(&turn(reads)).unwrap())
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

fn messages_of(body: &[u8]) -> Vec<Value> {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let v: Value = serde_json::from_slice(body).expect("upstream body is JSON");
    let mut out = v["messages"].as_array().cloned().unwrap();
    out.iter_mut().for_each(strip);
    out
}

/// The preview part of the tool_result digest at message `idx`, or `None`
/// when the block went out raw.
fn preview_at(messages: &[Value], idx: usize) -> Option<String> {
    let content = &messages[idx]["content"][0]["content"];
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items[0]["text"].as_str().unwrap_or_default().to_string(),
        _ => return None,
    };
    if !text.contains(CTX_OFFLOAD_MARKER_PREFIX) {
        return None;
    }
    let cut = text
        .find("\n…[truncated")
        .expect("digest took the preview fallback");
    Some(text[..cut].to_string())
}

fn configure(store: &std::path::Path) -> impl FnOnce(&mut headroom_proxy::config::Config) + '_ {
    move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.prefix_replay = true;
        c.ctx_offload = true;
        c.ctx_offload_min_bytes = 1000;
        c.ctx_offload_stale_messages = 0;
        c.ctx_offload_stale_window = 8;
        c.ctx_store_dir = Some(store.to_path_buf());
    }
}

#[tokio::test]
async fn a_legacy_gate_entry_keeps_its_budget_while_new_blocks_take_the_cap() {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();

    // Turn 1 converts read 1, which gives the session a gate file on disk.
    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    post_turn(&client, &proxy.url(), 1).await;
    proxy.shutdown().await;

    // Pretend read 2 was converted before the cap shipped: add its hash to the
    // persisted set as the legacy, un-namespaced entry.
    let gate_dir = store.path().join("offload-gate");
    let gate_file = std::fs::read_dir(&gate_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .next()
        .expect("turn 1 persisted the gate");
    let mut gate: Value = serde_json::from_slice(&std::fs::read(&gate_file).unwrap()).unwrap();
    let legacy_hash = headroom_core::ccr::compute_key(prose(2).as_bytes());
    gate["hashes"]
        .as_array_mut()
        .unwrap()
        .push(Value::String(legacy_hash.clone()));
    std::fs::write(&gate_file, serde_json::to_vec(&gate).unwrap()).unwrap();

    // Restart, then three more turns: read 2 arrives under the legacy entry,
    // read 3 is a first conversion, read 4 too.
    let proxy = start_proxy_with(&upstream.uri(), configure(store.path())).await;
    for reads in 2..=4 {
        post_turn(&client, &proxy.url(), reads).await;
    }
    proxy.shutdown().await;

    let bodies = captured.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4);
    let turns: Vec<Vec<Value>> = bodies.iter().map(|b| messages_of(b)).collect();

    // Read n's tool_result sits at message 2n.
    let capped = preview_at(&turns[0], 2).expect("read 1 was offloaded");
    assert_eq!(capped.len(), 512, "a first conversion takes the cap");
    let legacy = preview_at(&turns[1], 4).expect("read 2 was offloaded under its legacy entry");
    assert_eq!(
        legacy.len(),
        prose(2).len() / 4,
        "a legacy entry keeps the legacy budget after a restart"
    );
    assert!(legacy.len() > 512);
    assert_eq!(preview_at(&turns[2], 6).unwrap().len(), 512);
    assert_eq!(preview_at(&turns[3], 8).unwrap().len(), 512);

    // And the mix never rewrites a forwarded message.
    for (i, pair) in turns.windows(2).enumerate() {
        let n = pair[0].len();
        assert_eq!(
            pair[0][..n],
            pair[1][..n],
            "turn {} rewrote a forwarded message",
            i + 2
        );
    }
}
