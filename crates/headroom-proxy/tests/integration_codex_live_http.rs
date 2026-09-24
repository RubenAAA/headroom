//! Codex Live HTTP call creation (`POST /v1/live`, port of upstream `53982aef`).
//!
//! A mock stands in for `chatgpt.com/backend-api/codex/realtime/calls` via
//! `HEADROOM_CODEX_REALTIME_CALLS_URL`. Everything in this file runs
//! sequentially in one test: the override is process-global env, so
//! parallel tests must not touch it.

// Edition 2024 makes std::env::set_var and remove_var unsafe. Tests call them
// to set up config; non-test code stays free of unsafe.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

mod common;

use common::start_proxy;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One test, sequential sub-cases: the realtime-URL override is
/// process-global, so this file must never run two of these in parallel.
#[tokio::test]
async fn codex_live_http_call_creation() {
    let realtime = MockServer::start().await;
    let fallback = MockServer::start().await;

    // The realtime backend answers call creation with 201 + Location,
    // like the real endpoint. Query must carry the quicksilver intent.
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/realtime/calls"))
        .and(query_param("intent", "quicksilver"))
        .and(query_param("architecture", "avas"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id": "call_1"}))
                .insert_header("location", "/realtime/calls/call_1"),
        )
        .expect(1)
        .mount(&realtime)
        .await;
    // The generic default upstream must NOT see the live call. Asserted
    // below by inspecting received requests (a duplicate identical
    // matcher would shadow instead of complement).
    unsafe {
        std::env::set_var(
            "HEADROOM_CODEX_REALTIME_CALLS_URL",
            format!(
                "{}/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas",
                realtime.uri()
            ),
        )
    };
    let proxy = start_proxy(&fallback.uri()).await;

    // Hand-built multipart body: also proves the parser reads raw wire
    // bytes, not a cooperating client library.
    let boundary = "----livecallboundary7MA4YWxkTrZu0gW";
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n";
    let session = json!({"model": "gpt-5", "modalities": ["audio"]}).to_string();
    let multipart = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\n{sdp}\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{session}\r\n\
         --{b}--\r\n",
        b = boundary,
        sdp = sdp,
        session = session,
    );

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/live", proxy.url()))
        .header("chatgpt-account-id", "acct_42")
        .header("authorization", "Bearer chatgpt-sub-token")
        .header("x-headroom-mode", "token")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(multipart)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    // Location survives the forward — the exact header the generic
    // passthrough lost upstream.
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "/realtime/calls/call_1"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, json!({"id": "call_1"}));

    // The realtime mock also records what it received: valid JSON payload
    // with the SDP string and the parsed session object.
    let received = realtime.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(sent["sdp"], json!(sdp));
    assert_eq!(sent["session"]["model"], json!("gpt-5"));
    // Internal headers never reach the upstream...
    assert!(received[0].headers.get("x-headroom-mode").is_none());
    // ...but the caller's credential and routing hint do.
    assert!(received[0].headers.get("authorization").is_some());
    assert!(received[0].headers.get("chatgpt-account-id").is_some());
    // ...and the multipart framing is replaced by our JSON body.
    let content_type = received[0].headers.get("content-type").unwrap();
    let content_type = content_type.to_str().unwrap();
    assert!(
        content_type.contains("application/json"),
        "upstream content-type: {content_type}"
    );

    // The default upstream saw nothing: the live call never fell through.
    assert!(fallback.received_requests().await.unwrap().is_empty());

    // A non-ChatGPT-authenticated POST is not ours: it falls through to
    // the generic forwarder with its body unread.
    Mock::given(method("POST"))
        .and(path("/v1/live"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "fallback"})))
        .expect(1)
        .mount(&fallback)
        .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/live", proxy.url()))
        .header("content-type", "application/json")
        .body(r#"{"hello":"world"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, json!({"id": "fallback"}));

    // Missing fields are a 400, not a 500 and not a forward.
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/live", proxy.url()))
        .header("chatgpt-account-id", "acct_42")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(format!("--{b}--\r\n", b = boundary))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    unsafe { std::env::remove_var("HEADROOM_CODEX_REALTIME_CALLS_URL") };
}
