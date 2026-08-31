//! Cross-turn de-dup folds only where the model can resolve the pointer.
//!
//! Folding rewrites a repeated span to `[↑NL same as msg M]`. On
//! `/v1/chat/completions` with `stream: true` nothing can resolve that
//! reference — the streaming arm never injects the retrieval tool, and
//! OpenAI-compatible clients never show the model numbered messages — so the
//! pointer reads as deleted content and models retry-loop on output they
//! think went missing. The Anthropic path keeps folding, because it can
//! resolve.
//!
//! Both cases send the same body and differ only in the `stream` flag, so a
//! regression that drops the gate shows up as the two assertions disagreeing.

mod common;

use common::start_proxy_with;
use serde_json::json;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_capture(upstream: &MockServer) -> Arc<Mutex<Option<Vec<u8>>>> {
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            *captured_clone.lock().unwrap() = Some(req.body.clone());
            ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
        })
        .mount(upstream)
        .await;
    captured
}

/// A span long enough to clear the de-dup floor, re-displayed verbatim by a
/// second tool call — the shape a bash agent produces with `cat` then `sed`.
fn repeated_span() -> String {
    (0..12)
        .map(|i| {
            format!(
                "    result_{i} = compute_overdraft(business_id={i}, amount={})",
                i * 100
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn payload(stream: bool) -> Vec<u8> {
    let s = repeated_span();
    let body = json!({
        "model": "gpt-4o",
        "stream": stream,
        "messages": [
            {"role": "user", "content": "fix the overdraft bug"},
            {"role": "tool", "tool_call_id": "t1",
             "content": format!("$ cat merge.py\n{s}\n# end")},
            {"role": "tool", "tool_call_id": "t2",
             "content": format!("$ sed -n 1,20p merge.py\n{s}\n# more")},
        ]
    });
    serde_json::to_vec(&body).unwrap()
}

async fn upstream_body_for(stream: bool) -> Vec<u8> {
    let upstream = MockServer::start().await;
    let captured = mount_capture(&upstream).await;
    let proxy = start_proxy_with(&upstream.uri(), |c| {
        c.compression = true;
        c.compression_mode = headroom_proxy::config::CompressionMode::LiveZone;
        c.enable_cross_turn_dedup = true;
    })
    .await;

    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url()))
        .header("content-type", "application/json")
        .body(payload(stream))
        .send()
        .await
        .unwrap();

    let got = captured.lock().unwrap().clone().expect("upstream got body");
    proxy.shutdown().await;
    got
}

#[tokio::test]
async fn a_buffered_chat_request_still_folds() {
    let got = String::from_utf8(upstream_body_for(false).await).expect("utf-8");
    assert!(
        got.contains("same as msg "),
        "the buffered path resolves pointers and must keep folding"
    );
}

#[tokio::test]
async fn a_streaming_chat_request_is_left_alone() {
    let got = String::from_utf8(upstream_body_for(true).await).expect("utf-8");
    assert!(
        !got.contains("same as msg "),
        "a folded pointer on the streaming path is unresolvable"
    );
    assert!(
        got.contains("sed -n 1,20p merge.py"),
        "the re-read must reach upstream verbatim"
    );
}
