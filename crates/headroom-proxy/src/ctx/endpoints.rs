//! CTX-5/6 — HTTP endpoints for context-mode retrieval and management.
//!
//! Axum handlers mounted at `/ctx/*` when `ctx_offload` is enabled. These give
//! the model (via the `headroom` CLI running over Bash) and operators (via curl)
//! direct access to the offload store, FTS content index, and runtime stats.
//!
//! All handlers run DB work on `tokio::task::spawn_blocking` so the async
//! runtime is never blocked by rusqlite.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::ctx::offload_store::OffloadStore;
use crate::proxy::AppState;
use headroom_core::ctx::{ContentType, SearchOpts, SortMode};

/// Build the `/ctx` sub-router. Merged into `build_app()` when `ctx_offload`
/// is enabled.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(handle_search))
        .route("/get/{hash}", get(handle_get))
        .route("/index", post(handle_index))
        .route("/fetch", post(handle_fetch))
        .route("/stats", get(handle_stats))
        .route("/doctor", get(handle_doctor))
        .route("/purge", post(handle_purge))
}

/// Helper: clone the `Arc<OffloadStore>` from `AppState`, or return 503.
fn clone_store(state: &AppState) -> Result<Arc<OffloadStore>, StatusCode> {
    state
        .ctx_offload
        .as_ref()
        .map(|r| Arc::clone(&r.store))
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// Which project's content index this request addresses.
///
/// The `headroom` CLI sends its working directory in `x-headroom-cwd`, so a
/// search run inside a project searches that project. A caller that sends
/// neither header gets the shared bucket — the same one every request used
/// before the stores were sharded.
fn request_project(headers: &axum::http::HeaderMap) -> String {
    let ctx = crate::memory::router::RequestContext {
        headers: headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|val| (k.as_str().to_lowercase(), val.to_string()))
            })
            .collect(),
        system_prompt: String::new(),
        base_user_id: String::new(),
        project_root_override: None,
    };
    crate::memory::router::ProjectResolver::resolve_project_dir(&ctx)
        .unwrap_or_else(|| super::projects::UNRESOLVED_PROJECT.to_string())
}

