//! The context-limit guard nudges the client toward compaction (#3998).
//!
//! Unit tests pin the rewriter's byte handling; these pin the wiring: the
//! guard arms on Anthropic streams, rewrites `message_start` and the final
//! delta near the wall, leaves quiet streams byte-identical, nudges buffered
//! JSON too, and learns a lower wall from a 400 for the next turn.
//!
//! Every test uses a model id no other test uses. Unknown models fall through
//! to the 128K registry default, so the trigger sits at 115_200 and the
//! report at 121_600 — and, more importantly, the suite never shares a
//! learned-limit key, so parallel runs cannot teach each other.

use super::common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use common::start_proxy_with;
use http_body_util::StreamBody;
use hyper::body::Frame;
use hyper::service::service_fn;
use hyper::{Request, Response};
use serde_json::json;

struct ScriptedResponse {
    status: u16,
    content_type: &'static str,
    frames: Vec<Vec<u8>>,
}

/// Upstream that answers the nth request with `script[n]` (last repeats).
async fn scripted_upstream(
    script: Vec<ScriptedResponse>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let script = Arc::new(script);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        let attempts = Arc::new(AtomicUsize::new(0));
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (script, attempts) = (script.clone(), attempts.clone());
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |_req: Request<hyper::body::Incoming>| {
                            let (script, attempts) = (script.clone(), attempts.clone());
                            async move {
                                let n = attempts.fetch_add(1, Ordering::SeqCst);
                                let r = &script[n.min(script.len() - 1)];
                                let (tx, rx) = tokio::sync::mpsc::channel::<
                                    Result<Frame<Bytes>, std::io::Error>,
                                >(8);
                                let frames = r.frames.clone();
                                tokio::spawn(async move {
                                    for f in frames {
                                        if tx.send(Ok(Frame::data(Bytes::from(f)))).await.is_err() {
                                            return;
                                        }
                                        tokio::time::sleep(Duration::from_millis(2)).await;
                                    }
                                });
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(r.status)
                                        .header("content-type", r.content_type)
                                        .body(StreamBody::new(
                                            tokio_stream::wrappers::ReceiverStream::new(rx),
                                        ))
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

/// A minimal Anthropic turn. `delta_input` is the cumulative input usage on
/// the final delta when the provider reports it there.
fn sse_turn(model: &str, input_tokens: u64, delta_input: Option<u64>) -> Vec<Vec<u8>> {
    let delta_usage = match delta_input {
        Some(n) => format!(r#","usage":{{"input_tokens":{n},"output_tokens":7}}"#),
        None => r#","usage":{"output_tokens":7}"#.to_string(),
    };
    vec![
        format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"model\":\"{model}\",\"usage\":{{\"input_tokens\":{input_tokens},\"output_tokens\":1}}}}}}\n\n"
        )
        .into_bytes(),
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_vec(),
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n".to_vec(),
        b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n".to_vec(),
        format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}{delta_usage}}}\n\n"
        )
        .into_bytes(),
        b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
    ]
}

async fn post_turn(proxy_url: &str, model: &str, stream: bool) -> reqwest::Response {
    let mut req = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    if stream {
        req["stream"] = json!(true);
    }
    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&req).unwrap())
        .send()
        .await
        .expect("proxy responds")
}

async fn start_quiet_proxy(upstream_addr: &SocketAddr) -> common::ProxyHandle {
    start_proxy_with(&format!("http://{upstream_addr}"), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        c.retry_enabled = false;
    })
    .await
}

#[tokio::test]
async fn stream_nudges_start_and_final_delta_near_the_wall() {
    let model = "ctx-guard-it-stream";
    // 120_000 clears the 115_200 trigger but not the 121_600 report level.
    let frames = sse_turn(model, 120_000, Some(120_000));
    let (addr, _upstream) = scripted_upstream(vec![ScriptedResponse {
        status: 200,
        content_type: "text/event-stream",
        frames,
    }])
    .await;
    let proxy = start_quiet_proxy(&addr).await;

    let resp = post_turn(&proxy.url(), model, true).await;
    assert_eq!(resp.status(), 200);
    let body = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    proxy.shutdown().await;

    assert_eq!(
        body.matches("\"input_tokens\":121600").count(),
        2,
        "message_start and the final delta should both report the wall:\n{body}"
    );
    assert!(
        !body.contains("120000"),
        "no forwarded total may leak through once nudged:\n{body}"
    );
}

#[tokio::test]
async fn stream_far_from_the_wall_is_byte_identical() {
    let model = "ctx-guard-it-quiet";
    let frames = sse_turn(model, 100, None);
    let expected: Vec<u8> = frames.concat();
    let (addr, _upstream) = scripted_upstream(vec![ScriptedResponse {
        status: 200,
        content_type: "text/event-stream",
        frames,
    }])
    .await;
    let proxy = start_quiet_proxy(&addr).await;

    let resp = post_turn(&proxy.url(), model, true).await;
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.expect("body");
    proxy.shutdown().await;

    assert_eq!(
        body.as_ref(),
        expected.as_slice(),
        "a quiet stream must pass through untouched"
    );
}

#[tokio::test]
async fn buffered_json_is_nudged_near_the_wall() {
    let model = "ctx-guard-it-buffered";
    let payload = serde_json::to_vec(&json!({
        "id": "m",
        "type": "message",
        "model": model,
        "role": "assistant",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 120_000u64, "output_tokens": 7u64}
    }))
    .unwrap();
    let (addr, _upstream) = scripted_upstream(vec![ScriptedResponse {
        status: 200,
        content_type: "application/json",
        frames: vec![payload],
    }])
    .await;
    let proxy = start_quiet_proxy(&addr).await;

    let resp = post_turn(&proxy.url(), model, false).await;
    assert_eq!(resp.status(), 200);
    let body = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    proxy.shutdown().await;

    assert!(
        body.contains("\"input_tokens\":121600"),
        "buffered usage should be nudged exactly like streamed usage:\n{body}"
    );
}

#[tokio::test]
async fn a_400_teaches_the_next_turn_a_lower_wall() {
    let model = "ctx-guard-it-learn";
    let rejection = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 200000 tokens > 100000 maximum"}}"#
        .to_vec();
    // 95_000 is quiet against the 128K default (trigger 115_200) but past
    // the learned 100K wall (trigger 90_000). The report level still comes
    // from the believed 128K: 121_600.
    let frames = sse_turn(model, 95_000, Some(95_000));
    let (addr, _upstream) = scripted_upstream(vec![
        ScriptedResponse {
            status: 400,
            content_type: "application/json",
            frames: vec![rejection.clone()],
        },
        ScriptedResponse {
            status: 200,
            content_type: "text/event-stream",
            frames,
        },
    ])
    .await;
    let proxy = start_quiet_proxy(&addr).await;
    let url = proxy.url();

    let first = post_turn(&url, model, true).await;
    assert_eq!(first.status(), 400);
    assert_eq!(
        first.bytes().await.expect("body").as_ref(),
        rejection.as_slice(),
        "the refusal itself must reach the client unchanged"
    );

    let second = post_turn(&url, model, true).await;
    assert_eq!(second.status(), 200);
    let body = String::from_utf8_lossy(&second.bytes().await.expect("body")).to_string();
    proxy.shutdown().await;

    assert_eq!(
        body.matches("\"input_tokens\":121600").count(),
        2,
        "the turn after a 400 should nudge at the learned wall:\n{body}"
    );
    assert!(
        !body.contains("95000"),
        "no forwarded total may leak:\n{body}"
    );
}
