//! A buffered Anthropic turn mixing memory and client calls (#4009).
//!
//! The proxy runs the memory call itself and holds the answer for the next
//! request, since a continuation would carry the client's still-unanswered
//! call and upstream would 400. The turn that goes back to the client must
//! not show the memory call — the client declared no such tool, and echoing
//! it back unanswered would make the restore drop the held answer. The
//! second request then proves the hold side: the client's `tool_result`
//! lets the proxy put the memory pair back into the history upstream sees.

use super::common::start_proxy_with;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tempfile::TempDir;

const MODEL: &str = "defer-strip-test-model";

fn mixed_turn() -> String {
    json!({
        "id": "msg_mixed1",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": [
            {"type": "text", "text": "checking memory first"},
            {"type": "tool_use", "id": "toolu_memstrip1",
             "name": "memory_search", "input": {"query": "strip me"}},
            {"type": "tool_use", "id": "toolu_bashstrip2",
             "name": "Bash", "input": {"command": "ls"}},
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 30, "output_tokens": 12},
    })
    .to_string()
}

fn final_turn() -> String {
    json!({
        "id": "msg_final2",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": [{"type": "text", "text": "all done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 60, "output_tokens": 5},
    })
    .to_string()
}

/// First request gets the mixed turn, later ones the final text. Every
/// request body lands in `seen`.
async fn upstream(
    seen: Arc<std::sync::Mutex<Vec<String>>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let text = String::from_utf8_lossy(&body).into_owned();
                        let mut seen = seen.lock().unwrap();
                        let payload = if seen.is_empty() {
                            mixed_turn()
                        } else {
                            final_turn()
                        };
                        seen.push(text);
                        let stream = futures_util::stream::iter(vec![Ok::<_, Infallible>(
                            Frame::data(Bytes::from(payload)),
                        )]);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", "application/json")
                                .body(StreamBody::new(stream))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, task)
}

fn bash_tool() -> Value {
    json!({"name": "Bash", "description": "run a command",
           "input_schema": {"type": "object"}})
}

async fn post(proxy_url: &str, body: Value) -> String {
    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("proxy answers")
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn buffered_mixed_turn_hides_memory_calls_and_restores_the_answer() {
    let dir = TempDir::new().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (addr, _task) = upstream(Arc::clone(&seen)).await;

    let store = dir.path().to_path_buf();
    let proxy = start_proxy_with(&format!("http://{addr}"), move |config| {
        config.memory_enabled = true;
        config.memory_inject_tools = true;
        config.memory_mode = "tool".to_string();
        config.ctx_store_dir = Some(store.clone());
        config.compression = true;
        config.ctx_offload = true;
    })
    .await;
    let url = proxy.url();

    // Turn 1: the model calls memory_search and Bash together.
    let turn1 = post(
        &url,
        json!({
            "model": MODEL,
            "max_tokens": 64,
            "messages": [{"role": "user",
                          "content": "strip-mixed-turn-probe: list files"}],
            "tools": [bash_tool()],
        }),
    )
    .await;
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a mixed turn must not continue: the client call has no result yet"
    );
    let turn1_json: Value = serde_json::from_str(&turn1).expect("client turn is json");
    assert!(
        turn1.contains("\"name\":\"Bash\""),
        "the client call must survive: {turn1}"
    );
    assert_eq!(
        turn1_json.get("stop_reason").and_then(Value::as_str),
        Some("tool_use"),
        "the turn still promises the client call: {turn1}"
    );
    assert!(
        !turn1.contains("memory_search") && !turn1.contains("toolu_memstrip1"),
        "the proxy call must never reach the client: {turn1}"
    );

    // Turn 2: the client answers Bash, echoing the assistant turn verbatim —
    // exactly what it received, memory call and all (or rather, not at all).
    let echoed = turn1_json
        .get("content")
        .expect("turn carries content")
        .clone();
    let turn2 = post(
        &url,
        json!({
            "model": MODEL,
            "max_tokens": 64,
            "messages": [
                {"role": "user",
                 "content": "strip-mixed-turn-probe: list files"},
                {"role": "assistant", "content": echoed},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_bashstrip2",
                     "content": "main.rs"},
                ]},
            ],
            "tools": [bash_tool()],
        }),
    )
    .await;
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "the second turn goes upstream once");
    assert!(
        turn2.contains("all done"),
        "the client gets the final answer: {turn2}"
    );

    // The history upstream saw must carry the restored memory pair: the
    // held call put back next to its answer, no dangling `tool_use`.
    let upstream_messages: Vec<Value> = serde_json::from_str::<Value>(&requests[1])
        .expect("upstream request is json")
        .get("messages")
        .and_then(Value::as_array)
        .expect("upstream request carries messages")
        .clone();
    let mut call_ids = Vec::new();
    let mut result_ids = Vec::new();
    for m in &upstream_messages {
        for b in m
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_use") => call_ids.push(
                    b.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                ),
                Some("tool_result") => result_ids.push(
                    b.get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                ),
                _ => {}
            }
        }
    }
    for id in &["toolu_memstrip1", "toolu_bashstrip2"] {
        assert!(
            call_ids.iter().any(|c| c == id),
            "upstream must see the restored {id} call: {}",
            requests[1]
        );
        assert!(
            result_ids.iter().any(|r| r == id),
            "upstream must see the {id} result: {}",
            requests[1]
        );
    }
}
