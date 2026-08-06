//! Codex Responses WebSocket handler: frame compression, header prep,
//! x-codex handshake forwarding, teardown semantics, and the HTTP SSE→WS
//! fallback. Mirrors the load-bearing Python assertions from
//! `tests/test_openai_codex_ws_lifecycle.py`.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::start_proxy_with;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WiremockRequest, ResponseTemplate};

fn live_zone_config(c: &mut headroom_proxy::Config) {
    c.compression = true;
    c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
    c.retry_max_attempts = 1; // fast connect-failure tests
    c.retry_base_delay_ms = 1;
    c.retry_max_delay_ms = 2;
}

/// Scripted mock upstream WS server. Captures the handshake request
/// headers and every received frame; sends `events` as text frames after
/// the first client frame arrives; optionally closes afterwards.
struct MockUpstream {
    addr: SocketAddr,
    req_headers: Arc<Mutex<Option<http::HeaderMap>>>,
    frames: Arc<Mutex<Vec<Message>>>,
    peer_gone: Arc<AtomicBool>,
}

async fn spawn_mock_upstream(
    events: Vec<String>,
    close_after_events: bool,
    codex_headers: Vec<(&'static str, &'static str)>,
) -> MockUpstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req_headers: Arc<Mutex<Option<http::HeaderMap>>> = Arc::new(Mutex::new(None));
    let frames: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let peer_gone = Arc::new(AtomicBool::new(false));

    let rh = req_headers.clone();
    let fr = frames.clone();
    let pg = peer_gone.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let rh = rh.clone();
            let fr = fr.clone();
            let pg = pg.clone();
            let events = events.clone();
            let codex_headers = codex_headers.clone();
            tokio::spawn(async move {
                let callback =
                    |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     mut resp: tokio_tungstenite::tungstenite::handshake::server::Response|
                     -> Result<
                        tokio_tungstenite::tungstenite::handshake::server::Response,
                        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
                    > {
                        *rh.lock().unwrap() = Some(req.headers().clone());
                        // Echo the first requested subprotocol (as OpenAI does);
                        // tungstenite clients reject a missing echo.
                        if let Some(proto) = req
                            .headers()
                            .get("sec-websocket-protocol")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.split(',').next())
                            .map(|s| s.trim().to_string())
                        {
                            if let Ok(v) = http::HeaderValue::from_str(&proto) {
                                resp.headers_mut()
                                    .insert(http::header::SEC_WEBSOCKET_PROTOCOL, v);
                            }
                        }
                        for (k, v) in &codex_headers {
                            resp.headers_mut().append(
                                http::HeaderName::from_static(k),
                                http::HeaderValue::from_static(v),
                            );
                        }
                        Ok(resp)
                    };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    return;
                };
                let (mut sink, mut src) = ws.split();
                let mut sent_events = false;
                while let Some(msg) = src.next().await {
                    let Ok(msg) = msg else { break };
                    match msg {
                        Message::Close(_) => break,
                        Message::Ping(p) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        m => {
                            fr.lock().unwrap().push(m);
                            if !sent_events {
                                sent_events = true;
                                for e in &events {
                                    if sink.send(Message::Text(e.clone())).await.is_err() {
                                        break;
                                    }
                                }
                                if close_after_events {
                                    let _ = sink.send(Message::Close(None)).await;
                                }
                            }
                        }
                    }
                }
                pg.store(true, Ordering::SeqCst);
            });
        }
    });

    MockUpstream {
        addr,
        req_headers,
        frames,
        peer_gone,
    }
}

fn big_response_create_frame() -> String {
    let blob =
        "{\"level\": \"info\", \"msg\": \"request handled ok\", \"latency_ms\": 12}\n".repeat(400);
    json!({
        "type": "response.create",
        "response": {
            "model": "gpt-5.4-codex",
            "instructions": "You are Codex.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run tests"}]},
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"},
                {"type": "function_call_output", "call_id": "call_1", "output": blob}
            ],
            "stream": true
        }
    })
    .to_string()
}

async fn wait_for_frames(frames: &Arc<Mutex<Vec<Message>>>, n: usize) {
    for _ in 0..300 {
        if frames.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "timed out waiting for {n} upstream frames (got {})",
        frames.lock().unwrap().len()
    );
}

