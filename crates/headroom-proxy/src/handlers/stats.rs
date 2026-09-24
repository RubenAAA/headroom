//! `/stats`, `/stats/reset`, and `/stats-history` handlers.
//!
//! Port of the Python `/stats` endpoint family from `headroom/proxy/server.py`
//! (~L3437-3510). The Rust version is simpler: it reads from the in-memory
//! `CostTracker` and `SavingsTracker` already held on `AppState`, with no
//! external subsystem dependencies.

use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;

use crate::proxy::AppState;

// ── /stats ──

pub async fn handle_stats(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let cost_stats = state.cost_tracker.stats();
    let savings_preview = state.savings_tracker.stats_preview(20);
    // Per-request metadata (ids, providers, models, errors) is sensitive:
    // loopback callers see recent rows; network callers only with an
    // explicit dashboard-CIDR grant plus same-origin provenance. Aggregates
    // below are served to everyone (port of upstream's `include_sensitive`
    // split). Per-row tool-schema component (upstream `server.py:4180`):
    // rows stay message-only by design (`tokens_saved` untouched); the
    // additive deferral/hook-shrink share rides alongside so readers can
    // headline without guessing.
    let can_view = crate::forwarded_headers::can_view_dashboard_metadata(
        Some(addr.ip().to_string()).as_deref(),
        &headers,
        &state.trusted_gateway_cidrs,
        &state.trusted_dashboard_client_cidrs,
    );
    let recent: Vec<serde_json::Value> = if can_view {
        state
            .request_logger
            .get_recent(100)
            .into_iter()
            .map(|entry| {
                let tool = crate::tool_schema_savings::tool_schema_saved_from_tags(&entry.tags);
                let headline = crate::tool_schema_savings::headline_tokens_saved(
                    entry.tokens_saved,
                    &entry.tags,
                );
                let mut row = serde_json::to_value(&entry).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("tool_schema_saved_tokens".to_string(), tool.into());
                    obj.insert("headline_tokens_saved".to_string(), headline.into());
                }
                row
            })
            .collect()
    } else {
        Vec::new()
    };

    // One bounded window for the display aggregations below, mirroring
    // upstream's single `get_recent(10_000)` pass over the request log.
    let window = state.request_logger.get_recent(10_000);

    // Requests per display provider. OpenAI-compatible upstreams are
    // relabeled (e.g. `OpenRouter`); display only — the stored metrics key
    // stays the internal provider (upstream issue #1533).
    let mut provider_tallies: HashMap<String, i64> = HashMap::new();
    for entry in &window {
        *provider_tallies.entry(entry.provider.clone()).or_default() += 1;
    }
    let by_provider = crate::display_provider::remap_provider_counts(
        &provider_tallies,
        Some(state.config.upstream.as_str()),
        state.config.provider_name.as_deref(),
    );

    // Tool-schema deferral savings: tool-definition tokens kept out of the
    // model's context by deferring heavy schemas until needed. Attributed to
    // Headroom only, additive to `tokens_saved` — see
    // `tool_schema_savings`. Aggregated over the same window.
    let mut tool_schema_tokens: i64 = 0;
    let mut tool_schema_requests: i64 = 0;
    for entry in &window {
        let saved = crate::tool_schema_savings::tool_schema_saved_from_tags(&entry.tags);
        if saved > 0 {
            tool_schema_tokens = tool_schema_tokens.saturating_add(saved);
            tool_schema_requests += 1;
        }
    }

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
        // The only view that crosses the boundary: bytes the proxy put on the
        // wire next to the usage Anthropic reported for those same requests.
        // Everything above measures the proxy against itself.
        "wire_verdict": state.savings_tracker.wire_verdict(),
        // Which tools cost what, and which were never called. The cost already
        // lands in cache_write_tokens; this says where it went.
        "tool_inventory": state.savings_tracker.tool_inventory_report(),
        // What the proxy itself adds to `tools` + `system`. Invisible to
        // tokens_saved, which is measured after the injection stages run.
        "proxy_overhead": state.savings_tracker.proxy_overhead_report(),
        "recent_requests": recent,
        "total_logged": state.request_logger.len(),
        // Requests per dashboard display provider over the recent window.
        "by_provider": by_provider,
        // Tool-definition tokens kept out of context by schema deferral,
        // over the recent window. Counted only when Headroom performed the
        // deferral. Estimated, not realized: the figure is what we asked
        // the provider to defer, unverifiable in `usage` — an intermediary
        // that rebuilds `tools[]` drops `defer_loading` silently.
        "tool_search": serde_json::json!({
            "tokens": tool_schema_tokens,
            "tokens_saved": tool_schema_tokens,
            "estimated": true,
            "requests": tool_schema_requests,
            "window": window.len(),
        }),
        // Per-language AST-compression pauses. Empty on a healthy install;
        // non-empty is the explanation for a savings drop in one language.
        "code_syntax_breaker": headroom_core::transforms::code_compressor::syntax_breaker_status(),
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

