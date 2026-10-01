//! GET /spark-context: last Spark turn's context usage for the statusline.
//! Pure observer over the bounded request log; null until a Spark turn lands.

use super::common;

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

fn in_session(session: &str, mut e: RequestLogEntry) -> RequestLogEntry {
    e.tags.insert("session_id".into(), session.into());
    e
}

async fn context_for(proxy: &common::ProxyHandle, query: &str) -> serde_json::Value {
    reqwest::get(format!("{}/spark-context{query}", proxy.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn spark_context_session_filter_returns_only_that_sessions_turn() {
    let spark = "muse-spark-1.3-contributor-free";
    let proxy = seed(vec![
        in_session("sess-a", entry(spark, 149_000, None)),
        in_session("sess-b", entry(spark, 24_000, None)),
        in_session("sess-a", entry(spark, 150_000, None)),
        in_session("sess-b", entry(spark, 25_000, None)),
    ])
    .await;
    assert_eq!(
        context_for(&proxy, "?session=sess-a").await["input_tokens"],
        150_000
    );
    assert_eq!(
        context_for(&proxy, "?session=sess-b").await["input_tokens"],
        25_000
    );
    // No parameter keeps the old global behaviour: newest turn of any session.
    assert_eq!(context_for(&proxy, "").await["input_tokens"], 25_000);
    proxy.shutdown().await;
}

#[tokio::test]
async fn spark_context_session_filter_skips_subagent_turns() {
    let spark = "muse-spark-1.3-contributor-free";
    let mut sub = in_session("sess-a", entry(spark, 20_000, None));
    sub.tags.insert("agent_id".into(), "agent-1".into());
    let proxy = seed(vec![in_session("sess-a", entry(spark, 114_000, None)), sub]).await;
    // The subagent turn is newer, but it is not the session's own context.
    assert_eq!(
        context_for(&proxy, "?session=sess-a").await["input_tokens"],
        114_000
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn spark_context_session_filter_never_falls_back_to_another_session() {
    let spark = "muse-spark-1.3-contributor-free";
    let proxy = seed(vec![
        in_session("sess-a", entry(spark, 150_000, None)),
        entry(spark, 99_000, None),
    ])
    .await;
    let body = context_for(&proxy, "?session=sess-unknown").await;
    assert!(body["observed_at"].is_null());
    proxy.shutdown().await;
}
