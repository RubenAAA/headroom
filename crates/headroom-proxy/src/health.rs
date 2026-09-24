//! Health endpoints. These are intercepted by Rust and never forwarded.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::proxy::AppState;

/// Own health: 200 if the proxy process is up.
pub async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "headroom-proxy" }))
}

/// Liveness: the process answers. Orchestrators poll this for restarts.
/// Port of upstream `/livez`. Previously unmounted (auth-exempt but falling
/// through to `catch_all`, so probes were forwarded upstream); now answered
/// locally like `/healthz`.
pub async fn livez() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "headroom-proxy" }))
}

/// Kompress soft-component status for readiness reporting. Kompress is
/// explicitly excluded from overall readiness (port of upstream `d50cfabe`):
/// a deferred or unloadable model only degrades plain-text compression to
/// passthrough, so it must never fail the probe.
fn kompress_check() -> serde_json::Value {
    let status = headroom_core::transforms::live_zone::kompress_status();
    json!({
        "ready": status == headroom_core::transforms::live_zone::KompressStatus::Loaded,
        "status": status.to_string(),
        "soft": true,
    })
}

/// Readiness: the proxy serves traffic. Overall `ready` excludes the
/// Kompress soft component — only hard gates (none outstanding: reaching
/// this handler proves the listener and event loop serve) decide it.
/// Port of upstream `/readyz`.
pub async fn readyz() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "ready": true,
        "service": "headroom-proxy",
        "checks": {
            "kompress": kompress_check(),
        },
    }))
}

/// Aggregate health with config echo (backend + upstream URLs), for humans
/// and dashboards. Port of upstream `/health`. Always 200: the readiness
/// verdict lives on `/readyz`; this reports detail.
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "service": "headroom-proxy",
        "ready": true,
        "checks": {
            "kompress": kompress_check(),
        },
        "config": {
            "mode": state.config.mode,
            "upstream": state.config.upstream.as_str(),
        },
    }))
}

/// Effective rollout state of this running Rust proxy process.
pub async fn rollout_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.config.rollout.to_value())
}

/// Upstream health: GETs upstream `/healthz`. Returns 200 when reachable +
/// 2xx, 503 otherwise. The endpoint name is reserved by the proxy and is
/// not forwarded; operators must not name a real upstream route this.
pub async fn healthz_upstream(State(state): State<AppState>) -> Response {
    // Use an absolute path so upstream URLs with non-trailing-slash paths
    // (e.g. http://localhost:8788/api) resolve to /healthz, not replace the
    // last segment. Url::join("healthz") would strip "api" per RFC 3986.
    let mut url = state.config.upstream.clone();
    url.set_path("/healthz");
    url.set_query(None);
    match state.client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            (StatusCode::OK, Json(json!({"ok": true}))).into_response()
        }
        Ok(resp) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "upstream_status": resp.status().as_u16()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[tokio::test]
    async fn rollout_status_exposes_running_snapshot() {
        let state = AppState::new(Config::for_test("http://127.0.0.1:9".parse().unwrap())).unwrap();
        let expected = state.config.rollout.snapshot_digest();
        let Json(payload) = rollout_status(State(state)).await;
        assert_eq!(payload["snapshot_digest"], expected);
        assert_eq!(payload["qualification_eligible"], true);
    }

    #[tokio::test]
    async fn livez_reports_alive() {
        let response = livez().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_ready_with_kompress_soft() {
        // The load-bearing semantic (port of upstream `d50cfabe`): overall
        // readiness never depends on Kompress. The kompress block reports
        // itself; `ready` stays true regardless of its state (which is
        // global and test-order-dependent, so only shape is asserted here).
        let response = readyz().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["ready"], true);
        assert_eq!(payload["checks"]["kompress"]["soft"], true);
        assert!(payload["checks"]["kompress"]["status"].is_string());
    }

    #[tokio::test]
    async fn health_echoes_config() {
        let state = AppState::new(Config::for_test("http://127.0.0.1:9".parse().unwrap())).unwrap();
        let response = health(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["config"]["upstream"], "http://127.0.0.1:9/");
        assert_eq!(payload["config"]["mode"], crate::modes::PROXY_MODE_CACHE);
    }
}
