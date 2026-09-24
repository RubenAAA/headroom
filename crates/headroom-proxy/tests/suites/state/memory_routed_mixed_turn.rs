//! Memory turns on a Responses-shaped routed model (e.g. Spark over Zen).
//!
//! Two shapes the proxy must get right:
//!
//! - A turn calling only memory tools continues upstream, and the
//!   continuation pairs every `function_call` in `input[]` with a
//!   `function_call_output` — Zen refuses the turn with 400 "No tool
//!   output found for function call" otherwise (measured 2026-09-24).
//! - A turn mixing memory calls with client calls cannot continue at
//!   all: the client's calls have no results yet. The proxy answers the
//!   memory calls in place instead, and the client's calls reach the
//!   client untouched — the same treatment the CCR mixed branch gives
//!   retrievals.

use super::common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tempfile::TempDir;

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// First-response output items: always the memory call, plus a client
/// `Read` call when `mixed`, plus a text item when `with_text`.
fn first_response_events(mixed: bool, with_text: bool) -> String {
    let mut events = vec![sse(
        "response.created",
        json!({"type":"response.created","response":{"id":"resp_1"}}),
    )];
    if with_text {
        events.push(sse(
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":0,
                   "delta":"I'll look that up."}),
        ));
        events.push(sse(
            "response.output_text.done",
            json!({"type":"response.output_text.done","item_id":"msg_1","output_index":0,
                   "text":"I'll look that up."}),
        ));
    }
    events.push(sse(
        "response.output_item.added",
        json!({"type":"response.output_item.added","output_index":0,"item":
               {"type":"function_call","id":"fc_1","call_id":"call_mem123",
                "name":"memory_search","arguments":""}}),
    ));
    events.push(sse(
        "response.function_call_arguments.delta",
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1",
               "output_index":0,"delta":"{\"query\":\"x\"}"}),
    ));
    events.push(sse(
        "response.function_call_arguments.done",
        json!({"type":"response.function_call_arguments.done","item_id":"fc_1",
               "output_index":0,"arguments":"{\"query\":\"x\"}"}),
    ));
    events.push(sse(
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":0,"item":
               {"type":"function_call","id":"fc_1","call_id":"call_mem123",
                "name":"memory_search","arguments":"{\"query\":\"x\"}"}}),
    ));
    if mixed {
        events.push(sse(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":1,"item":
                   {"type":"function_call","id":"fc_2","call_id":"call_client456",
                    "name":"Read","arguments":""}}),
        ));
        events.push(sse(
            "response.function_call_arguments.delta",
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_2",
                   "output_index":1,"delta":"{\"path\":\"f\"}"}),
        ));
        events.push(sse(
            "response.function_call_arguments.done",
            json!({"type":"response.function_call_arguments.done","item_id":"fc_2",
                   "output_index":1,"arguments":"{\"path\":\"f\"}"}),
        ));
        events.push(sse(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":1,"item":
                   {"type":"function_call","id":"fc_2","call_id":"call_client456",
                    "name":"Read","arguments":"{\"path\":\"f\"}"}}),
        ));
    }
    events.push(sse(
        "response.completed",
        json!({"type":"response.completed","response":
               {"id":"resp_1","status":"completed",
                "usage":{"input_tokens":10,"output_tokens":5}}}),
    ));
    events.concat()
}

async fn upstream(
    rounds: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    mixed: bool,
    with_text: bool,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let rounds = Arc::clone(&rounds);
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let rounds = Arc::clone(&rounds);
                    let seen = Arc::clone(&seen);
                    async move {
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let text = String::from_utf8_lossy(&body).into_owned();
                        seen.lock().unwrap().push(text.clone());
                        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        let is_continuation = parsed
                            .get("input")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter().any(|i| {
                                    i.get("type").and_then(|t| t.as_str())
                                        == Some("function_call_output")
                                })
                            })
                            .unwrap_or(false);
                        let (content_type, payload): (&str, String) = if is_continuation {
                            rounds.fetch_add(1, Ordering::SeqCst);
                            (
                                "application/json",
                                json!({
                                    "id": "resp_2", "object": "response",
                                    "status": "completed",
                                    "output": [{"type": "message", "role": "assistant",
                                        "content": [{"type": "output_text",
                                                     "text": "memory says hi"}]}],
                                    "usage": {"input_tokens": 20, "output_tokens": 9}
                                })
                                .to_string(),
                            )
                        } else {
                            ("text/event-stream", first_response_events(mixed, with_text))
                        };
                        let stream = futures_util::stream::iter(vec![Ok::<_, Infallible>(
                            Frame::data(Bytes::from(payload)),
                        )]);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", content_type)
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

