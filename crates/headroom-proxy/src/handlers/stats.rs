//! `/stats`, `/stats/reset`, and `/stats-history` handlers.
//!
//! Port of the Python `/stats` endpoint family from `headroom/proxy/server.py`
//! (~L3437-3510). The Rust version is simpler: it reads from the in-memory
//! `CostTracker` and `SavingsTracker` already held on `AppState`, with no
//! external subsystem dependencies.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::proxy::AppState;

// ── /stats ──

pub async fn handle_stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cost_stats = state.cost_tracker.stats();
    let savings_preview = state.savings_tracker.stats_preview(20);
    let recent = state.request_logger.get_recent(100);

    Json(serde_json::json!({
        "cost": cost_stats,
        "persistent_savings": savings_preview,
        // Durable cache counters, and the one derived number that says whether
        // the proxy is worth running. Unlike `/cache-health`, these survive a
        // restart.
        "lifetime_metrics": state.savings_tracker.metrics_snapshot(&serde_json::json!({
            "path": state.savings_tracker.storage_path().display().to_string(),
        })),
        "savings_verdict": state.savings_tracker.savings_verdict(),
        "recent_requests": recent,
        "total_logged": state.request_logger.len(),
        "codex_rate_limits": state
            .codex_rate_limits
            .snapshot()
            .map(|s| s.to_json()),
    }))
}

// ── /codex-limits ──

/// Latest Codex quota snapshot, for the statusline.
///
/// Split out from `/stats` because the statusline polls it on every prompt and
/// `/stats` builds the whole cost/savings/recent-request payload to answer.
pub async fn handle_codex_limits(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state.codex_rate_limits.snapshot() {
        Some(snapshot) => Json(snapshot.to_json()),
        None => Json(serde_json::json!({"observed_at": null})),
    }
}

// ── /stats/reset ──

pub async fn handle_stats_reset(State(state): State<AppState>) -> Response {
    state.cost_tracker.reset_runtime();
    (StatusCode::OK, Json(serde_json::json!({"status": "reset"}))).into_response()
}

// ── /stats-history ──

#[derive(Debug, Deserialize)]
pub struct StatsHistoryParams {
    #[serde(default = "default_format")]
    format: String,
    #[serde(default = "default_series")]
    series: String,
    #[serde(default = "default_history_mode")]
    history_mode: String,
}

fn default_format() -> String {
    "json".into()
}
fn default_series() -> String {
    "history".into()
}
fn default_history_mode() -> String {
    "compact".into()
}

pub async fn handle_stats_history(
    State(state): State<AppState>,
    Query(params): Query<StatsHistoryParams>,
) -> Response {
    if params.format == "csv" {
        let csv = state.savings_tracker.export_csv(&params.series);
        let filename = format!("headroom-stats-history-{}.csv", params.series);
        let mut resp = Response::new(Body::from(csv));
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/csv; charset=utf-8"),
        );
        if let Ok(disp) =
            http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        {
            resp.headers_mut()
                .insert(http::header::CONTENT_DISPOSITION, disp);
        }
        return resp;
    }

    let history = state.savings_tracker.history_response(&params.history_mode);
    Json(history).into_response()
}