/// response.create frames are compressed before reaching the upstream —
/// envelope shape preserved — while other frame types pass byte-identical.
#[tokio::test]
async fn response_create_compressed_other_frames_byte_identical() {
    let upstream = spawn_mock_upstream(vec![], false, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let mut req = format!("{}/v1/responses", proxy.ws_url())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        http::HeaderValue::from_static("Bearer sk-test-payg"),
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();

    // 1. Wrapped-envelope response.create — must arrive compressed.
    let original = big_response_create_frame();
    ws.send(Message::Text(original.clone())).await.unwrap();
    wait_for_frames(&upstream.frames, 1).await;
    let got = match &upstream.frames.lock().unwrap()[0] {
        Message::Text(t) => t.clone(),
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(
        got.len() < original.len(),
        "expected compressed frame ({} bytes) to be smaller than original ({} bytes)",
        got.len(),
        original.len()
    );
    let parsed: Value = serde_json::from_str(&got).unwrap();
    assert_eq!(parsed["type"], "response.create");
    assert!(parsed["response"].is_object(), "envelope shape preserved");
    assert_eq!(parsed["response"]["model"], "gpt-5.4-codex");
    assert_eq!(parsed["response"]["input"][1]["call_id"], "call_1");

    // 2. Non-response.create JSON frame — byte-identical.
    let cancel = json!({"type": "response.cancel", "response_id": "resp_1"}).to_string();
    ws.send(Message::Text(cancel.clone())).await.unwrap();
    wait_for_frames(&upstream.frames, 2).await;
    match &upstream.frames.lock().unwrap()[1] {
        Message::Text(t) => assert_eq!(t, &cancel),
        other => panic!("expected text frame, got {other:?}"),
    }

    // 3. Non-JSON frame — byte-identical passthrough.
    let garbage = "this is not json {".to_string();
    ws.send(Message::Text(garbage.clone())).await.unwrap();
    wait_for_frames(&upstream.frames, 3).await;
    match &upstream.frames.lock().unwrap()[2] {
        Message::Text(t) => assert_eq!(t, &garbage),
        other => panic!("expected text frame, got {other:?}"),
    }

    let _ = ws.close(None).await;
    proxy.shutdown().await;
}

/// The bare (no `response` wrapper) envelope shape also compresses.
#[tokio::test]
async fn bare_envelope_compressed() {
    let upstream = spawn_mock_upstream(vec![], false, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let mut bare: Value = serde_json::from_str(&big_response_create_frame()).unwrap();
    let inner = bare["response"].take();
    let mut frame = inner;
    frame["type"] = json!("response.create");
    let original = frame.to_string();

    let mut req = format!("{}/v1/codex/responses", proxy.ws_url())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "authorization",
        http::HeaderValue::from_static("Bearer sk-test-payg"),
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(original.clone())).await.unwrap();
    wait_for_frames(&upstream.frames, 1).await;
    let got = match &upstream.frames.lock().unwrap()[0] {
        Message::Text(t) => t.clone(),
        other => panic!("expected text frame, got {other:?}"),
    };
    assert!(got.len() < original.len(), "bare envelope should compress");
    let parsed: Value = serde_json::from_str(&got).unwrap();
    assert_eq!(parsed["type"], "response.create");
    assert!(parsed.get("response").is_none(), "bare shape stays bare");

    let _ = ws.close(None).await;
    proxy.shutdown().await;
}

/// Header prep: lite header dropped, x-headroom-* stripped, OpenAI-Beta
/// merged with the required responses_websockets token, adjacent headers
/// (authorization) preserved, subprotocol forwarded upstream. And the
/// upstream's x-codex-* handshake headers appear on the client-facing 101.
#[tokio::test]
async fn header_prep_and_x_codex_forwarding() {
    let upstream = spawn_mock_upstream(
        vec![],
        false,
        vec![
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-over-limit", "false"),
            ("set-cookie", "session=abc123; Path=/"),
            ("authorization", "Bearer upstream-secret"),
        ],
    )
    .await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let mut req = format!("{}/backend-api/codex/responses", proxy.ws_url())
        .into_client_request()
        .unwrap();
    {
        let h = req.headers_mut();
        h.insert(
            "authorization",
            http::HeaderValue::from_static("Bearer sk-test-payg"),
        );
        h.insert(
            "x-openai-internal-codex-responses-lite",
            http::HeaderValue::from_static("1"),
        );
        h.insert("openai-beta", http::HeaderValue::from_static("foo=1"));
        h.insert(
            "x-headroom-tag-team",
            http::HeaderValue::from_static("alpha"),
        );
        h.insert(
            "sec-websocket-protocol",
            http::HeaderValue::from_static("codex.v1"),
        );
    }
    let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();

    // x-codex-* handshake headers forwarded onto the client 101.
    assert_eq!(
        resp.headers()
            .get("x-codex-primary-used-percent")
            .and_then(|v| v.to_str().ok()),
        Some("12.5")
    );
    assert_eq!(
        resp.headers()
            .get("x-codex-over-limit")
            .and_then(|v| v.to_str().ok()),
        Some("false")
    );
    // set-cookie from the upstream handshake forwarded to the client 101;
    // authorization must NEVER be (e2e_ws_codex_usage_headers.py parity).
    assert_eq!(
        resp.headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok()),
        Some("session=abc123; Path=/")
    );
    assert!(
        resp.headers().get("authorization").is_none(),
        "upstream authorization must not leak onto the client 101"
    );
    // Subprotocol negotiated back to the client.
    assert_eq!(
        resp.headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("codex.v1")
    );

    // Trigger the upstream connection's header capture assertions.
    ws.send(Message::Text(json!({"type": "session.update"}).to_string()))
        .await
        .unwrap();
    wait_for_frames(&upstream.frames, 1).await;

    let headers = upstream.req_headers.lock().unwrap().clone().unwrap();
    assert!(
        headers
            .get("x-openai-internal-codex-responses-lite")
            .is_none(),
        "lite header must not leak upstream"
    );
    assert!(
        headers.get("x-headroom-tag-team").is_none(),
        "internal x-headroom-* headers must be stripped"
    );
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer sk-test-payg")
    );
    let beta = headers
        .get("openai-beta")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(beta, "foo=1,responses_websockets=2026-02-06");
    assert_eq!(
        headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("codex.v1")
    );

    let _ = ws.close(None).await;
    proxy.shutdown().await;
}

