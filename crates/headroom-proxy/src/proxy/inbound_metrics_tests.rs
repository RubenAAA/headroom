use super::*;
use crate::observability::proxy_counters;
use axum::routing::get;
use tower::ServiceExt;

/// The inbound counters are process-global, so these tests would race each
/// other's increments if they ran concurrently. An async lock, since the
/// guard is held across the request's `.await`.
async fn inbound_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Drive one request through the same middleware `build_app` installs.
async fn run_once(path: &str, handler_router: Router) -> axum::http::StatusCode {
    let app = handler_router.layer(axum::middleware::from_fn(track_inbound_request));
    let request = axum::extract::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .expect("request builds");
    app.oneshot(request)
        .await
        .expect("service responds")
        .status()
}

/// The active gauge is a balance: it must come back down once the handler
/// returns, or a long-running proxy would show ever-growing "active" load.
#[tokio::test]
async fn a_completed_request_leaves_the_active_gauge_balanced() {
    let _guard = inbound_test_lock().await;
    let before = proxy_counters::inbound_active_for_test();

    let status = run_once("/ok", Router::new().route("/ok", get(|| async { "hi" }))).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        proxy_counters::inbound_active_for_test(),
        before,
        "active gauge must return to its prior value"
    );
}

/// A handler that fails still has to decrement — otherwise errors would leak
/// the gauge upward.
#[tokio::test]
async fn a_failing_handler_still_decrements() {
    let _guard = inbound_test_lock().await;
    let before = proxy_counters::inbound_active_for_test();

    let status = run_once(
        "/boom",
        Router::new().route(
            "/boom",
            get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        ),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(proxy_counters::inbound_active_for_test(), before);
}

/// A request that matches nothing still counts — it consumed proxy work.
#[tokio::test]
async fn an_unmatched_route_is_still_counted() {
    let _guard = inbound_test_lock().await;
    let before = proxy_counters::inbound_total_for_test();

    run_once("/nope", Router::new().route("/ok", get(|| async { "hi" }))).await;

    assert_eq!(proxy_counters::inbound_total_for_test(), before + 1);
}
