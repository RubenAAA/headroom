//! CTX-5/6 integration tests — /ctx/* endpoints.

mod common;

use common::start_proxy_with;
use tempfile::TempDir;

fn ctx_config(dir: &std::path::Path) -> impl FnOnce(&mut headroom_proxy::Config) + '_ {
    move |config: &mut headroom_proxy::Config| {
        config.ctx_offload = true;
        config.ctx_capture = true;
        config.ctx_store_dir = Some(dir.to_path_buf());
        config.ctx_offload_min_bytes = 100;
    }
}

#[tokio::test]
async fn search_returns_empty_when_no_content() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;
    let resp = reqwest::get(format!("{}/ctx/search?q=hello", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["hits"].as_array().unwrap().is_empty());
    proxy.shutdown().await;
}

#[tokio::test]
async fn get_returns_404_for_unknown_hash() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;
    let resp = reqwest::get(format!("{}/ctx/get/abcdef1234567890abcdef12", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    proxy.shutdown().await;
}

#[tokio::test]
async fn index_then_search_finds_content() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;
    let base = proxy.url();

    // Index some content.
    let index_body = serde_json::json!({
        "label": "test-doc",
        "content": "The quick brown fox jumps over the lazy dog. Error: disk full.",
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/ctx/index"))
        .json(&index_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let index_result: serde_json::Value = resp.json().await.unwrap();
    assert!(index_result["chunks"].as_u64().unwrap() > 0);

    // Search for a term from the indexed content.
    let resp = reqwest::get(format!("{base}/ctx/search?q=disk+full"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let search_result: serde_json::Value = resp.json().await.unwrap();
    let hits = search_result["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "should find indexed content");
    assert!(hits[0]["content"].as_str().unwrap().contains("disk full"));

    proxy.shutdown().await;
}

#[tokio::test]
async fn stats_returns_counters() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;
    let resp = reqwest::get(format!("{}/ctx/stats", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.get("offloaded_bytes").is_some());
    assert!(body.get("ccr_entries").is_some());
    proxy.shutdown().await;
}

#[tokio::test]
async fn doctor_passes_all_checks() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;
    let resp = reqwest::get(format!("{}/ctx/doctor", proxy.url()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["ok"].as_bool().unwrap(), "doctor should pass: {body}");
    let checks = body["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    for check in checks {
        assert!(
            check["ok"].as_bool().unwrap(),
            "check '{}' failed: {}",
            check["name"],
            check["detail"]
        );
    }
    proxy.shutdown().await;
}

#[tokio::test]
async fn purge_requires_confirm() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;

    // Without confirm — should not purge.
    let resp = reqwest::Client::new()
        .post(format!("{}/ctx/purge", proxy.url()))
        .json(&serde_json::json!({"scope": "session"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(!body["purged"].as_bool().unwrap());

    // With confirm — should purge.
    let resp = reqwest::Client::new()
        .post(format!("{}/ctx/purge", proxy.url()))
        .json(&serde_json::json!({"scope": "session", "confirm": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["purged"].as_bool().unwrap());

    proxy.shutdown().await;
}

#[tokio::test]
async fn purge_invalid_scope_returns_400() {
    let dir = TempDir::new().unwrap();
    let proxy = start_proxy_with("http://127.0.0.1:1", ctx_config(dir.path())).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/ctx/purge", proxy.url()))
        .json(&serde_json::json!({"scope": "invalid", "confirm": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    proxy.shutdown().await;
}

#[tokio::test]
async fn endpoints_not_available_when_offload_disabled() {
    let proxy = start_proxy_with("http://127.0.0.1:1", |_| {}).await;
    // When ctx_offload is disabled, /ctx/* routes aren't mounted —
    // requests hit the catch-all and get forwarded upstream (502 from
    // unreachable upstream).
    let resp = reqwest::get(format!("{}/ctx/search?q=test", proxy.url()))
        .await
        .unwrap();
    assert!(
        resp.status().is_server_error(),
        "expected 5xx when offload disabled, got {}",
        resp.status()
    );
    proxy.shutdown().await;
}
