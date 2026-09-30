//! A tool-less Spark turn that calls one of the Zen gate's shadow tools.
//!
//! The gate needs `bash`, `edit`, `glob`, `grep` and `read` in the tool list,
//! and Zen takes only `tool_choice: "auto"`, so a model asked to compute
//! something sometimes calls one. The client sent no tools: without the
//! proxy's answer it got a `tool_use` for a tool it does not have and no text
//! (4 of 34 probe runs, 2026-09-30). The proxy now answers the call with "no
//! tools are available" and lets the model write its reply.

use super::common;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with_state;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tempfile::TempDir;

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Round one: the model calls `bash` and says nothing.
fn shadow_call_events() -> String {
    let item = json!({"type":"function_call","id":"fc_1","call_id":"call_shadow1",
                      "name":"bash","arguments":"{\"command\":\"echo 1\"}"});
    [
        sse(
            "response.created",
            json!({"type":"response.created","response":{"id":"resp_1"}}),
        ),
        sse(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":0,"item":
                   {"type":"function_call","id":"fc_1","call_id":"call_shadow1",
                    "name":"bash","arguments":""}}),
        ),
        sse(
            "response.function_call_arguments.done",
            json!({"type":"response.function_call_arguments.done","item_id":"fc_1",
                   "output_index":0,"arguments":"{\"command\":\"echo 1\"}"}),
        ),
        sse(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
        ),
        sse(
            "response.completed",
            json!({"type":"response.completed","response":
                   {"id":"resp_1","status":"completed",
                    "usage":{"input_tokens":10,"output_tokens":5}}}),
        ),
    ]
    .concat()
}

/// `follow_up` is what the model says once it has the "no tools" result.
async fn upstream(
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    follow_up: &'static str,
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
                        seen.lock().unwrap().push(text.clone());
                        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        let is_continuation = parsed["input"].as_array().is_some_and(|items| {
                            items
                                .iter()
                                .any(|i| i["type"].as_str() == Some("function_call_output"))
                        });
                        let (content_type, payload) = if is_continuation {
                            (
                                "application/json",
                                json!({"id": "resp_2", "object": "response",
                                    "status": "completed",
                                    "output": [{"type": "message", "role": "assistant",
                                        "content": [{"type": "output_text", "text": follow_up}]}],
                                    "usage": {"input_tokens": 20, "output_tokens": 9}})
                                .to_string(),
                            )
                        } else {
                            ("text/event-stream", shadow_call_events())
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

/// One turn through a routed Responses model. `tools` is what the client
/// sends; `None` sends no `tools` key at all.
async fn run_turn(tools: Option<Value>, stream: bool) -> (String, Vec<String>) {
    let dir = TempDir::new().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (addr, _task) = upstream(Arc::clone(&seen), "ANSWER_IN_TEXT").await;

    let store = dir.path().to_path_buf();
    let upstream_url = format!("http://{addr}");
    let route_upstream = upstream_url.clone();
    let proxy = start_proxy_with_state(
        &upstream_url,
        move |config| {
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

    let mut body = json!({
        "model": "claude-spark-test",
        "stream": stream,
        "messages": [{"role": "user", "content": "add 1 and 1"}],
    });
    if let Some(tools) = tools {
        body["tools"] = tools;
    }
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
    proxy.shutdown().await;
    (text, requests)
}

#[tokio::test]
async fn a_shadow_call_on_a_toolless_turn_is_answered_and_the_model_writes_its_reply() {
    let (client_saw, upstream_saw) = run_turn(None, true).await;

    assert_eq!(
        upstream_saw.len(),
        2,
        "the call is answered in a second round"
    );
    let second: Value = serde_json::from_str(&upstream_saw[1]).unwrap();
    let outputs: Vec<&Value> = second["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["call_id"], "call_shadow1");
    assert!(
        outputs[0]["output"]
            .as_str()
            .unwrap()
            .contains("No tools are available"),
        "the result tells the model there are no tools: {outputs:?}"
    );
    assert!(
        client_saw.contains("ANSWER_IN_TEXT"),
        "the client gets the reply: {client_saw}"
    );
    assert!(
        !client_saw.contains("\"type\":\"tool_use\""),
        "the client never sees the shadow call: {client_saw}"
    );
}

#[tokio::test]
async fn a_turn_whose_client_has_tools_is_left_to_the_client() {
    let tools = json!([{"name": "bash", "description": "run a command",
                        "input_schema": {"type": "object"}}]);
    let (client_saw, upstream_saw) = run_turn(Some(tools), true).await;

    assert_eq!(
        upstream_saw.len(),
        1,
        "the client's own tool call is not ours"
    );
    assert!(
        client_saw.contains("\"name\":\"bash\""),
        "a client that declares `bash` gets the call, not the proxy: {client_saw}"
    );
}

/// The buffered arm resolves proxy tools through `resolve_routed_proxy_tools`,
/// not the stream rewriter, so it needs its own check.
#[tokio::test]
async fn the_buffered_arm_answers_a_shadow_call_too() {
    let (client_saw, upstream_saw) = run_turn(None, false).await;

    assert_eq!(
        upstream_saw.len(),
        2,
        "the call is answered in a second round"
    );
    assert!(
        client_saw.contains("ANSWER_IN_TEXT"),
        "the client gets the reply: {client_saw}"
    );
    assert!(
        !client_saw.contains("\"type\":\"tool_use\""),
        "the client never sees the shadow call: {client_saw}"
    );
}
