//! The savings ledger files a Claude Code turn under the project its
//! `# Environment` block names (upstream `3f3cf19e`). Claude Code states its
//! working directory in a user message, not `system`, so reading `system`
//! alone booked every turn under the fallback project.

use super::common;

use std::sync::Arc;
use std::time::Duration;

use common::start_proxy_with_state;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turn_is_booked_under_the_stated_working_directory() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 50, "output_tokens": 2}
        })))
        .mount(&upstream)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("stated-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
        Some(dir.path().join("savings.json")),
        false,
    ));
    let probe = tracker.clone();
    let proxy = start_proxy_with_state(
        &upstream.uri(),
        |c| {
            // Interception is what builds the outcome that books the turn.
            c.compression = true;
            c.compression_mode = headroom_proxy::config::CompressionMode::Off;
        },
        move |mut s| {
            s.savings_tracker = tracker;
            s
        },
    )
    .await;

    let env = format!(
        "<system-reminder>\n# Environment\n - Primary working directory: {}\n</system-reminder>",
        repo.display()
    );
    let body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 8,
        "system": "You are Claude Code.",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": env},
            {"type": "text", "text": "hi"}
        ]}]
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url()))
        .header("x-api-key", "k")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;

    let mut projects = Vec::new();
    for _ in 0..100 {
        projects = probe.snapshot()["projects"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        if !projects.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        projects.iter().any(|p| p.starts_with("stated-repo")),
        "turn booked under {projects:?}"
    );
    proxy.shutdown().await;
}
