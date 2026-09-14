//! Buffered CCR for native `/v1/responses` streams.
//!
//! Port of the Python `buffered_stream_ccr` branch in
//! `headroom/proxy/handlers/openai.py`: a `stream: true` Responses request
//! whose tool list carries `headroom_retrieve` must not stream the retrieve
//! call to a client that cannot answer it. The proxy calls upstream buffered
//! (`stream: false`), resolves retrieval server-side, and resynthesizes SSE.
//!
//! Covered here, end to end against a hyper mock upstream:
//!
//! 1. retrieve tool + `stream: true` → upstream sees `stream: false`, the
//!    client gets `text/event-stream` carrying the incremental sequence
//!    (`response.created` … `output_text.delta` … `response.completed` …
//!    `[DONE]`).
//! 2. same + ChatGPT auth → stays streaming upstream (subscription sessions
//!    own their transcript server-side).
//! 3. no retrieve tool + `stream: true` → untouched (upstream sees
//!    `stream: true`).

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::start_proxy_with;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio_stream::wrappers::ReceiverStream;

const COMPLETED_JSON: &str = r#"{"id":"resp_test","object":"response","status":"completed","model":"gpt-5","output":[{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Hello world","annotations":[]}]}],"usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":0}}}"#;

#[derive(Clone, Default)]
struct Seen {
    body: Option<Value>,
    accept: Option<String>,
}

/// Hyper mock upstream: records the request body + Accept header, answers a
/// fixed completed Responses JSON document.
async fn json_upstream(seen: Arc<Mutex<Seen>>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<hyper::body::Incoming>| {
                            let seen = seen.clone();
                            async move {
                                let accept = req
                                    .headers()
                                    .get("accept")
                                    .and_then(|v| v.to_str().ok())
                                    .map(str::to_string);
                                let body = req.into_body().collect().await.map(|c| c.to_bytes());
                                if let Some(v) = body
                                    .ok()
                                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                                {
                                    let mut seen = seen.lock().expect("seen lock");
                                    seen.body = Some(v);
                                    seen.accept = accept;
                                }
                                // StreamBody over one frame: exercises the
                                // chunked path without timing flakes.
                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(1);
                                let _ = tx.send(Ok(Frame::data(Bytes::from(COMPLETED_JSON)))).await;
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(200)
                                        .header("content-type", "application/json")
                                        .body(StreamBody::new(ReceiverStream::new(rx)))
                                        .unwrap(),
                                )
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    (addr, task)
}

fn responses_body(stream: bool, tools: Value) -> Value {
    json!({
        "model": "gpt-5",
        "stream": stream,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "tools": tools,
    })
}

fn retrieve_tools() -> Value {
    json!([{"type": "function", "name": "headroom_retrieve"}])
}

fn other_tools() -> Value {
    json!([{"type": "function", "name": "Read"}])
}

async fn post_responses(
    proxy_base: &str,
    body: Value,
    chatgpt_auth: bool,
) -> (u16, String, String) {
    let mut req = reqwest::Client::new()
        .post(format!("{proxy_base}/v1/responses"))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    if chatgpt_auth {
        req = req.header("chatgpt-account-id", "acct-test");
    }
    let resp = req
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .expect("proxy responds");
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = resp.text().await.expect("read body");
    (status, content_type, text)
}

#[tokio::test]
async fn streaming_responses_with_retrieve_tool_is_buffered_and_resynthesized() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (addr, _upstream) = json_upstream(seen.clone()).await;
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with(&format!("http://{addr}"), move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.ctx_offload = true;
        c.ctx_store_dir = Some(store_dir);
    })
    .await;

    let (status, content_type, text) =
        post_responses(&proxy.url(), responses_body(true, retrieve_tools()), false).await;
    proxy.shutdown().await;

    // Upstream was called buffered …
    let seen = seen.lock().expect("seen lock").clone();
    let upstream_body = seen.body.expect("upstream saw a body");
    assert_eq!(
        upstream_body.get("stream"),
        Some(&json!(false)),
        "upstream must be called with stream:false, got {upstream_body}"
    );
    assert!(
        seen.accept
            .as_deref()
            .unwrap_or("")
            .contains("application/json"),
        "upstream Accept must ask for JSON, got {:?}",
        seen.accept
    );

    // … and the client still gets a stream.
    assert_eq!(status, 200);
    assert!(
        content_type.contains("text/event-stream"),
        "client must get SSE, got {content_type}"
    );
    for needle in [
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.output_text.delta",
        "Hello world",
        "response.output_item.done",
        "response.completed",
        "data: [DONE]",
    ] {
        assert!(
            text.contains(needle),
            "resynthesized SSE must contain {needle:?}\n{text}"
        );
    }
}

#[tokio::test]
async fn chatgpt_auth_stays_streaming_upstream() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (addr, _upstream) = json_upstream(seen.clone()).await;
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with(&format!("http://{addr}"), move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.ctx_offload = true;
        c.ctx_store_dir = Some(store_dir);
    })
    .await;

    let _ = post_responses(&proxy.url(), responses_body(true, retrieve_tools()), true).await;
    proxy.shutdown().await;

    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(
        seen.body.expect("upstream saw a body").get("stream"),
        Some(&json!(true)),
        "ChatGPT-OAuth sessions must stay streaming upstream"
    );
}

#[tokio::test]
async fn streaming_responses_without_retrieve_tool_is_untouched() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (addr, _upstream) = json_upstream(seen.clone()).await;
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().to_path_buf();
    let proxy = start_proxy_with(&format!("http://{addr}"), move |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.ctx_offload = true;
        c.ctx_store_dir = Some(store_dir);
    })
    .await;

    let _ = post_responses(&proxy.url(), responses_body(true, other_tools()), false).await;
    proxy.shutdown().await;

    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(
        seen.body.expect("upstream saw a body").get("stream"),
        Some(&json!(true)),
        "requests without the retrieve tool must not be buffered"
    );
}