// ── /spark-context ──

/// Muse Spark context window, in tokens. Per Meta's model docs every
/// `muse-spark` variant (1.1–1.3, standard and contributor tiers) shares a
/// 1,048,576-token window.
pub const SPARK_CONTEXT_WINDOW: u32 = 1_048_576;

/// Latest Spark turn's context usage, for the statusline.
///
/// Claude Code sends no usable `context_window` for routed Spark models — the
/// same gap that motivates `/codex-limits` for quota — so the proxy serves
/// what it saw instead. Each Anthropic turn resends the full transcript, so
/// the last turn's `input_tokens_original` (what the client sent,
/// pre-compression) is the session's live context usage, rendered by the
/// statusline script as `spark ctx:used/window`.
///
/// Pure observer over the bounded request log; empty until a Spark turn lands.
/// Single-session heuristic: with parallel Spark sessions this reports the
/// most recent turn globally, so the script ages the snapshot out via
/// `age_seconds`.
pub async fn handle_spark_context(State(state): State<AppState>) -> Json<serde_json::Value> {
    let hit = state.request_logger.latest_matching(|e| {
        e.error.is_none() && e.input_tokens_original > 0 && e.model.to_lowercase().contains("spark")
    });
    let Some(entry) = hit else {
        return Json(serde_json::json!({"observed_at": null}));
    };
    let Ok(observed_at) = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
        .map(|dt| dt.timestamp().max(0) as u64)
    else {
        return Json(serde_json::json!({"observed_at": null}));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(observed_at);
    Json(serde_json::json!({
        "observed_at": observed_at,
        "age_seconds": now.saturating_sub(observed_at),
        "model": entry.model,
        "input_tokens": entry.input_tokens_original,
        "context_window": SPARK_CONTEXT_WINDOW,
    }))
}

// ── /stats/reset ──

/// Resettable state is operator territory: loopback callers only (invisible
/// 404 otherwise, mirroring the `/debug/*` middleware), and same-origin when
/// browser provenance headers are present. Port of upstream's
/// `Depends(_require_loopback), Depends(_require_same_origin)`.
pub async fn handle_stats_reset(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    use crate::loopback_guard::{is_loopback_host, is_loopback_host_header};

    let peer = addr.ip().to_string();
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    if !is_loopback_host(Some(peer.as_str())) || !is_loopback_host_header(host) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !crate::forwarded_headers::has_same_origin_or_no_provenance(
        host.unwrap_or_default(),
        header_str("origin"),
        header_str("referer"),
        "http",
    ) {
        return StatusCode::FORBIDDEN.into_response();
    }
    state.cost_tracker.reset_runtime();
    (StatusCode::OK, Json(serde_json::json!({"status": "reset"}))).into_response()
}

// ── /stats-lifetime ──

/// Persisted lifetime aggregates. Project names are directory-derived and
/// stay loopback/grant-only; aggregates are served to everyone. Port of
/// upstream `/stats-lifetime` (which additionally nulls a persistence error
/// field Rust does not have).
pub async fn handle_stats_lifetime(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let snap = state.savings_tracker.snapshot();
    let can_view = crate::forwarded_headers::can_view_dashboard_metadata(
        Some(addr.ip().to_string()).as_deref(),
        &headers,
        &state.trusted_gateway_cidrs,
        &state.trusted_dashboard_client_cidrs,
    );
    let mut payload = serde_json::json!({
        "lifetime": snap.get("lifetime").cloned().unwrap_or(serde_json::Value::Null),
        "storage_path": snap.get("storage_path").cloned().unwrap_or(serde_json::Value::Null),
        "schema_version": snap.get("schema_version").cloned().unwrap_or(serde_json::Value::Null),
    });
    if can_view {
        payload["projects"] = snap
            .get("projects")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }
    Json(payload)
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
