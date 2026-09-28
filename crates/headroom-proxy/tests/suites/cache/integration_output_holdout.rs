//! The output-shaper holdout splits conversations, never turns.
//!
//! With `--output-shaper` on and `--output-holdout` above zero, a conversation
//! is either steered on every turn or on none. Flipping it between turns would
//! rewrite the system prompt and cost the whole cached prefix. Claude Code
//! opens every session with the same `<system-reminder>`, so the arm has to come
//! from the session id, or every session lands in one arm and there is no
//! control to compare against.
//!
//! This drives two turns of ten sessions, all opening with identical text,
//! through a real proxy and checks the wire and the labels the savings ledger
//! reads.

use super::common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use common::start_proxy_with;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SENTINEL: &str = "<headroom_output_shaping>";

fn sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_holdout\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
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

/// Turn `replies + 1` of session `session`. Every session types the same
/// opening, so only `metadata.user_id` tells them apart.
fn turn(session: usize, replies: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>\nshared project rules\n</system-reminder>"},
        {"type": "text", "text": "fix the parser"}
    ]})];
    for i in 0..replies {
        messages.push(json!({"role": "assistant", "content": format!("reply {i}")}));
        messages.push(json!({"role": "user", "content": format!("follow up {i}")}));
    }
    json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "metadata": {"user_id": json!({"session_id": format!("session-{session}")}).to_string()},
        "system": [{"type": "text", "text": "you are a helpful assistant"}],
        "messages": messages,
    })
}

async fn post_turn(client: &reqwest::Client, proxy_url: &str, body: &Value) {
    let resp = client
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-ant-oat-output-holdout")
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

#[tokio::test]
async fn holdout_splits_sessions_and_keeps_each_one_stable() {
    const SESSIONS: usize = 10;
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let store = tempfile::tempdir().unwrap();

    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.ctx_capture = true;
        c.ctx_inject = true;
        c.ctx_store_dir = Some(store.path().to_path_buf());
        c.output_shaper_enabled = true;
        c.output_holdout = 0.5;
        c.verbosity_level = 2;
    })
    .await;

    let client = reqwest::Client::new();
    for replies in 0..2 {
        for session in 0..SESSIONS {
            post_turn(&client, &proxy.url(), &turn(session, replies)).await;
        }
    }

    let bodies: Vec<Value> = captured
        .lock()
        .unwrap()
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2 * SESSIONS);

    let mut steered = 0;
    for session in 0..SESSIONS {
        let first = serde_json::to_string(&bodies[session]["system"]).unwrap();
        let second = serde_json::to_string(&bodies[SESSIONS + session]["system"]).unwrap();
        assert_eq!(
            first, second,
            "session {session} changed its system prompt between turns"
        );
        if first.contains(SENTINEL) {
            steered += 1;
        }
    }
    assert!(
        steered > 0 && steered < SESSIONS,
        "{steered} of {SESSIONS} sessions steered; both arms should be present"
    );

    // Every request carries its stratum and conversation label, the pair the
    // savings ledger needs to count conversations per arm.
    let mut rows = Vec::new();
    for _ in 0..50 {
        let stats: Value = client
            .get(format!("{}/stats", proxy.url()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        rows = stats["recent_requests"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if rows.len() >= 2 * SESSIONS {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(rows.len(), 2 * SESSIONS, "request log rows");
    let mut conversations = HashSet::new();
    let mut control = 0;
    for row in &rows {
        let labels: Vec<&str> = row["transforms_applied"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let conv = labels
            .iter()
            .find_map(|l| l.strip_prefix("output_shaper:conv:"))
            .unwrap_or_else(|| panic!("no conversation label in {labels:?}"));
        conversations.insert(conv.to_string());
        if labels
            .iter()
            .any(|l| l.starts_with("output_shaper:control:"))
        {
            control += 1;
        } else {
            assert!(
                labels
                    .iter()
                    .any(|l| l.starts_with("output_shaper:stratum:")),
                "no arm label in {labels:?}"
            );
        }
    }
    assert_eq!(conversations.len(), SESSIONS);
    assert_eq!(control, 2 * (SESSIONS - steered));

    proxy.shutdown().await;
}