/// The content store for the requesting project, or 503 if it cannot be
/// opened (the registry logs why).
fn content_for(
    store: &OffloadStore,
    headers: &axum::http::HeaderMap,
) -> Result<Arc<headroom_core::ctx::CtxStore>, StatusCode> {
    store
        .content_for(&request_project(headers))
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

// ── CTX-5: /ctx/search ──

#[derive(Deserialize)]
pub struct SearchParams {
    /// Search query string (required).
    q: String,
    /// Sort mode: `relevance` (default) or `timeline`.
    sort: Option<String>,
    /// Source label filter.
    source: Option<String>,
    /// Content type filter: `code` or `prose`.
    #[serde(rename = "type")]
    content_type: Option<String>,
    /// Max results (default 10).
    limit: Option<usize>,
}

#[derive(Serialize)]
struct SearchResponse {
    hits: Vec<SearchHitJson>,
}

#[derive(Serialize)]
struct SearchHitJson {
    title: String,
    content: String,
    source: String,
    rank: f64,
    content_type: String,
    highlighted: String,
    timestamp: Option<String>,
    match_layer: String,
}

async fn handle_search(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, StatusCode> {
    let store = clone_store(&state)?;

    let sort = match params.sort.as_deref() {
        Some("timeline") => SortMode::Timeline,
        _ => SortMode::Relevance,
    };
    let content_type = match params.content_type.as_deref() {
        Some("code") => Some(ContentType::Code),
        Some("prose") => Some(ContentType::Prose),
        _ => None,
    };
    let limit = params.limit.unwrap_or(10).min(50);
    let queries = vec![params.q];

    let content = content_for(&store, &headers)?;
    let hits = tokio::task::spawn_blocking(move || {
        let opts = SearchOpts {
            limit,
            source: params.source,
            content_type,
            sort,
        };
        content.search(&queries, &opts)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // CTX-6: record search query metric.
    crate::observability::ctx_metrics::observe_search_query();

    let hits_json: Vec<SearchHitJson> = hits
        .into_iter()
        .map(|h| SearchHitJson {
            title: h.title,
            content: h.content,
            source: h.source,
            rank: h.rank,
            content_type: h.content_type,
            highlighted: h.highlighted,
            timestamp: h.timestamp,
            match_layer: h.match_layer.to_string(),
        })
        .collect();

    Ok(Json(SearchResponse { hits: hits_json }))
}

// ── CTX-5: /ctx/get/:hash ──

#[derive(Serialize)]
struct GetResponse {
    hash: String,
    content: String,
    bytes: usize,
}

async fn handle_get(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<GetResponse>, StatusCode> {
    let store = clone_store(&state)?;

    let hash_str = hash.clone();
    let ccr = store.ccr();
    let content = tokio::task::spawn_blocking(move || ccr.get(&hash_str))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // PR-J5: retrieval hit/miss counters. A miss is an information-loss
    // signal (expired/evicted offload original) — count before returning 404.
    crate::observability::ctx_metrics::observe_retrieval(content.is_some());
    let content = content.ok_or(StatusCode::NOT_FOUND)?;

    let bytes = content.len();
    Ok(Json(GetResponse {
        hash,
        content,
        bytes,
    }))
}

// ── CTX-5: /ctx/index ──

#[derive(Deserialize)]
struct IndexRequest {
    /// Label (source name) for the indexed content.
    label: String,
    /// The raw content to index.
    content: String,
    /// Optional content hash for dedup.
    content_hash: Option<String>,
}

#[derive(Serialize)]
struct IndexResponse {
    source_id: i64,
    label: String,
    chunks: usize,
}

async fn handle_index(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<IndexRequest>,
) -> Result<Json<IndexResponse>, StatusCode> {
    let store = clone_store(&state)?;

    let content = content_for(&store, &headers)?;
    let label = req.label;
    let raw = req.content;
    let opts = headroom_core::ctx::IndexOpts {
        content_hash: req.content_hash,
        plain_text_lines: Some(50),
        ..Default::default()
    };

    let summary = tokio::task::spawn_blocking(move || content.index_content(&label, &raw, &opts))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(IndexResponse {
        source_id: summary.source_id,
        label: summary.label,
        chunks: summary.total_chunks,
    }))
}

// ── CTX-5: /ctx/fetch ──

#[derive(Deserialize)]
struct FetchRequest {
    /// URL to fetch.
    url: String,
    /// Optional label for the indexed content.
    source: Option<String>,
    /// Skip cache and re-fetch.
    #[serde(default)]
    force: bool,
    /// Cache TTL in seconds (default 86400 = 24h).
    ttl: Option<u64>,
}

#[derive(Serialize)]
struct FetchResponse {
    label: String,
    chunks: usize,
    bytes: usize,
    cached: bool,
    age: Option<String>,
}

async fn handle_fetch(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<FetchRequest>,
) -> Result<Json<FetchResponse>, StatusCode> {
    let store = clone_store(&state)?;
    let content = content_for(&store, &headers)?;

    let ttl = req.ttl.map(std::time::Duration::from_secs);
    let result =
        super::fetch::fetch_and_index(&req.url, req.source.as_deref(), &content, req.force, ttl)
            .await
            .map_err(|e| {
                tracing::warn!(event = "ctx_fetch_failed", error = %e);
                StatusCode::BAD_GATEWAY
            })?;

    Ok(Json(FetchResponse {
        label: result.label,
        chunks: result.chunks,
        bytes: result.bytes,
        cached: result.cached,
        age: result.age,
    }))
}

// ── CTX-6: /ctx/stats ──

#[derive(Serialize)]
struct StatsResponse {
    offloaded_bytes: u64,
    offloaded_blocks: u64,
    /// Bytes offload took out and proactive expansion put back. Reported next
    /// to `offloaded_bytes` because the saving is the difference, not the
    /// first figure.
    proactive_expansion_bytes: u64,
    /// Provider-reported cache-creation tokens on requests that injected a
    /// proactive expansion. This shows write amplification the byte count
    /// alone cannot price.
    proactive_expansion_cache_write_tokens: u64,
    proactive_expansions: u64,
    recall_injections: u64,
    search_queries: u64,
    retrieval_hits: u64,
    retrieval_misses: u64,
    ccr_entries: usize,
}

async fn handle_stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, StatusCode> {
    let store = clone_store(&state)?;

    let ccr = store.ccr();
    let ccr_entries = tokio::task::spawn_blocking(move || ccr.len())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let registry = crate::observability::prometheus::registry();
    let offloaded_bytes = crate::observability::ctx_metrics::offloaded_bytes_get(registry);
    let offloaded_blocks = crate::observability::ctx_metrics::offloaded_blocks_get(registry);
    let proactive_expansion_bytes =
        crate::observability::ctx_metrics::proactive_expansion_bytes_get(registry);
    let proactive_expansion_cache_write_tokens =
        crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry);
    let proactive_expansions =
        crate::observability::ctx_metrics::proactive_expansions_get(registry);
    let recall_injections = crate::observability::ctx_metrics::recall_injections_get(registry);
    let search_queries = crate::observability::ctx_metrics::search_queries_get(registry);
    let retrieval_hits = crate::observability::ctx_metrics::retrieval_hits_get(registry);
    let retrieval_misses = crate::observability::ctx_metrics::retrieval_misses_get(registry);

    Ok(Json(StatsResponse {
        offloaded_bytes,
        offloaded_blocks,
        proactive_expansion_bytes,
        proactive_expansion_cache_write_tokens,
        proactive_expansions,
        recall_injections,
        search_queries,
        retrieval_hits,
        retrieval_misses,
        ccr_entries,
    }))
}

// ── CTX-6: /ctx/doctor ──

#[derive(Serialize)]
struct DoctorResponse {
    checks: Vec<DoctorCheck>,
    ok: bool,
}

#[derive(Serialize)]
struct DoctorCheck {
    name: String,
    ok: bool,
    detail: String,
}

async fn handle_doctor(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<DoctorResponse>, StatusCode> {
    let mut checks = Vec::new();

    match state.ctx_offload.as_ref() {
        Some(runtime) => {
            let store = &runtime.store;

            // Check: CCR store accessible
            let ccr = store.ccr();
            let ccr_len = tokio::task::spawn_blocking(move || ccr.len())
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            checks.push(DoctorCheck {
                name: "ccr_store".into(),
                ok: true,
                detail: format!("{ccr_len} entries"),
            });

            // Check: content DB accessible + FTS5 probe
            let content = content_for(store, &headers)?;
            let fts_ok = tokio::task::spawn_blocking(move || {
                content
                    .search(
                        &["__doctor_probe__".into()],
                        &SearchOpts {
                            limit: 1,
                            ..Default::default()
                        },
                    )
                    .is_ok()
            })
            .await
            .unwrap_or(false);
            checks.push(DoctorCheck {
                name: "fts5_content_db".into(),
                ok: fts_ok,
                detail: if fts_ok {
                    "FTS5 + trigram probe OK".into()
                } else {
                    "FTS5 probe failed — DB may be corrupt".into()
                },
            });

            // Check: content DB path
            let db_path = content_for(store, &headers)?.path().display().to_string();
            checks.push(DoctorCheck {
                name: "content_db_path".into(),
                ok: true,
                detail: db_path,
            });
        }
        None => {
            checks.push(DoctorCheck {
                name: "offload_store".into(),
                ok: false,
                detail: "ctx_offload is disabled or store failed to open".into(),
            });
        }
    }

    let ok = checks.iter().all(|c| c.ok);
    Ok(Json(DoctorResponse { checks, ok }))
}

// ── CTX-6: /ctx/purge ──

#[derive(Deserialize)]
struct PurgeRequest {
    /// `"session"` or `"project"`.
    scope: String,
    /// Must be `true` to execute. Defaults to `false` when omitted.
    #[serde(default)]
    confirm: bool,
}

#[derive(Serialize)]
struct PurgeResponse {
    purged: bool,
    scope: String,
    detail: String,
}

async fn handle_purge(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PurgeRequest>,
) -> Result<Json<PurgeResponse>, StatusCode> {
    if !req.confirm {
        return Ok(Json(PurgeResponse {
            purged: false,
            scope: req.scope,
            detail: "confirm must be true".into(),
        }));
    }

    match req.scope.as_str() {
        "session" => {
            let store = clone_store(&state)?;
            let content = content_for(&store, &headers)?;
            let detail = tokio::task::spawn_blocking(move || match content.purge_all() {
                Ok(n) => format!("purged {n} chunks from content DB"),
                Err(e) => format!("purge failed: {e}"),
            })
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(PurgeResponse {
                purged: true,
                scope: req.scope,
                detail,
            }))
        }
        "project" => {
            let store = clone_store(&state)?;
            let content_path = content_for(&store, &headers)?.path().to_path_buf();
            let ccr_path = content_path
                .parent()
                .map(|p| p.join("ccr.db"))
                .unwrap_or_default();

            let mut detail = String::new();
            for suffix in &["", "-wal", "-shm"] {
                let p = format!("{}{suffix}", content_path.display());
                match std::fs::remove_file(&p) {
                    Ok(()) => detail.push_str(&format!("removed {p}; ")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => detail.push_str(&format!("failed to remove {p}: {e}; ")),
                }
            }
            for suffix in &["", "-wal", "-shm"] {
                let p = format!("{}{suffix}", ccr_path.display());
                match std::fs::remove_file(&p) {
                    Ok(()) => detail.push_str(&format!("removed {p}; ")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => detail.push_str(&format!("failed to remove {p}: {e}; ")),
                }
            }
            Ok(Json(PurgeResponse {
                purged: true,
                scope: req.scope,
                detail,
            }))
        }
        _ => Err(StatusCode::BAD_REQUEST),
    }
}