async fn run_turn(mixed: bool, with_text: bool) -> (String, Vec<String>) {
    let dir = TempDir::new().unwrap();
    let rounds = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (addr, _task) = upstream(Arc::clone(&rounds), Arc::clone(&seen), mixed, with_text).await;

    let store = dir.path().to_path_buf();
    let upstream_url = format!("http://{addr}");
    let route_upstream = upstream_url.clone();
    let proxy = start_proxy_with_state(
        &upstream_url,
        move |config| {
            config.memory_enabled = true;
            config.memory_inject_tools = true;
            config.memory_mode = "tool".to_string();
            config.ctx_store_dir = Some(store.clone());
            config.compression = true;
            config.ctx_offload = true;
            config.model_routes = vec![headroom_proxy::config::ProviderRoute {
                model_prefix: "claude-spark-test".to_string(),
                prefix_match: false,
                upstream: Some(route_upstream.parse().expect("upstream url")),
                translate: true,
                cursor_agent: None,
                target_model: Some("spark-test-model".to_string()),
                auth_env: Some("none".to_string()),
            }];
        },
        |state| state,
    )
    .await;

    let body = json!({
        "model": "claude-spark-test",
        "stream": true,
        "messages": [{"role": "user", "content": "what did we learn?"}],
        "tools": [{"name": "Read", "description": "read a file",
                   "input_schema": {"type": "object"}}],
    });
    let text = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("content-type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("proxy answers")
        .text()
        .await
        .unwrap();
    let requests = seen.lock().unwrap().clone();
    (text, requests)
}

fn input_items(body: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(body).expect("upstream request is json");
    v.get("input")
        .and_then(|a| a.as_array())
        .expect("Responses request carries input[]")
        .clone()
}

#[tokio::test]
async fn pure_memory_turn_continues_with_paired_call_and_output() {
    let (client_saw, upstream_saw) = run_turn(false, true).await;

    assert_eq!(
        upstream_saw.len(),
        2,
        "a pure memory turn continues upstream"
    );
    let input = input_items(&upstream_saw[1]);
    let calls: Vec<&str> = input
        .iter()
        .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("function_call"))
        .filter_map(|i| i.get("call_id").and_then(|c| c.as_str()))
        .collect();
    let outputs: Vec<&str> = input
        .iter()
        .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("function_call_output"))
        .filter_map(|i| i.get("call_id").and_then(|c| c.as_str()))
        .collect();
    assert_eq!(calls, vec!["call_mem123"]);
    assert_eq!(outputs, calls, "every echoed call needs its output");
    // The echoed assistant text is input-shaped (plain string), never
    // `output_text`: the latter is not a valid input content part.
    assert!(
        !upstream_saw[1].contains("output_text"),
        "echo must not carry output content types: {}",
        upstream_saw[1]
    );
    assert!(
        client_saw.contains("memory says hi"),
        "the client gets the continued answer: {client_saw}"
    );
}

#[tokio::test]
async fn mixed_turn_answers_memory_in_place_without_continuing() {
    let (client_saw, upstream_saw) = run_turn(true, false).await;

    assert_eq!(
        upstream_saw.len(),
        1,
        "a mixed turn must not continue: the client call has no result yet"
    );
    // The client call reaches the client runnable, with stop_reason intact…
    assert!(
        client_saw.contains("\"name\":\"Read\""),
        "the client call must survive: {client_saw}"
    );
    assert!(
        client_saw.contains("\"stop_reason\":\"tool_use\""),
        "the turn still promises the client call: {client_saw}"
    );
    // …and the memory answer rides along as prose instead of a second,
    // doomed continuation.
    assert!(
        client_saw.contains("memory_context"),
        "the memory answer must arrive in place: {client_saw}"
    );
    // No proxy tool call leaks to a client that never declared it.
    assert!(
        !client_saw.contains("memory_search"),
        "the proxy call must never reach the client: {client_saw}"
    );
}