/// Client disconnect tears down the upstream connection.
#[tokio::test]
async fn client_disconnect_tears_down_upstream() {
    let upstream = spawn_mock_upstream(vec![], false, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let (mut ws, _resp) =
        tokio_tungstenite::connect_async(format!("{}/v1/responses", proxy.ws_url()))
            .await
            .unwrap();
    ws.send(Message::Text(json!({"type": "session.update"}).to_string()))
        .await
        .unwrap();
    wait_for_frames(&upstream.frames, 1).await;

    drop(ws); // abrupt client disconnect

    for _ in 0..300 {
        if upstream.peer_gone.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        upstream.peer_gone.load(Ordering::SeqCst),
        "upstream connection should be torn down after client disconnect"
    );
    proxy.shutdown().await;
}

/// Upstream close tears down the client: events arrive, then a close.
#[tokio::test]
async fn upstream_close_tears_down_client() {
    let events = vec![
        json!({"type": "response.created", "response": {"id": "resp_1"}}).to_string(),
        json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "usage": {"input_tokens": 10, "output_tokens": 4,
                "input_tokens_details": {"cached_tokens": 6}}}
        })
        .to_string(),
    ];
    let upstream = spawn_mock_upstream(events.clone(), true, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let (mut ws, _resp) =
        tokio_tungstenite::connect_async(format!("{}/v1/responses", proxy.ws_url()))
            .await
            .unwrap();
    ws.send(Message::Text(big_response_create_frame()))
        .await
        .unwrap();

    // Upstream events relayed byte-equal, in order.
    let mut got_events = Vec::new();
    let mut got_close = false;
    while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        match msg {
            Ok(Message::Text(t)) => got_events.push(t),
            Ok(Message::Close(_)) => {
                got_close = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert_eq!(got_events, events, "upstream events must relay byte-equal");
    assert!(got_close, "client should see close after upstream closes");
    proxy.shutdown().await;
}

/// Upstream WS connect failure → HTTP SSE fallback: the proxy still accepts
/// the client WS, POSTs the (unwrapped, stream-forced) first frame to the
/// HTTP responses endpoint, and relays each SSE `data:` line as a WS text
/// frame. The lite header must not reach the fallback either.
#[tokio::test]
async fn upstream_connect_failure_http_fallback() {
    // Plain HTTP server: the WS upgrade to it fails, but POST works.
    let http_upstream = MockServer::start().await;
    let captured: Arc<Mutex<Option<(Value, http::HeaderMap)>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    let sse_body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\"}\n",
        "\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n",
        "\n",
        "data: [DONE]\n",
        "\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |req: &WiremockRequest| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let mut headers = http::HeaderMap::new();
            for (name, value) in req.headers.iter() {
                headers.append(name.clone(), value.clone());
            }
            *cap.lock().unwrap() = Some((body, headers));
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream")
        })
        .mount(&http_upstream)
        .await;

    let proxy = start_proxy_with(&http_upstream.uri(), live_zone_config).await;

    let mut req = format!("{}/v1/responses", proxy.ws_url())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "x-openai-internal-codex-responses-lite",
        http::HeaderValue::from_static("1"),
    );
    // Client WS still accepted despite the upstream WS connect failing.
    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(big_response_create_frame()))
        .await
        .unwrap();

    let mut got = Vec::new();
    while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        match msg {
            Ok(Message::Text(t)) => got.push(t),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    assert_eq!(
        got,
        vec![
            "{\"type\":\"response.created\"}".to_string(),
            "{\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}"
                .to_string(),
        ],
        "SSE data lines must arrive as WS text frames ([DONE] skipped)"
    );

    let (body, headers) = captured.lock().unwrap().clone().expect("fallback POST hit");
    assert!(body.get("type").is_none(), "envelope unwrapped for HTTP");
    assert_eq!(body["stream"], json!(true), "stream forced on");
    assert_eq!(body["model"], "gpt-5.4-codex");
    assert!(body["input"].is_array());
    assert!(
        headers
            .get("x-openai-internal-codex-responses-lite")
            .is_none(),
        "lite header must not reach the HTTP fallback"
    );
    proxy.shutdown().await;
}

