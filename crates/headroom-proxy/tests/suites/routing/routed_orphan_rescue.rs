//! A Spark turn that stops text-only on a message it labelled `commentary`.
//!
//! Spark tags the text that precedes a tool call `phase: commentary`. A turn
//! that ends on such a message with no call is the model stopping mid-work;
//! the proxy sends it back once with a nudge. Until 2026-10-06 the rescue
//! never reached the client: the nudge round came back as SSE and was parsed
//! as JSON, and on routed shapes the settled turn stayed in OpenAI shape, so
//! the emit tail found no content either way. Both rounds here stream, as
//! Zen does.

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

const PREAMBLE: &str = "Reading the config now.";

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn completed(id: &str) -> String {
    sse(
        "response.completed",
        json!({"type":"response.completed","response":
               {"id":id,"status":"completed",
                "usage":{"input_tokens":10,"output_tokens":5}}}),
    )
}

/// One text message carrying `phase`, then the terminal event.
fn text_turn(id: &str, text: &str, phase: &str) -> String {
    let item = json!({"type":"message","id":"msg_1","role":"assistant","phase":phase,
                      "content":[{"type":"output_text","text":text}]});
    [
        sse(
            "response.created",
            json!({"type":"response.created","response":{"id":id}}),
        ),
        sse(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":0,"item":
                   {"type":"message","id":"msg_1","role":"assistant","phase":phase,"content":[]}}),
        ),
        sse(
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        ),
        sse(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
        ),
        completed(id),
    ]
    .concat()
}

/// The nudged round: the call the preamble promised.
fn tool_turn() -> String {
    let item = json!({"type":"function_call","id":"fc_1","call_id":"call_read1",
                      "name":"Read","arguments":"{\"file_path\":\"/etc/app.toml\"}"});
    [
        sse(
            "response.created",
            json!({"type":"response.created","response":{"id":"resp_2"}}),
        ),
        sse(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":0,"item":
                   {"type":"function_call","id":"fc_1","call_id":"call_read1",
                    "name":"Read","arguments":""}}),
        ),
        sse(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
        ),
        completed("resp_2"),
    ]
    .concat()
}

/// Round one streams `PREAMBLE` labelled `first_phase`; a request carrying
/// the nudge gets `tool_turn` back, or more commentary when `nudge_calls`
/// is false.
async fn upstream(
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    first_phase: &'static str,
    nudge_calls: bool,
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
                        let payload = if !text.contains("emit the tool call") {
                            text_turn("resp_1", PREAMBLE, first_phase)
                        } else if nudge_calls {
                            tool_turn()
                        } else {
                            text_turn("resp_2", "Still reading.", "commentary")
                        };
                        let stream = futures_util::stream::iter(vec![Ok::<_, Infallible>(
                            Frame::data(Bytes::from(payload)),
                        )]);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", "text/event-stream")
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

/// One streamed turn through a routed Responses model whose client
/// declares `Read`. Returns what the client saw and every upstream body.
async fn run_turn(first_phase: &'static str, nudge_calls: bool) -> (String, Vec<String>) {
    let dir = TempDir::new().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (addr, _task) = upstream(Arc::clone(&seen), first_phase, nudge_calls).await;

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

    let body = json!({
        "model": "claude-spark-test",
        "stream": true,
        "messages": [{"role": "user", "content": "check the config"}],
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
    proxy.shutdown().await;
    (text, requests)
}

#[tokio::test]
async fn a_commentary_stop_is_rescued_with_the_promised_tool_call() {
    let (client_saw, upstream_saw) = run_turn("commentary", true).await;

    assert_eq!(upstream_saw.len(), 2, "one nudge round: {upstream_saw:?}");
    assert!(
        client_saw.contains(r#""type":"tool_use""#) && client_saw.contains(r#""name":"Read""#),
        "the client gets the call the nudge produced: {client_saw}"
    );
    assert!(
        client_saw.contains(r#""stop_reason":"tool_use""#),
        "the turn stops on the call: {client_saw}"
    );
}

#[tokio::test]
async fn a_nudge_that_stays_text_only_leaves_the_original_turn() {
    let (client_saw, upstream_saw) = run_turn("commentary", false).await;

    assert_eq!(upstream_saw.len(), 2, "one nudge round: {upstream_saw:?}");
    assert!(
        client_saw.contains(PREAMBLE),
        "the original text reaches the client: {client_saw}"
    );
    assert!(
        !client_saw.contains(r#""type":"tool_use""#),
        "no call was made: {client_saw}"
    );
}

#[tokio::test]
async fn a_final_answer_is_never_nudged() {
    let (client_saw, upstream_saw) = run_turn("final_answer", true).await;

    assert_eq!(upstream_saw.len(), 1, "no nudge round: {upstream_saw:?}");
    assert!(client_saw.contains(PREAMBLE), "{client_saw}");
}
