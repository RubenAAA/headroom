//! GET /spark-context: last Spark turn's context usage for the statusline.
//! Pure observer over the bounded request log; null until a Spark turn lands.

mod common;

use common::start_proxy_with_state;
use headroom_proxy::request_logger::RequestLogEntry;

fn entry(model: &str, input_tokens_original: i64, error: Option<&str>) -> RequestLogEntry {
    RequestLogEntry {
        request_id: format!("req-{model}-{input_tokens_original}"),
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false),
        provider: "openai_responses".into(),
        model: model.into(),
        input_tokens_original,
        input_tokens_optimized: input_tokens_original,
        output_tokens: 10,
        tokens_saved: 0,
        savings_percent: 0.0,
        total_latency_ms: 1.0,
        tags: std::collections::HashMap::new(),
        cache_hit: false,
        transforms_applied: vec![],
        turn_id: None,
        error: error.map(str::to_string),
    }
}

async fn seed(models: Vec<RequestLogEntry>) -> common::ProxyHandle {
    start_proxy_with_state(
        "http://127.0.0.1:1",
        |_| {},
        |s| {
            for e in models {
                s.request_logger.log(e);
            }
            s
        },
    )
    .await
}

#[tokio::test]
async fn spark_context_returns_newest_spark_turn() {
    let proxy = seed(vec![
        entry("muse-spark-1.3-contributor-free", 1000, None),
        entry("claude-sonnet-5", 5000, None),
        entry("muse-spark-1.3-contributor-free", 223762, None),
    ])
    .await;
    let body: serde_json::Value = reqwest::get(format!("{}/spark-context", proxy.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["input_tokens"], 223762);
    assert_eq!(body["model"], "muse-spark-1.3-contributor-free");
    assert_eq!(body["context_window"], 1048576);
    assert!(body["observed_at"].as_u64().unwrap() > 0);
    assert!(body["age_seconds"].as_u64().unwrap() <= 120);
    proxy.shutdown().await;
}

#[tokio::test]
async fn spark_context_null_without_spark_turns() {
    let proxy = seed(vec![entry("claude-sonnet-5", 5000, None)]).await;
    let body: serde_json::Value = reqwest::get(format!("{}/spark-context", proxy.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["observed_at"].is_null());
    proxy.shutdown().await;
}

#[tokio::test]
async fn spark_context_null_on_empty_log() {
    let proxy = seed(vec![]).await;
    let body: serde_json::Value = reqwest::get(format!("{}/spark-context", proxy.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["observed_at"].is_null());
    proxy.shutdown().await;
}

#[tokio::test]
async fn spark_context_ignores_errored_turns() {
    let proxy = seed(vec![entry(
        "muse-spark-1.3-contributor-free",
        223762,
        Some("upstream 500"),
    )])
    .await;
    let body: serde_json::Value = reqwest::get(format!("{}/spark-context", proxy.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["observed_at"].is_null());
    proxy.shutdown().await;
}