/// A client that never sends its first frame is closed with 1001 within
/// the first-frame timeout (shortened via env for the test).
#[tokio::test]
async fn first_frame_timeout_closes_1001() {
    // Process-global env: other codex tests send their first frame
    // immediately after connect, so a 1s bound cannot misfire on them.
    std::env::set_var("HEADROOM_WS_FIRST_FRAME_TIMEOUT_SECONDS", "1");
    let upstream = spawn_mock_upstream(vec![], false, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let (mut ws, _resp) =
        tokio_tungstenite::connect_async(format!("{}/v1/responses", proxy.ws_url()))
            .await
            .unwrap();

    // Send nothing; expect Close(1001) within ~1s + margin.
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("expected close before timeout")
        .expect("stream should yield a frame")
        .expect("close frame, not protocol error");
    match msg {
        Message::Close(Some(cf)) => {
            assert_eq!(
                u16::from(cf.code),
                1001,
                "close code must be 1001 (going away)"
            );
            assert!(cf.reason.contains("first-frame timeout"));
        }
        other => panic!("expected Close(1001), got {other:?}"),
    }
    std::env::remove_var("HEADROOM_WS_FIRST_FRAME_TIMEOUT_SECONDS");
    proxy.shutdown().await;
}

/// A non-loopback Origin is refused pre-upgrade (no 101).
#[tokio::test]
async fn disallowed_origin_refused_pre_upgrade() {
    let upstream = spawn_mock_upstream(vec![], false, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let mut req = format!("{}/v1/responses", proxy.ws_url())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "origin",
        http::HeaderValue::from_static("https://evil.example.com"),
    );
    let err = tokio_tungstenite::connect_async(req)
        .await
        .expect_err("handshake must be refused");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), http::StatusCode::FORBIDDEN);
        }
        other => panic!("expected HTTP 403 refusal, got {other:?}"),
    }
    // Upstream must never have been dialed into a session for this client.
    assert!(upstream.req_headers.lock().unwrap().is_none());
    proxy.shutdown().await;
}

/// Loopback origins are allowed without any env allowlist.
#[tokio::test]
async fn loopback_origin_allowed() {
    let upstream = spawn_mock_upstream(vec![], false, vec![]).await;
    let proxy = start_proxy_with(&format!("http://{}", upstream.addr), live_zone_config).await;

    let mut req = format!("{}/v1/responses", proxy.ws_url())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "origin",
        http::HeaderValue::from_static("http://localhost:3000"),
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(json!({"type": "session.update"}).to_string()))
        .await
        .unwrap();
    wait_for_frames(&upstream.frames, 1).await;
    let _ = ws.close(None).await;
    proxy.shutdown().await;
}
