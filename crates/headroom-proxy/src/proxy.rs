//! Core reverse-proxy router and HTTP forwarding handler.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use url::Url;

use axum::body::{to_bytes, Body};
use axum::extract::{ConnectInfo, DefaultBodyLimit, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, Request, Response, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::Router;
#[cfg(test)]
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt};
#[cfg(test)]
use http_body_util::BodyExt;

use crate::cache_stabilization;
use crate::cache_stabilization::drift_detector::{
    compute_structural_hash, derive_session_key, observe_drift, ApiKind, DriftState,
};
use crate::cache_stabilization::prefix_replay::{SessionReplayStore, REPLAY_STORE_CAPACITY};
use crate::compression;
use crate::config::Config;
use crate::error::ProxyError;
use crate::headers::{build_forward_request_headers, filter_response_headers};
use crate::health::{healthz, healthz_upstream};
use crate::websocket::ws_handler;
// Phase F PR-F1: imported as `classify_auth_mode` to make the call
// site self-documenting. `AuthMode` is re-exported under the same
// path for downstream handlers that read the value back out of
// `req.extensions()` (Phase F PR-F2/F3/F4).
use headroom_core::auth_mode::{classify as classify_auth_mode, AuthMode};
use headroom_core::compression_policy::CompressionPolicy;

/// Shared state passed to every handler.
///
/// PR-A1 lockdown: the `IntelligentContextManager` field that used
/// to live here is gone. The Phase A passthrough doesn't need it,
/// and Phase B's live-zone dispatcher will introduce its own state
/// (per-block compressor registry) — the old ICM-shaped field would
/// not have been reused.
///
/// PR-D4 adds `vertex_token_source`: an `Arc<dyn TokenSource>` used
/// by the Vertex `:rawPredict` / `:streamRawPredict` handlers to
/// resolve a GCP ADC bearer token. Production wires
/// [`crate::vertex::adc::GcpAdcTokenSource`] (lazy ADC chain
/// resolution + cached tokens with refresh-ahead-of-expiry); tests
/// inject [`crate::vertex::adc::StaticTokenSource`] so they never
/// hit real GCP.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
    /// PR-D1: AWS credentials resolved at startup via the
    /// `aws-config` default chain. `None` when the proxy boots
    /// without AWS creds available (operator running locally
    /// against a non-Bedrock upstream); the Bedrock invoke handler
    /// returns 5xx with a structured `event=bedrock_credentials_missing`
    /// log so failures are LOUD — no silent fallback to unsigned
    /// requests.
    pub bedrock_credentials: Option<Arc<aws_credential_types::Credentials>>,
    /// PR-E6: per-session structural-hash LRU for the cache-bust
    /// drift detector. Bounded to 1000 sessions in production. The
    /// detector is read-only — observing it never mutates the
    /// request body — so this can be cloned freely into every handler
    /// path that buffers the body.
    pub drift_state: DriftState,
    /// B2: per-session record of the tool order last forwarded upstream,
    /// bounded to 1000 sessions. Read and written once per Anthropic
    /// request, after tools are final.
    pub tool_order_state: cache_stabilization::tool_order::ToolOrderStore,
    /// PR-D4: GCP ADC bearer-token source for Vertex routes. Default:
    /// [`crate::vertex::adc::GcpAdcTokenSource`] constructed lazily;
    /// the actual ADC chain is only resolved when the first Vertex
    /// route hits `bearer()`. Tests override via
    /// [`AppState::with_token_source`].
    pub vertex_token_source: Arc<dyn crate::vertex::TokenSource>,
    /// Freeze-replay: per-session store of the previously-forwarded
    /// (compressed) messages so the next turn can replay the prefix
    /// byte-identical (ports Python `PrefixCacheTracker`). Cloneable
    /// `Arc<Mutex<LruCache<..>>>` handle like `drift_state`; only the
    /// Anthropic buffered path reads/writes it, and only when
    /// `Config::prefix_replay` is on.
    pub replay_store: SessionReplayStore,
    /// CTX-7: re-cache watchdog. Correlates request-side conversation
    /// identity (+ PR-E6 drift dims) with the response's billed
    /// `usage` to flag prompt-cache re-writes inside the TTL window.
    /// Pure observer — never mutates bytes; snapshot served at
    /// `GET /cache-health`.
    pub usage_observer: Arc<cache_stabilization::usage_observer::UsageObserver>,
    /// Last Codex quota snapshot seen on the translate path, served at
    /// `GET /codex-limits` for the statusline. Pure observer; empty until a
    /// codex-routed turn has completed.
    pub codex_rate_limits: crate::codex_rate_limits::CodexRateLimitStore,
    /// CTX-2: passive session-capture observer. `Some` only when
    /// `config.ctx_capture` is set; otherwise capture is a no-op and no
    /// sessions DB is opened. Pure observer — never mutates or delays a
    /// request; all work runs on its own background thread.
    pub ctx_observer: Option<Arc<crate::ctx::observer::CtxObserver>>,
    /// CTX-3: tool_result offload runtime. `Some` only when
    /// `config.ctx_offload` is set. Holds the static offload config (applied on
    /// the request path, before the live-zone compressors) and the background
    /// persistence sink for offloaded originals. The transform mutates wire
    /// bytes but is a pure function of block bytes (cache-safe per I1/I2); the
    /// sink never touches wire bytes.
    pub ctx_offload: Option<CtxOffloadRuntime>,
    /// CTX-4: recall/resume injection engine. `Some` only when `config.ctx_inject`
    /// (which requires `ctx_capture`). Mutates wire bytes on the request path
    /// but replays a once-decided, timestamp-free block verbatim (I1/I4).
    pub ctx_inject: Option<Arc<crate::ctx::inject::InjectEngine>>,
    /// CCR Phase 4: multi-turn tracker for offloaded/compressed context.
    /// Present only when `ctx_offload` and `ccr_context_tracking` are both
    /// enabled, because expansion needs the CCR store owned by `ctx_offload`.
    pub ccr_context_tracker:
        Option<Arc<Mutex<headroom_core::ccr::context_tracker::ContextTracker>>>,
    /// Phase 2: cost tracker — accumulates per-model token/cache counts,
    /// enforces budgets, produces monotonic savings_usd.
    pub cost_tracker: Arc<headroom_core::cost_tracker::CostTracker>,
    /// Phase 2: durable proxy savings tracker — persists cumulative
    /// compression savings, display sessions, per-project stats, and
    /// bounded history to a JSON file.
    pub savings_tracker: Arc<headroom_core::savings_tracker::SavingsTracker>,
    /// Bounded request logger — stores recent RequestLogEntry entries for
    /// the /stats endpoint and /stats/recent_requests dashboard.
    pub request_logger: Arc<crate::request_logger::RequestLogger>,
    /// cc-switch reconciler: dynamically captured upstream URL when
    /// `HEADROOM_CC_SWITCH_RECONCILE=1`. When `Some`, overrides
    /// `config.upstream` for the main Anthropic forwarding path.
    pub dynamic_upstream: crate::cc_switch_reconciler::DynamicUpstream,
    /// WebSocket session registry for /debug/ws-sessions and relay tracking.
    pub ws_sessions: Arc<Mutex<crate::ws_session_registry::WebSocketSessionRegistry>>,
    /// Memory handler: orchestrates memory tool injection, context search,
    /// and tool call execution. `Some` only when `config.memory_enabled`.
    pub memory_handler: Option<Arc<tokio::sync::Mutex<crate::memory::handler::MemoryHandler>>>,
    /// Per-key token-bucket rate limiter. `Some` only when
    /// `config.rate_limit_enabled` is set.
    pub rate_limiter: Option<Arc<headroom_core::proxy::rate_limiter::TokenBucketRateLimiter>>,
    /// Semantic response cache. `Some` only when `config.cache_enabled`.
    /// Serves identical non-streaming requests from an in-memory LRU
    /// instead of hitting upstream.
    pub semantic_cache: Option<Arc<crate::semantic_cache::SemanticCache>>,
    /// Probe recorder for compression events. `Some` when
    /// `HEADROOM_PROBE_RECORD_DIR` is set.
    pub probe_recorder: Option<Arc<crate::probe_recorder::CompressionEventRecorder>>,
    /// Compression feedback loop for tool-result compression learning.
    pub compression_feedback: Option<Arc<crate::compression_feedback::CompressionFeedback>>,
    /// Trusted gateway CIDRs for X-Forwarded-For resolution.
    pub trusted_gateway_cidrs: Vec<crate::forwarded_headers::IpCidr>,
    /// Background compressor for deferred off-path compression jobs.
    pub background_compressor: Option<Arc<crate::background_compression::BackgroundCompressor>>,
    /// Fail-closed action for compression failures on WebSocket frames.
    pub compression_failure_action: crate::compression_failure::CompressionFailureAction,
    /// CCR batch context store — keyed by upstream batch id, holds the
    /// original (pre-compression) request messages/tools/model so batch
    /// results can be CCR-post-processed. Constructed unconditionally.
    pub batch_context_store: Arc<headroom_core::ccr::BatchContextStore>,
}

/// TTL for a stored CCR batch context (24h). Mirrors Python's
/// `BatchContextStore` default.
pub(crate) const BATCH_CONTEXT_TTL_SECS: u64 = 86_400;

/// Maximum number of concurrently tracked CCR batch contexts.
pub(crate) const BATCH_CONTEXT_MAX: usize = 10_000;

/// CTX-3 offload runtime bundled into [`AppState`].
#[derive(Clone)]
pub struct CtxOffloadRuntime {
    /// Static offload config (min-bytes threshold). Never changes mid-session.
    pub config: crate::compression::ctx_offload::CtxOffloadConfig,
    /// Background sink that persists offloaded originals to CCR + FTS.
    pub store: Arc<crate::ctx::offload_store::OffloadStore>,
    /// PR-J4: per-session monotonic offload sets gating first conversions of
    /// frozen-history blocks to drift-detector rebuild boundaries.
    pub gate: Arc<crate::compression::ctx_offload::OffloadGate>,
}

/// PR-E6: maximum number of sessions tracked by the drift detector
/// LRU. Sessions are keyed per conversation (credential + first-
/// message fingerprint), not per credential, so the working set is
/// the number of *concurrently active conversations* — 1000 keeps a
/// noisy fleet in cache for at least one full turn before the oldest
/// evicts. A burst of short one-shot conversations can cycle the LRU
/// and evict a live session between its turns; the cost is telemetry-
/// only (one repeated `cache_drift_first_request`, no lost requests).
/// Operators with larger fleets can bump this; the memory cost per
/// entry is ~250 bytes (key string + 163-byte StructuralHash + LRU
/// overhead).
const DRIFT_DETECTOR_CAPACITY: usize = 1000;

/// Maximum number of messages allowed in a request body.
/// Mirrors Python's `MAX_MESSAGE_ARRAY_LENGTH = 10_000`.
const MAX_MESSAGE_ARRAY_LENGTH: usize = 10_000;

impl AppState {
    pub fn new(config: Config) -> Result<Self, ProxyError> {
        let mut client_builder = reqwest::Client::builder()
            .connect_timeout(config.upstream_connect_timeout)
            .timeout(config.upstream_timeout)
            // Don't auto-follow redirects: pass them through verbatim.
            .redirect(reqwest::redirect::Policy::none())
            // Pool needs to be allowed to be idle for long-lived streams.
            .pool_idle_timeout(std::time::Duration::from_secs(90));
        // Provider-only HTTP proxy: scoped to this upstream client so
        // routing never leaks into the process environment (which tool
        // executions inherit). HTTP/2 is disabled when a proxy is set so
        // HTTPS provider APIs tunnel through a CONNECT proxy instead of
        // failing ALPN negotiation through it.
        if let Some(proxy_url) = config.http_proxy.as_deref() {
            let proxy = reqwest::Proxy::all(proxy_url).map_err(ProxyError::Upstream)?;
            client_builder = client_builder.proxy(proxy).http1_only();
            tracing::info!(
                event = "provider_http_proxy_configured",
                "provider upstream calls routed through HTTP proxy (HTTP/2 disabled)"
            );
        }
        // Both HTTP/1.1 and HTTP/2 negotiated via ALPN (unless a proxy
        // forced HTTP/1.1 above).
        let client = client_builder.build().map_err(ProxyError::Upstream)?;

        // PR-D4: lazy ADC token source. Provider resolution is
        // deferred to first `bearer()` call so proxy startup stays
        // cheap when no Vertex route is exercised.
        let vertex_token_source: Arc<dyn crate::vertex::TokenSource> =
            Arc::new(crate::vertex::adc::GcpAdcTokenSource::new());

        // CTX flags imply interception (Config::from_cli forces `compression`
        // on when any is set); say so once at startup so operators see why
        // bodies are being buffered.
        if config.compression && (config.ctx_capture || config.ctx_offload || config.ctx_inject) {
            tracing::info!(
                event = "ctx_interception_active",
                ctx_capture = config.ctx_capture,
                ctx_offload = config.ctx_offload,
                ctx_inject = config.ctx_inject,
                compression_mode = config.compression_mode.as_str(),
                "ctx features enabled; request interception (body buffering) active"
            );
        }

        // CTX-2: construct the passive-capture observer only when enabled.
        // A failure to open the sessions DB is logged loudly and disables
        // capture (a broken observer must never take down the proxy) — the
        // request path is unaffected either way.
        let ctx_observer = if config.ctx_capture {
            let base = config
                .ctx_store_dir
                .clone()
                .or_else(headroom_core::ctx::default_base_dir);
            match base {
                Some(dir) => match crate::ctx::observer::CtxObserver::start(&dir) {
                    Ok(obs) => Some(Arc::new(obs)),
                    Err(e) => {
                        tracing::warn!(
                            event = "ctx_observer_start_failed",
                            error = %e,
                            "CTX-2 capture disabled: could not open sessions DB"
                        );
                        None
                    }
                },
                None => {
                    tracing::warn!(
                        event = "ctx_observer_no_store_dir",
                        "CTX-2 capture enabled but no store dir and $HOME unset; disabled"
                    );
                    None
                }
            }
        } else {
            None
        };

        // CTX-3: construct the offload runtime only when enabled. Independent
        // of `ctx_capture` — offload is its own flag. A failure to open the CCR
        // / content stores is logged loudly and disables offload (a broken sink
        // must never take down the proxy). The store dir resolution mirrors the
        // observer above.
        let ctx_offload = if config.ctx_offload {
            let base = config
                .ctx_store_dir
                .clone()
                .or_else(headroom_core::ctx::default_base_dir);
            match base {
                Some(dir) => {
                    match crate::ctx::offload_store::OffloadStore::start(
                        &dir,
                        config.ctx_offload_ttl_seconds,
                    ) {
                        Ok(store) => Some(CtxOffloadRuntime {
                            config: crate::compression::ctx_offload::CtxOffloadConfig {
                                min_bytes: config.ctx_offload_min_bytes,
                            },
                            store: Arc::new(store),
                            gate: Arc::new(crate::compression::ctx_offload::OffloadGate::new(
                                DRIFT_DETECTOR_CAPACITY,
                            )),
                        }),
                        Err(e) => {
                            tracing::warn!(
                                event = "ctx_offload_start_failed",
                                error = %e,
                                "CTX-3 offload disabled: could not open CCR/content stores"
                            );
                            None
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        event = "ctx_offload_no_store_dir",
                        "CTX-3 offload enabled but no store dir and $HOME unset; disabled"
                    );
                    None
                }
            }
        } else {
            None
        };

        // CTX-4: recall/resume injection. Requires ctx_capture (the identity +
        // sessions layer) — enforce loudly, no silent dependency. Shares the
        // observer's sessions store so it reads the events/prefixes the
        // observer writes; opens its own content-store handle for BM25 recall.
        let ctx_inject = if config.ctx_inject {
            if !config.ctx_capture {
                return Err(ProxyError::Config(
                    "--ctx-inject requires --ctx-capture (the sessions/identity layer); \
                     enable ctx_capture or disable ctx_inject"
                        .to_string(),
                ));
            }
            match ctx_observer.as_ref() {
                Some(observer) => {
                    let base = config
                        .ctx_store_dir
                        .clone()
                        .or_else(headroom_core::ctx::default_base_dir);
                    match base {
                        Some(dir) => {
                            let content_path = headroom_core::ctx::content_db_path(&dir, "");
                            if let Some(parent) = content_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            match headroom_core::ctx::CtxStore::open(&content_path) {
                                Ok(content) => {
                                    Some(Arc::new(crate::ctx::inject::InjectEngine::new(
                                        observer.sessions(),
                                        content,
                                    )))
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        event = "ctx_inject_start_failed",
                                        error = %e,
                                        "CTX-4 injection disabled: could not open content store"
                                    );
                                    None
                                }
                            }
                        }
                        None => {
                            tracing::warn!(
                                event = "ctx_inject_no_store_dir",
                                "CTX-4 injection enabled but no store dir and $HOME unset; disabled"
                            );
                            None
                        }
                    }
                }
                None => {
                    // ctx_capture was on but the observer failed to open; without
                    // the sessions store injection cannot run. Log and disable.
                    tracing::warn!(
                        event = "ctx_inject_no_observer",
                        "CTX-4 injection disabled: sessions observer unavailable"
                    );
                    None
                }
            }
        } else {
            None
        };

        let ccr_context_tracker = if config.ctx_offload && config.ccr_context_tracking {
            Some(Arc::new(Mutex::new(
                headroom_core::ccr::context_tracker::ContextTracker::new(Some(
                    headroom_core::ccr::context_tracker::ContextTrackerConfig {
                        proactive_expansion: config.ccr_proactive_expansion,
                        max_proactive_expansions: config.ccr_max_proactive_expansions,
                        ..Default::default()
                    },
                )),
            )))
        } else {
            None
        };

        // Extract memory config fields before config is consumed by Arc::new.
        let memory_enabled = config.memory_enabled;
        let memory_inject_tools = config.memory_inject_tools;
        let memory_inject_context = config.memory_inject_context;
        let memory_top_k = config.memory_top_k;
        let memory_min_similarity = config.memory_min_similarity;
        let memory_mode = config.memory_mode.clone();
        let memory_use_native_tool = config.memory_use_native_tool;

        // Extract rate-limit config before config is consumed by Arc::new.
        let rate_limit_enabled = config.rate_limit_enabled;
        let rate_limit_rpm = config.rate_limit_rpm;
        let rate_limit_tpm = config.rate_limit_tpm;

        // Extract semantic cache config before config is consumed by Arc::new.
        let cache_enabled = config.cache_enabled;
        let cache_ttl_seconds = config.cache_ttl_seconds;
        let cache_max_entries = config.cache_max_entries;

        // Extract cost/budget config before config is consumed by Arc::new.
        let cost_tracking_enabled = config.cost_tracking_enabled;
        let budget_limit_usd = config.budget_limit_usd;
        let budget_period = config.budget_period.clone();

        let rate_limiter = if rate_limit_enabled {
            Some(Arc::new(
                headroom_core::proxy::rate_limiter::TokenBucketRateLimiter::new(
                    rate_limit_rpm,
                    rate_limit_tpm,
                ),
            ))
        } else {
            None
        };

        let memory_handler = if memory_enabled {
            let handler_config = crate::memory::handler::MemoryConfig {
                enabled: true,
                backend_name: "local".to_string(),
                db_path: std::env::var("HEADROOM_MEMORY_DB_PATH")
                    .unwrap_or_else(|_| "headroom_memory.db".to_string()),
                inject_tools: memory_inject_tools,
                inject_context: memory_inject_context,
                top_k: memory_top_k,
                min_similarity: memory_min_similarity,
                mode: if memory_mode == "tool" {
                    crate::memory::handler::MemoryMode::Tool
                } else {
                    crate::memory::handler::MemoryMode::AutoTail
                },
                use_native_tool: memory_use_native_tool,
                ..Default::default()
            };
            let mut handler =
                crate::memory::handler::MemoryHandler::new(handler_config, "rust-proxy");
            // Wire the in-memory backend so memory operations actually work.
            handler.set_backend(Arc::new(
                crate::memory::local_backend::LocalMemoryBackend::new(),
            ));
            tracing::info!(
                event = "memory_backend_started",
                backend = "local",
                "memory backend initialized (in-memory)"
            );
            Some(Arc::new(tokio::sync::Mutex::new(handler)))
        } else {
            None
        };

        let semantic_cache = if cache_enabled {
            let cache = Arc::new(crate::semantic_cache::SemanticCache::new(
                cache_max_entries,
                cache_ttl_seconds,
            ));
            tracing::info!(
                event = "semantic_cache_started",
                max_entries = cache_max_entries,
                ttl_seconds = cache_ttl_seconds,
                "semantic response cache enabled"
            );
            Some(cache)
        } else {
            None
        };

        Ok(Self {
            config: Arc::new(config),
            client,
            bedrock_credentials: None,
            drift_state: DriftState::new(DRIFT_DETECTOR_CAPACITY),
            tool_order_state: cache_stabilization::tool_order::ToolOrderStore::default(),
            replay_store: SessionReplayStore::new(REPLAY_STORE_CAPACITY),
            vertex_token_source,
            usage_observer: Arc::new(cache_stabilization::usage_observer::UsageObserver::new()),
            codex_rate_limits: crate::codex_rate_limits::CodexRateLimitStore::new(),
            ctx_observer,
            ctx_offload,
            ctx_inject,
            ccr_context_tracker,
            cost_tracker: Arc::new(headroom_core::cost_tracker::CostTracker::new(
                if cost_tracking_enabled {
                    budget_limit_usd
                } else {
                    None
                },
                &budget_period,
            )),
            savings_tracker: Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
                None, false,
            )),
            request_logger: Arc::new(crate::request_logger::RequestLogger::new(None)),
            dynamic_upstream: crate::cc_switch_reconciler::new_dynamic_upstream(),
            ws_sessions: Arc::new(Mutex::new(
                crate::ws_session_registry::WebSocketSessionRegistry::new(),
            )),
            memory_handler,
            rate_limiter,
            semantic_cache,
            probe_recorder: crate::probe_recorder::probe_recorder_from_env().map(Arc::new),
            compression_feedback: Some(Arc::new(
                crate::compression_feedback::CompressionFeedback::new(true),
            )),
            trusted_gateway_cidrs: std::env::var("HEADROOM_TRUSTED_GATEWAY_CIDRS")
                .ok()
                .and_then(|v| crate::forwarded_headers::load_trusted_gateway_cidrs(&v).ok())
                .unwrap_or_default(),
            background_compressor: std::env::var("HEADROOM_BACKGROUND_COMPRESSION")
                .ok()
                .map(|v| v == "1" || v.to_lowercase() == "true")
                .unwrap_or(false)
                .then(|| {
                    let min_tokens: usize =
                        std::env::var("HEADROOM_BACKGROUND_COMPRESSION_MIN_TOKENS")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(50_000);
                    let _ = min_tokens; // used by callers, not here
                    Arc::new(crate::background_compression::BackgroundCompressor::new(10))
                }),
            compression_failure_action:
                crate::compression_failure::decide_compression_failure_action(
                    std::env::var("HEADROOM_WS_FAIL_OPEN_ON_COMPRESSION_FAILURE")
                        .ok()
                        .map(|v| v == "1" || v.to_lowercase() == "true")
                        .unwrap_or(false),
                    false, // is_codex_client — resolved per-request
                    false, // is_timeout — resolved per-request
                    0,     // frame_bytes — resolved per-request
                    crate::compression_failure::oversize_threshold_bytes(
                        std::env::var("HEADROOM_WS_COMPRESSION_FAIL_THRESHOLD_BYTES")
                            .ok()
                            .as_deref(),
                    ),
                ),
            batch_context_store: Arc::new(headroom_core::ccr::BatchContextStore::new(
                std::time::Duration::from_secs(BATCH_CONTEXT_TTL_SECS),
                BATCH_CONTEXT_MAX,
            )),
        })
    }

    /// Return the effective upstream URL: the cc-switch dynamic override
    /// (when set) or the static config default.
    pub async fn effective_upstream(&self) -> Url {
        let dynamic = self.dynamic_upstream.read().await;
        dynamic
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.config.upstream.clone())
    }

    /// PR-D1: attach AWS credentials resolved out-of-band (via
    /// `aws-config`'s default chain at startup). Returns the
    /// modified state; intended to be chained off `AppState::new`.
    /// Tests that don't exercise the Bedrock route can leave
    /// credentials unset (the catch-all paths never read them).
    pub fn with_bedrock_credentials(mut self, creds: aws_credential_types::Credentials) -> Self {
        self.bedrock_credentials = Some(Arc::new(creds));
        self
    }

    /// Test helper: build an `AppState` with an explicit token source.
    /// Lets the integration tests substitute a `StaticTokenSource` so
    /// the test suite never hits real GCP.
    pub fn with_token_source(
        config: Config,
        token_source: Arc<dyn crate::vertex::TokenSource>,
    ) -> Result<Self, ProxyError> {
        let mut s = Self::new(config)?;
        s.vertex_token_source = token_source;
        Ok(s)
    }

    /// CTX-5/6: access the offload store (CCR + content DB) when available.
    /// Returns `None` when `ctx_offload` is disabled.
    pub fn ctx_store(&self) -> Option<&crate::ctx::offload_store::OffloadStore> {
        self.ctx_offload.as_ref().map(|r| r.store.as_ref())
    }
}

/// Phase 2: proxy-side implementation of [`headroom_core::request_outcome::OutcomeSink`].
/// Fans out per-request bookkeeping to the cost tracker, savings tracker,
/// and output-savings recorder.
///
/// Visible crate-wide so handlers that run outside `forward_http` (the routed
/// model translate path) book their traffic through the same funnel rather
/// than a private near-copy. A second sink was easy to let drift: the one in
/// `websocket_codex` silently omits the Prometheus families below.
pub(crate) struct ProxyOutcomeSink {
    pub(crate) cost_tracker: Arc<headroom_core::cost_tracker::CostTracker>,
    pub(crate) savings_tracker: Arc<headroom_core::savings_tracker::SavingsTracker>,
    pub(crate) request_logger: Arc<crate::request_logger::RequestLogger>,
}

impl ProxyOutcomeSink {
    /// Build a sink from the shared trackers on [`AppState`].
    pub(crate) fn from_state(state: &AppState) -> Self {
        Self {
            cost_tracker: state.cost_tracker.clone(),
            savings_tracker: state.savings_tracker.clone(),
            request_logger: state.request_logger.clone(),
        }
    }
}

impl headroom_core::request_outcome::OutcomeSink for ProxyOutcomeSink {
    fn record_request(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let rec = headroom_core::savings_tracker::RequestRecord {
            model: &outcome.model,
            input_tokens: outcome.original_tokens,
            tokens_saved: outcome.tokens_saved,
            provider: Some(&outcome.provider),
            project: outcome.project.as_deref(),
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            uncached_input_tokens: outcome.uncached_input_tokens,
            total_input_tokens: None,
            total_input_cost_usd: None,
            timestamp: None,
            // Read-only counterfactual estimate; `record_output_savings` did
            // the ledger mutation, so the two compose without double-counting.
            output_tokens_saved: headroom_core::output_savings::get_recorder()
                .estimate_request_savings(&outcome.transforms_applied, outcome.output_tokens),
        };
        self.savings_tracker.record_request(&rec);

        // Provider-cache Prometheus families. Same place Python emits them
        // (step 1 of the outcome funnel, inside `metrics.record_request`), so
        // every handler that reaches an outcome reports cache usage exactly
        // once.
        crate::observability::proxy_counters::record_provider_cache_observation(
            &outcome.provider,
            &outcome.model,
            outcome.cache_read_tokens.max(0) as u64,
            outcome.cache_write_tokens.max(0) as u64,
            outcome.cache_write_5m_tokens.max(0) as u64,
            outcome.cache_write_1h_tokens.max(0) as u64,
            outcome.uncached_input_tokens.max(0) as u64,
        );

        // Request-level families: counts, tokens, and the latency/overhead/ttfb
        // histograms plus their min/max gauges.
        //
        // `input_tokens` prefers `attempted_input_tokens` (the provider's own
        // usage count, which is what Python reports) and falls back to
        // `original_tokens`. What keeps this counter alive in passthrough is
        // the first branch: the outcome sites set `attempted_input_tokens`
        // straight from the provider's usage block. The fallback cannot rescue
        // a passthrough request — `original_tokens` is only populated when
        // compression runs, so there it is 0 as well.
        let input_tokens = if outcome.attempted_input_tokens > 0 {
            outcome.attempted_input_tokens
        } else {
            outcome.original_tokens
        };
        crate::observability::proxy_counters::record_request(
            &outcome.provider,
            &outcome.model,
            input_tokens.max(0) as u64,
            outcome.output_tokens.max(0) as u64,
            outcome.tokens_saved.max(0) as u64,
            outcome.total_latency_ms,
            outcome.cache_hit(),
            outcome.overhead_ms,
            outcome.ttfb_ms,
        );

        if let Some(signals) = &outcome.waste_signals {
            for (signal, tokens) in signals {
                if *tokens > 0 {
                    crate::observability::proxy_counters::record_waste_signal_tokens(
                        signal,
                        *tokens as u64,
                    );
                }
            }
        }
    }

    fn record_tokens(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let rec = headroom_core::cost_tracker::TokenRecord {
            tokens_saved: outcome.tokens_saved,
            tokens_sent: outcome.original_tokens,
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            cache_write_5m_tokens: outcome.cache_write_5m_tokens,
            cache_write_1h_tokens: outcome.cache_write_1h_tokens,
            uncached_tokens: outcome.uncached_input_tokens,
            output_tokens: outcome.output_tokens,
        };
        self.cost_tracker.record_tokens(&outcome.model, &rec);
    }

    fn log_request(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let entry = crate::request_logger::RequestLogEntry::from_outcome(outcome);
        self.request_logger.log(entry);
    }

    fn record_output_savings(&self, transforms: &[String], output_tokens: i64) {
        let recorder = headroom_core::output_savings::get_recorder();
        recorder.record_from_labels(transforms, output_tokens);
    }

    fn record_failed(&self, _outcome: &headroom_core::request_outcome::RequestOutcome) {
        crate::observability::proxy_counters::record_failed();
    }

    fn record_savings_ledger(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        // `optimized_tokens` is what we forwarded; the helper reconstructs the
        // pre-compression original. Offloaded to a blocking thread because the
        // append takes a cross-process flock — the same reason Python wraps it
        // in `asyncio.to_thread`. Fire-and-forget: a ledger write must never
        // delay or fail a served request.
        let forwarded = outcome.optimized_tokens;
        let saved = outcome.tokens_saved;
        let model = outcome.model.clone();
        let client = outcome.client.clone();
        tokio::task::spawn_blocking(move || {
            headroom_core::savings_ledger::record_from_forwarded(
                forwarded,
                saved,
                Some(&model),
                client.as_deref(),
            );
        });
    }
}

/// Build the axum app. `/healthz` and `/healthz/upstream` are intercepted;
/// everything else hits the catch-all forwarder. WebSocket upgrades are
/// handled inside the catch-all handler when an `Upgrade: websocket` header
/// is present.
pub fn build_app(state: AppState) -> Router {
    // Point `headroom-core`'s Kompress size gate at the Prometheus counter.
    // The core crate has no dependency on this one, so it reports through a
    // hook instead. First call wins; a second `build_app` (as in tests) is a
    // harmless no-op.
    headroom_core::transforms::observability::set_kompress_size_gate_hook(Box::new(|outcome| {
        crate::observability::proxy_counters::record_kompress_size_gate(outcome);
    }));

    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/upstream", get(healthz_upstream))
        // PR-D3: Prometheus scrape endpoint. Renders the global
        // registry in text format. The handler is stateless — no
        // `AppState` needed — and idempotent across concurrent
        // scrapes (`prometheus`'s registry uses internal locking).
        // Mounted unconditionally because it has no dependencies on
        // any feature flag; an operator who doesn't want it scraped
        // simply firewalls the path.
        .route("/metrics", get(crate::observability::handle_metrics))
        // CTX-7: re-cache watchdog snapshot. Cheap (one in-memory
        // snapshot, no I/O) so a statusline script can poll it every
        // few seconds. See `cache_stabilization::usage_observer`.
        .route("/cache-health", get(cache_health))
        .route(
            "/stats",
            get(crate::handlers::stats::handle_stats),
        )
        // Polled by the statusline on every prompt, so it stays separate from
        // the heavier /stats payload.
        .route(
            "/codex-limits",
            get(crate::handlers::stats::handle_codex_limits),
        )
        .route(
            "/stats/reset",
            post(crate::handlers::stats::handle_stats_reset),
        )
        .route(
            "/stats-history",
            get(crate::handlers::stats::handle_stats_history),
        )
        // PR-C2: explicit POST route for /v1/chat/completions. The
        // handler buffers the body and re-injects it into
        // `forward_http`, which runs the OpenAI live-zone gate
        // alongside the existing Anthropic dispatcher. Non-POST
        // methods (and other paths) still fall through to
        // `catch_all` so the proxy stays a transparent reverse
        // proxy for everything else.
        .route(
            "/v1/chat/completions",
            post(crate::handlers::chat_completions::handle_chat_completions),
        )
        // PR-C3: explicit POST route for /v1/responses. Same forward
        // pattern as /v1/chat/completions — the handler buffers the
        // body, then `forward_http`'s gate dispatches to the
        // Responses live-zone walker via `compress_openai_responses_request`.
        .route(
            "/v1/responses",
            // GET on this path is the Codex Responses WebSocket upgrade —
            // route it through `catch_all`, whose upgrade branch dispatches
            // to `websocket_codex::ws_codex_handler`. (The other three
            // Codex WS aliases have no explicit route and reach `catch_all`
            // via the fallback.) A plain non-upgrade GET forwards as HTTP.
            post(crate::handlers::responses::handle_responses).get(catch_all),
        )
        // PR-D4: native Vertex publisher path. The Vertex AI Anthropic
        // publisher endpoints look like
        // `POST /v1beta1/projects/{p}/locations/{l}/publishers/anthropic/models/{m}:rawPredict`
        // (and `:streamRawPredict`). The trailing `:<verb>` is awkward
        // in axum's `:param` syntax, so we capture the entire trailing
        // segment as `:model_action` and split on the last `:` inside
        // the dispatcher. Both verbs share the same axum route shape
        // — matchit can't distinguish two patterns that overlap on the
        // literal parameter. The verb dispatch lives in
        // [`crate::vertex::handle_vertex_predict_dispatch`].
        .route(
            "/v1beta1/projects/:project/locations/:location/publishers/anthropic/models/:model_action",
            post(crate::vertex::handle_vertex_predict_dispatch),
        );

    // PR-D1: native AWS Bedrock InvokeModel route. Mounts only when
    // `enable_bedrock_native` is on (default). The handler runs the
    // live-zone compressor over Anthropic-shape bodies, signs with
    // SigV4, and forwards to the configured Bedrock endpoint. The
    // `/converse` route mounts the same handler — the wire shape is
    // identical for `anthropic.claude-*` model IDs (Bedrock just
    // accepts both legacy `invoke` and modern `converse` paths).
    if state.config.enable_bedrock_native {
        // PR-D3: Bedrock-scoped auth-mode middleware. Build a
        // sub-router with ONLY the Bedrock routes, attach the
        // auth-mode layer (so it fires before the handler runs and
        // is scoped to these routes alone — `/v1/messages`,
        // `/healthz`, etc. do NOT run through this middleware), and
        // merge it into the parent router. The merge composes
        // routes without changing their layer stacks; the parent's
        // `with_state` (applied at the end) hands `AppState` to the
        // Bedrock handlers identically.
        let bedrock_router: Router<AppState> = Router::new()
            .route(
                "/model/:model_id/invoke",
                post(crate::bedrock::invoke::handle_invoke),
            )
            .route(
                "/model/:model_id/converse",
                post(crate::bedrock::invoke::handle_invoke),
            )
            // PR-D2/PR-D5: streaming counterparts. Bedrock's protocol is
            // binary EventStream; the handler parses incrementally,
            // optionally translates each chunk to an SSE frame, and
            // tees translated frames into AnthropicStreamState for
            // telemetry. `invoke-with-response-stream` and
            // `converse-stream` share the same wire framing and
            // processing pipeline, so both route to the same handler.
            // See `bedrock::invoke_streaming`.
            .route(
                "/model/:model_id/invoke-with-response-stream",
                post(crate::bedrock::invoke_streaming::handle_invoke_streaming),
            )
            .route(
                "/model/:model_id/converse-stream",
                post(crate::bedrock::invoke_streaming::handle_invoke_streaming),
            )
            .route_layer(axum::middleware::from_fn(
                crate::bedrock::classify_and_attach_auth_mode,
            ))
            // Match the explicit body-size cap used by the other proxy handlers.
            // The `Bytes` extractor axum uses for Bedrock would otherwise cap
            // at axum's built-in 2 MiB default, rejecting valid large payloads.
            .layer(DefaultBodyLimit::max(state.config.max_body_bytes as usize));
        router = router.merge(bedrock_router);
        if !state.config.bedrock_validate_eventstream_crc {
            tracing::warn!(
                event = "bedrock_eventstream_crc_validation_disabled",
                "Bedrock EventStream CRC validation is DISABLED — \
                 only safe for debugging; production must keep \
                 --bedrock-validate-eventstream-crc=true"
            );
        }
    } else {
        tracing::warn!(
            event = "bedrock_native_disabled",
            "Bedrock native InvokeModel route disabled by \
             --enable-bedrock-native=false; Bedrock requests will fall \
             through to the catch-all (no SigV4 re-signing — fails closed)"
        );
    }

    // PR-C4: Conversations API (passthrough-with-instrumentation).
    // The flag is read once at app-build time so router shape
    // matches the configured policy. When disabled, requests still
    // reach upstream via `catch_all`'s streaming forwarder, but the
    // per-route handlers (and their structured-log breadcrumbs) are
    // NOT mounted — operators flip the toggle to silence logs, not
    // to break the surface. The catch-all preserves byte equivalence.
    if state.config.enable_conversations_passthrough {
        router = router
            .route(
                "/v1/conversations",
                post(crate::handlers::conversations::handle_conversations_create),
            )
            .route(
                "/v1/conversations/:conversation_id",
                get(crate::handlers::conversations::handle_conversations_get)
                    .post(crate::handlers::conversations::handle_conversations_update)
                    .delete(crate::handlers::conversations::handle_conversations_delete),
            )
            .route(
                "/v1/conversations/:conversation_id/items",
                post(crate::handlers::conversations::handle_conversations_items_create)
                    .get(crate::handlers::conversations::handle_conversations_items_list),
            )
            .route(
                "/v1/conversations/:conversation_id/items/:item_id",
                get(crate::handlers::conversations::handle_conversations_item_get)
                    .delete(crate::handlers::conversations::handle_conversations_item_delete),
            );
    } else {
        // Mirror the WARN we use elsewhere when a default-on guard
        // is flipped off. Logged at app-build time, not per-request.
        tracing::warn!(
            event = "conversations_passthrough_disabled",
            "Conversations API per-route handlers disabled by \
             --enable-conversations-passthrough=false; requests will \
             still reach upstream via the catch-all (no per-route logs)"
        );
    }

    // Batch API routes. Gated on `enable_batch_api` — when disabled,
    // batch requests fall through to the catch-all (byte-equal passthrough).
    // NOTE: Google batch (`:batchGenerateContent`) is NOT registered here —
    // the Gemini dispatcher below owns `/v1beta/models/*model_action` and
    // delegates batch actions itself; registering it twice would panic axum
    // at startup ("Overlapping method route").
    if state.config.enable_batch_api {
        router = router
            .route(
                "/v1/batches",
                post(crate::handlers::batch::openai_batch_create)
                    .get(crate::handlers::batch::openai_batch_list),
            )
            .route(
                "/v1/batches/:batch_id",
                get(crate::handlers::batch::openai_batch_get),
            )
            .route(
                "/v1/batches/:batch_id/cancel",
                post(crate::handlers::batch::openai_batch_cancel),
            )
            // Anthropic batch (`/v1/messages/batches*`). Create compresses
            // each request's messages; passthrough (list/get/cancel) forwards
            // verbatim; results runs CCR post-processing.
            .route(
                "/v1/messages/batches",
                post(crate::handlers::batch_anthropic::anthropic_batch_create)
                    .get(crate::handlers::batch_anthropic::anthropic_batch_list),
            )
            .route(
                "/v1/messages/batches/:batch_id",
                get(crate::handlers::batch_anthropic::anthropic_batch_get),
            )
            .route(
                "/v1/messages/batches/:batch_id/cancel",
                post(crate::handlers::batch_anthropic::anthropic_batch_cancel),
            )
            .route(
                "/v1/messages/batches/:batch_id/results",
                get(crate::handlers::batch_anthropic::anthropic_batch_results),
            );
    }

    // Gemini native API routes. These handle the Gemini-specific format
    // (contents[] with parts[], systemInstruction) and apply compression
    // via the OpenAI pipeline after format conversion.
    router = router.route(
        "/v1beta/models/*model_action",
        post(crate::handlers::gemini::handle_gemini_action),
    );

    // Azure AI Foundry: Claude Code in Foundry mode points the
    // Anthropic SDK at `ANTHROPIC_FOUNDRY_BASE_URL` (which carries
    // an `/anthropic` path component to mirror the real Azure AI
    // Services URL shape), so requests arrive as
    // `POST /anthropic/v1/messages`. The handler normalizes the
    // path to `/v1/messages` and forwards through the same
    // `forward_http` pipeline, targeting `Config::foundry_base_url`
    // when configured. Mounted unconditionally to match the Python
    // route registration (`headroom/providers/proxy_routes.py`,
    // `foundry_anthropic_messages`).
    router = router.route(
        "/anthropic/v1/messages",
        post(crate::foundry::handle_foundry_messages),
    );

    // Model routing: intercept /v1/messages only when a local model
    // or extra model routes are configured. When disabled, /v1/messages
    // falls through to the catch-all and streams normally (zero overhead).
    if state.config.local_model.is_some() || !state.config.model_routes.is_empty() {
        router = router.route(
            "/v1/messages",
            post(crate::handlers::local_model::handle_messages),
        );
        // Gateway model discovery (CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1):
        // Claude Code queries this at startup to populate the /model picker
        // with routed models. See handlers::local_model::handle_models.
        router = router.route(
            "/v1/models",
            get(crate::handlers::local_model::handle_models),
        );
    }

    // /debug/* endpoints: loopback-only introspection. The loopback guard
    // layer rejects non-loopback callers with 404 (not 403 — invisible to
    // external scanners). Matches Python's `require_loopback` dependency.
    {
        use axum::extract::{ConnectInfo, MatchedPath};
        use axum::http::{HeaderMap, StatusCode};
        use axum::middleware::{self, Next};
        use axum::response::{IntoResponse, Response};

        async fn loopback_guard(req: axum::extract::Request, next: Next) -> Response {
            // Gate 1: client IP must be loopback.
            let client_ip = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());
            let client_ip_str = client_ip.map(|ip| ip.to_string());
            if !crate::loopback_guard::is_loopback_host(client_ip_str.as_deref()) {
                return StatusCode::NOT_FOUND.into_response();
            }

            // Gate 2: Host header must name loopback (DNS-rebinding defense).
            let host_header = req
                .headers()
                .get(http::header::HOST)
                .and_then(|v| v.to_str().ok());
            if !crate::loopback_guard::is_loopback_host_header(host_header) {
                return StatusCode::NOT_FOUND.into_response();
            }

            next.run(req).await
        }

        async fn debug_tasks(
            axum::extract::State(state): axum::extract::State<AppState>,
        ) -> axum::response::Json<serde_json::Value> {
            let warmup = crate::warmup::WarmupRegistry::default();
            let ws = state.ws_sessions.lock().unwrap();
            axum::response::Json(crate::debug_introspection::serialize_tasks_debug(
                &warmup, &ws,
            ))
        }

        async fn debug_ws_sessions(
            axum::extract::State(state): axum::extract::State<AppState>,
        ) -> axum::response::Json<serde_json::Value> {
            let ws = state.ws_sessions.lock().unwrap();
            axum::response::Json(crate::debug_introspection::serialize_ws_sessions_debug(&ws))
        }

        async fn debug_warmup(
            axum::extract::State(state): axum::extract::State<AppState>,
        ) -> axum::response::Json<serde_json::Value> {
            let warmup = crate::warmup::WarmupRegistry::default();
            let ws = state.ws_sessions.lock().unwrap();
            axum::response::Json(crate::debug_introspection::serialize_warmup_debug(
                &warmup, &ws,
            ))
        }

        let debug_router = Router::new()
            .route("/debug/tasks", get(debug_tasks))
            .route("/debug/ws-sessions", get(debug_ws_sessions))
            .route("/debug/warmup", get(debug_warmup))
            .layer(middleware::from_fn(loopback_guard));

        router = router.merge(debug_router);
    }

    // CTX-5/6: mount /ctx/* endpoints when the offload store is available.
    // The endpoints share the proxy's listener — no separate bind.
    if state.config.ctx_offload {
        router = router.nest("/ctx", crate::ctx::endpoints::router());
    }

    // Count every inbound request, including ones that fall through to the
    // catch-all. Applied last so it wraps the whole router.
    router = router.layer(axum::middleware::from_fn(track_inbound_request));

    router.fallback(any(catch_all)).with_state(state)
}

/// Count an inbound request for the lifetime of its handler.
///
/// `headroom_inbound_requests_active` is a balance, so the decrement has to
/// happen on every exit path. Awaiting the inner service and decrementing after
/// covers handler errors and early returns; a client that disconnects mid-flight
/// drops the future here, which is the one case the counter cannot observe.
async fn track_inbound_request(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    crate::observability::proxy_counters::record_inbound_request();
    let response = next.run(request).await;
    crate::observability::proxy_counters::record_inbound_request_completed();
    response
}

/// Catch-all handler. If the request is a WebSocket upgrade, hand off to the
/// ws module; otherwise forward as plain HTTP.
async fn catch_all(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    ws: Option<WebSocketUpgrade>,
    req: Request<Body>,
) -> Response<Body> {
    if is_websocket_upgrade(req.headers()) {
        if let Some(ws) = ws {
            return ws_handler(ws, state, client_addr, req).await;
        }
        // Header says websocket but axum didn't extract it (likely missing
        // Sec-WebSocket-Key) — fall through to HTTP forwarding which will
        // surface the upstream error.
    }
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// True if `Content-Type` is `application/json` (with any optional
/// parameters like `; charset=utf-8`). Compression only inspects JSON
/// bodies — multipart uploads, form-encoded posts, and binary
/// payloads stream through untouched.
/// CTX-7: serve the re-cache watchdog snapshot as JSON.
async fn cache_health(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.usage_observer.snapshot())
}

fn is_application_json(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            // Take the media-type portion before any ';'. Trim and
            // compare case-insensitively per RFC 7231 §3.1.1.1.
            let media_type = s.split(';').next().unwrap_or("").trim();
            media_type.eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false)
}

/// Phase 3: does the buffered request body carry a non-empty message list?
///
/// Mirror of Python's `bool(messages)` input to `CompressionDecision.decide`.
/// The array field depends on the endpoint shape: Anthropic / OpenAI Chat use
/// `messages`; OpenAI Responses uses `input`. A parse failure or a
/// missing/empty/non-array field is treated as "no messages" (the compressors
/// no-op on such bodies anyway).
fn request_has_messages(body: &[u8], endpoint: compression::CompressibleEndpoint) -> bool {
    let field = match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages
        | compression::CompressibleEndpoint::OpenAiChatCompletions => "messages",
        compression::CompressibleEndpoint::OpenAiResponses => "input",
    };
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get(field)
                .and_then(|m| m.as_array())
                .map(|a| !a.is_empty())
        })
        .unwrap_or(false)
}

/// Return the length of the messages/input array in the request body,
/// or `None` if the body can't be parsed or has no message array.
fn message_array_length(body: &[u8], endpoint: compression::CompressibleEndpoint) -> Option<usize> {
    let field = match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages
        | compression::CompressibleEndpoint::OpenAiChatCompletions => "messages",
        compression::CompressibleEndpoint::OpenAiResponses => "input",
    };
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get(field).and_then(|m| m.as_array()).map(|a| a.len()))
}

fn header_map_to_lowercase_strings(
    headers: Option<&HeaderMap>,
) -> std::collections::HashMap<String, String> {
    headers
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|value| (k.as_str().to_lowercase(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the workspace used by CCR Phase 4 tracking/expansion.
///
/// Mirrors Python's tier order: `x-headroom-project-id` →
/// `x-headroom-cwd` → system-prompt `cwd:` line. Returns `None` when no
/// stable workspace is available; callers fail closed rather than tracking
/// under a shared empty workspace.
fn resolve_ccr_workspace(
    headers: Option<&HeaderMap>,
    body: &serde_json::Value,
) -> Option<(String, Option<String>)> {
    let system_prompt = crate::memory::router::extract_system_prompt(body);
    let ctx = crate::memory::router::RequestContext {
        headers: header_map_to_lowercase_strings(headers),
        system_prompt,
        base_user_id: String::new(),
        project_root_override: None,
    };
    crate::memory::router::ProjectResolver::resolve(&ctx).map(|(key, display)| (key, Some(display)))
}

fn latest_user_query(body: &serde_json::Value) -> String {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| {
            messages.iter().rev().find_map(|msg| {
                if msg.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                    return None;
                }
                match msg.get("content") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(serde_json::Value::Array(blocks)) => blocks.iter().find_map(|block| {
                        (block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                            .then(|| {
                                block
                                    .get("text")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .flatten()
                    }),
                    _ => None,
                }
            })
        })
        .unwrap_or_default()
}

fn anthropic_turn_number(body: &serde_json::Value) -> u32 {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| messages.len().min(u32::MAX as usize) as u32)
        .unwrap_or(0)
}

fn append_context_to_latest_user_turn(
    body: &mut serde_json::Value,
    expansion_text: String,
) -> bool {
    if expansion_text.is_empty() {
        return false;
    }
    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|msg| msg.get("role").and_then(serde_json::Value::as_str) == Some("user"))
    else {
        return false;
    };

    match message.get_mut("content") {
        Some(serde_json::Value::String(s)) => {
            s.push_str("\n\n");
            s.push_str(&expansion_text);
            true
        }
        Some(serde_json::Value::Array(blocks)) => {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": expansion_text,
            }));
            true
        }
        _ => {
            message["content"] = serde_json::Value::String(expansion_text);
            true
        }
    }
}

fn maybe_append_ccr_proactive_expansion(
    state: &AppState,
    body: &mut serde_json::Value,
    user_query: &str,
    workspace_key: &str,
    workspace_label: Option<&str>,
    turn_number: u32,
    request_id: &str,
) -> bool {
    if user_query.trim().is_empty()
        || !state.config.ccr_proactive_expansion
        || crate::modes::is_cache_mode(Some(&state.config.mode))
    {
        return false;
    }
    let Some(tracker) = state.ccr_context_tracker.as_ref() else {
        return false;
    };
    let Some(runtime) = state.ctx_offload.as_ref() else {
        return false;
    };

    let recommendations = match tracker.lock() {
        Ok(mut guard) => guard.analyze_query(user_query, Some(turn_number), workspace_key),
        Err(_) => {
            tracing::warn!(
                request_id = %request_id,
                "CCR Phase 4: tracker mutex poisoned; skipping proactive expansion"
            );
            return false;
        }
    };
    if recommendations.is_empty() {
        return false;
    }

    let ccr = runtime.store.ccr();
    let mut expansions = Vec::new();
    for rec in recommendations {
        if let Some(content) = ccr.get(&rec.hash_key) {
            let item_count = content.lines().count().max(1);
            expansions.push(headroom_core::ccr::context_tracker::ExpansionContent {
                hash_key: rec.hash_key,
                content,
                reason: rec.reason,
                item_count,
            });
        }
    }
    if expansions.is_empty() {
        return false;
    }

    let expansion_text =
        headroom_core::ccr::context_tracker::ContextTracker::format_expansions_for_context(
            &expansions,
            workspace_label,
        );
    let changed = append_context_to_latest_user_turn(body, expansion_text);
    if changed {
        tracing::info!(
            request_id = %request_id,
            expansions = expansions.len(),
            "CCR Phase 4: proactively expanded relevant offloaded context"
        );
    }
    changed
}

fn track_ccr_context_records(
    state: &AppState,
    records: &[crate::compression::ctx_offload::OffloadRecord],
    workspace_key: &str,
    user_query: &str,
    turn_number: u32,
    request_id: &str,
) {
    if records.is_empty() || workspace_key.is_empty() {
        return;
    }
    let Some(tracker) = state.ccr_context_tracker.as_ref() else {
        return;
    };
    let mut guard = match tracker.lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!(
                request_id = %request_id,
                "CCR Phase 4: tracker mutex poisoned; skipping compression tracking"
            );
            return;
        }
    };
    for record in records {
        let sample = record.original.chars().take(500).collect::<String>();
        let item_count = record.original.lines().count().max(1);
        guard.track_compression(
            &record.hash,
            turn_number,
            (!record.title.is_empty()).then_some(record.title.as_str()),
            item_count,
            1,
            workspace_key,
            user_query,
            &sample,
        );
    }
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    upgrade && connection
}

/// Parse an Anthropic `/v1/messages` body, inject `context_management`
/// directives, and re-serialize. Forwards the body unchanged on any
/// parse/serialize failure or when nothing new was injected.
fn maybe_inject_context_management(
    body: bytes::Bytes,
    config: &crate::config::Config,
    request_id: &str,
) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let changed = crate::compression::context_editing::inject_context_management(
        &mut value,
        Some(config.context_edit_keep_tool_uses),
        config.context_edit_trigger_tokens,
        config.context_edit_keep_thinking,
    );
    if !changed {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                request_id = %request_id,
                keep_tool_uses = config.context_edit_keep_tool_uses,
                trigger_tokens = config.context_edit_trigger_tokens,
                keep_thinking = ?config.context_edit_keep_thinking,
                "injected context_management directives"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Prune the `tools[]` array per the operator-configured policy (A4).
///
/// Deterministic + cache-safe: the same policy always removes the same tools,
/// so the emitted tools prefix stays byte-stable across turns. Reduces
/// cache_creation — the only bucket that counts toward subscription usage
/// (cache reads are free per Anthropic's rate-limit docs). No-op (returns the
/// original bytes untouched) when the body has no tools array or nothing is
/// removed, so a cache-stable request is never perturbed.
fn maybe_prune_tools(
    body: bytes::Bytes,
    policy: &crate::cache_stabilization::tool_prune::PrunePolicy,
    request_id: &str,
) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(tools) = value.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return body;
    };
    let before = tools.len();
    let removed = crate::cache_stabilization::tool_prune::prune_tools(tools, policy);
    if removed == 0 {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                request_id = %request_id,
                tools_before = before,
                tools_removed = removed,
                "pruned tools[] per policy"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Strip annotation keys (`$schema`, `title`, `examples`, …) from `tools[]`
/// and normalise description whitespace, then re-serialize.
///
/// Mirrors the pass both Python handlers apply after tools are finalised
/// (`headroom/proxy/handlers/anthropic.py`, `.../openai.py`). Shape-agnostic:
/// it walks the whole `tools` array, so Anthropic's `input_schema` and
/// OpenAI's `function.parameters` are both covered.
///
/// Forwards the original bytes untouched when there is no `tools` array, when
/// compaction saves nothing, or on any parse/serialize failure — a
/// cache-stable request is never perturbed for zero gain.
fn maybe_compact_tool_schemas(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let (compacted, modified, before_bytes, after_bytes) =
        crate::tool_schema_compaction::compact_tools(value);
    if !modified {
        return body;
    }
    match serde_json::to_vec(&compacted) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                tools_before_bytes = before_bytes,
                tools_after_bytes = after_bytes,
                "tool schema compaction"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// B2: reorder `tools[]` to lead with the order forwarded on this session's
/// previous turn, appending genuinely-new tools at the end.
///
/// Runs last, once tools are final — after routing, memory/CCR injection,
/// pruning and schema compaction — so the recorded order is the order the
/// provider actually caches. See
/// [`cache_stabilization::tool_order`] for the guards and the measured effect.
///
/// Forwards the original bytes untouched when there is no `tools` array, when
/// the stabilizer declines, or on any parse/serialize failure.
///
/// An empty `session_key` is also a passthrough. It should not happen on this
/// branch (the drift detector populates it for every buffered Anthropic
/// request), but the failure mode if it ever did is every conversation on the
/// box collapsing into one store slot and replaying each other's tool order.
fn maybe_stabilize_tool_order(
    body: bytes::Bytes,
    store: &cache_stabilization::tool_order::ToolOrderStore,
    session_key: &str,
    request_id: &str,
) -> bytes::Bytes {
    if session_key.is_empty() {
        return body;
    }
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let model = value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let reordered = match value
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        Some(tools) => store.stabilize(session_key, &model, tools),
        None => return body,
    };
    if !reordered {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                model = %model,
                event = "cache_stable_tool_order",
                "replayed previous tool order; new tools appended at the end"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// B1: pin every `cache_control.ttl` in the body to `1h`.
///
/// Runs last, after every mutation that could add or move a marker, so what we
/// pin is what goes on the wire. See [`cache_stabilization::cache_ttl`] for the
/// economics and why this is skipped on PAYG.
///
/// Forwards the original bytes untouched when there is no marker to change or
/// on any parse/serialize failure.
fn maybe_force_1h_cache_ttl(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    if !cache_stabilization::cache_ttl::force_1h_ttl(&mut value) {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                event = "force_1h_cache_ttl",
                "pinned cache_control ttl to 1h"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Whether an upstream `reqwest` send error is a transient transport
/// failure worth retrying on a fresh connection.
///
/// Ports Python's broadening (commits 2ce19c2c + 5d14080c) from the
/// narrow `(ConnectError, Timeout)` set to any `httpx.TransportError`.
/// The `httpx` transport family includes h2 stream resets
/// (`RemoteProtocolError`/`StreamReset`) and pooled keep-alive
/// connections closed mid-response (`incomplete chunked read`). Under
/// concurrent load a single poisoned HTTP/2 connection would otherwise
/// cascade every in-flight request to a 502 with no reconnect.
///
/// In `reqwest` these surface as connect/timeout errors OR as
/// request/body-level errors (`is_request` covers a stream reset while
/// sending; `is_body` covers an incomplete response body read). We
/// deliberately exclude `is_status`/`is_decode`/`is_builder`, which are
/// not transport-transient and must not be retried.
fn is_retryable_transport_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
}

/// Append a token to the `anthropic-beta` header, preserving existing tokens
/// and skipping if already present.
fn append_anthropic_beta(headers: &mut http::HeaderMap, beta: &str) {
    const NAME: &str = "anthropic-beta";
    let existing = headers
        .get(NAME)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if existing.split(',').any(|t| t.trim() == beta) {
        return;
    }
    let merged = if existing.is_empty() {
        beta.to_string()
    } else {
        format!("{existing},{beta}")
    };
    if let Ok(val) = http::HeaderValue::from_str(&merged) {
        headers.insert(NAME, val);
    }
}

/// Per-request upstream base override, inserted into request
/// extensions by provider routes that forward to a different upstream
/// than `--upstream` (currently: the Azure AI Foundry route,
/// [`crate::foundry::handle_foundry_messages`], when
/// `Config::foundry_base_url` is configured). `forward_http` reads it
/// back out when building the upstream URL; absent extension means
/// `Config::upstream` as before.
#[derive(Clone, Debug)]
pub struct UpstreamOverride(pub url::Url);

/// Resolve a per-request upstream override from the `x-headroom-base-url`
/// request header. Returns `None` when the header is absent, empty, or
/// whitespace-only (after trimming), or when the value does not parse as a
/// URL — in all those cases the caller falls back to the default upstream.
/// The value is trimmed and a single trailing `/` is stripped, matching the
/// Python proxy's `.strip().rstrip("/")` contract.
fn header_upstream_override(headers: &HeaderMap) -> Option<url::Url> {
    let raw = headers
        .get(crate::headers::UPSTREAM_OVERRIDE_HEADER)
        .and_then(|v| v.to_str().ok())?;
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match url::Url::parse(trimmed) {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::warn!(
                event = "upstream_override_parse_failed",
                header = crate::headers::UPSTREAM_OVERRIDE_HEADER,
                value = %trimmed,
                error = %e,
                "ignoring malformed x-headroom-base-url; using default upstream"
            );
            None
        }
    }
}

/// Build the upstream URL by joining the configured base with the incoming
/// path-and-query. Preserves '?' and the query string verbatim.
pub(crate) fn build_upstream_url(base: &url::Url, uri: &Uri) -> Result<url::Url, ProxyError> {
    Ok(join_upstream_path(base, uri.path(), uri.query()))
}

/// Shared path-join helper used by HTTP and WebSocket handlers.
/// Appends `path` to `base`, preserving any base path prefix, then sets `query`.
pub(crate) fn join_upstream_path(base: &url::Url, path: &str, query: Option<&str>) -> url::Url {
    let mut joined = base.clone();
    // Strip trailing slash from base path so "http://x:1/api" + "/v1/foo"
    // yields "http://x:1/api/v1/foo" rather than "http://x:1/v1/foo".
    let base_path = joined.path().trim_end_matches('/').to_string();
    let combined = if path.is_empty() || path == "/" {
        if base_path.is_empty() {
            "/".to_string()
        } else {
            base_path
        }
    } else if base_path.is_empty() {
        path.to_string()
    } else {
        format!("{base_path}{path}")
    };
    joined.set_path(&combined);
    joined.set_query(query);
    joined
}

/// Forward an HTTP request to the upstream and stream the response back.
pub(crate) async fn forward_http(
    state: AppState,
    client_addr: SocketAddr,
    mut req: Request<Body>,
) -> Result<Response<Body>, ProxyError> {
    let start = Instant::now();
    let request_id = ensure_request_id(req.headers());
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path_for_log = uri.path().to_string();
    let mut stage_timer = crate::stage_timer::StageTimer::new();
    let body_bytes_hint = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // Phase F PR-F1: classify auth mode at request entry. The result
    // is stored in request extensions so downstream handlers (cache
    // gates, header injection, lossy-compressor gates) read it
    // without re-classifying. Pure function, <10us per call —
    // doing it once here is cheaper than threading the result.
    let auth_mode = classify_auth_mode(req.headers());
    req.extensions_mut().insert(auth_mode);

    // Resolve client IP through trusted gateway CIDRs.
    let client_ip = crate::forwarded_headers::resolve_client_ip(
        Some(&client_addr.ip().to_string()),
        req.headers(),
        &state.trusted_gateway_cidrs,
    );

    // Phase F PR-F2.1, c2/6: derive the per-mode CompressionPolicy at
    // request entry and stash alongside auth_mode. Storing the policy
    // (not just auth_mode) in extensions lets downstream stages read
    // the gate they need directly — no per-stage `for_mode` call.
    //
    // c3/6: when `auth_mode_policy_enforcement` is `Disabled` (default
    // until c6/6), force the policy to PAYG regardless of classifier
    // output. This means c4/6 + c5/6 only ship behaviour change when
    // an operator opts in via the env var, so the PR sequence is
    // safely landed in main without flipping the live wire on default
    // users until the final commit.
    let policy = if state.config.auth_mode_policy_enforcement.is_enabled() {
        CompressionPolicy::for_mode(auth_mode)
    } else {
        CompressionPolicy::for_mode(AuthMode::Payg)
    };
    req.extensions_mut().insert(policy);

    // Per PR-A1: structured entry log. The `auth_mode` field is now
    // populated with the real classification result (Phase F PR-F1
    // replaces the prior `auth_mode_placeholder = "unknown"`). Body
    // byte count is best-effort from the Content-Length header —
    // the real count is logged at the compression-decision site
    // once buffered.
    tracing::debug!(
        event = "auth_mode_classified",
        request_id = %request_id,
        auth_mode = auth_mode.as_str(),
        method = %method,
        path = %path_for_log,
        content_length_bytes = ?body_bytes_hint,
        "request received"
    );

    // F2.1 c2/6: emit the policy that the request will run under so
    // F2.2 has bake-time data to tune from. One log per request,
    // structured fields so it joins on auth_mode + request_id.
    // c3/6 adds `enforcement` so the dashboard can split "policy
    // resolved as PAYG because mode is PAYG" from "policy resolved as
    // PAYG because the enforcement flag is off."
    //
    // F2.2 c2/3: extend the structured fields with the three new
    // tuning fields so the bake dashboard has per-mode observability
    // for the F2.2-followup tune. ``volatile_token_threshold`` /
    // ``max_lossy_ratio`` are plumbed-but-unconsumed today, so the
    // log lines are the only signal that the values are flowing
    // correctly through the proxy → handlers → transforms path.
    tracing::debug!(
        event = "policy_selected",
        request_id = %request_id,
        auth_mode = auth_mode.as_str(),
        enforcement = state.config.auth_mode_policy_enforcement.as_str(),
        live_zone_only = policy.live_zone_only,
        cache_aligner_enabled = policy.cache_aligner_enabled,
        volatile_token_threshold = policy.volatile_token_threshold,
        max_lossy_ratio = policy.max_lossy_ratio,
        toin_read_only = policy.toin_read_only,
        "compression policy resolved"
    );

    // Provider routes (Foundry) may pin a different upstream base for
    // this request via the `UpstreamOverride` extension; everything
    // else forwards to the configured `--upstream`. Absent that, honor a
    // per-request `x-headroom-base-url` header (mirrors the Python proxy):
    // trim whitespace and strip a trailing `/`; an empty/whitespace-only
    // value or an unparseable URL falls through to the default upstream.
    let upstream_base = match req.extensions().get::<UpstreamOverride>() {
        Some(o) => o.0.clone(),
        None => match header_upstream_override(req.headers()) {
            Some(u) => u,
            None => state.effective_upstream().await,
        },
    };
    let upstream_url = build_upstream_url(&upstream_base, &uri)?;

    // Forwarded-Host: prefer client's Host. Forwarded-Proto: assume http for
    // now (we don't terminate TLS in this binary; if a TLS terminator is in
    // front, it should rewrite this — which we'd handle by not overwriting
    // an existing one in a future change).
    let forwarded_host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Build the outgoing headers off the incoming ones, then optionally drop
    // Host (rewrite_host=true => let reqwest set its own Host for the upstream).
    // PR-A5 (P5-49): strip internal `x-headroom-*` from upstream-bound
    // requests when `Config::strip_internal_headers == Enabled` (default).
    let strip_internal = state.config.strip_internal_headers.is_enabled();
    let pre_strip_internal_count = req
        .headers()
        .iter()
        .filter(|(name, _)| crate::headers::is_internal_header(name))
        .count();
    // PR-F4 (P5-53): synthetic X-Forwarded-* / X-Request-Id injection is
    // skipped for Subscription traffic (fingerprint risk). Staged behind
    // the same enforcement flag as CompressionPolicy (c3/6 pattern): with
    // enforcement disabled every request keeps the PAYG behavior.
    let header_auth_mode = if state.config.auth_mode_policy_enforcement.is_enabled() {
        auth_mode
    } else {
        AuthMode::Payg
    };
    let mut outgoing_headers = build_forward_request_headers(
        req.headers(),
        client_addr.ip(),
        "http",
        forwarded_host.as_deref(),
        &request_id,
        strip_internal,
        header_auth_mode,
    );
    if strip_internal && pre_strip_internal_count > 0 {
        tracing::info!(
            event = "outbound_headers",
            forwarder = "rust_proxy",
            stripped_count = pre_strip_internal_count,
            request_id = %request_id,
            "stripped internal x-headroom-* headers from upstream-bound request"
        );
    } else if !strip_internal && pre_strip_internal_count > 0 {
        tracing::warn!(
            event = "outbound_headers",
            forwarder = "rust_proxy",
            mode = "disabled",
            internal_count = pre_strip_internal_count,
            request_id = %request_id,
            "HEADROOM_PROXY_STRIP_INTERNAL_HEADERS=disabled; \
             internal x-headroom-* headers forwarded to upstream"
        );
    }
    if !state.config.rewrite_host {
        if let Some(h) = req.headers().get(http::header::HOST) {
            outgoing_headers.insert(http::header::HOST, h.clone());
        }
    }

    // ─── COMPRESSION GATE ──────────────────────────────────────────────
    //
    // PR-A1 lockdown (per `REALIGNMENT/03-phase-A-lockdown.md`): the
    // `/v1/messages` path no longer mutates the body. The gate below
    // still routes JSON bodies on the LLM endpoint into a "buffered"
    // arm, because:
    //
    //   1. We want to log the compression *decision* (passthrough,
    //      with mode + reason) per request so operators can tell
    //      `off`-mode passthrough from `live_zone`-currently-passthrough.
    //   2. Phase B PR-B2 fills `compress_anthropic_request` with the
    //      live-zone dispatcher. Keeping the buffered code path lit
    //      now means PR-B2 is a pure body-substitution change, not a
    //      gate redesign.
    //   3. The buffered branch issues a `debug_assert!` that the
    //      bytes forwarded to upstream are byte-equal to the bytes
    //      received — the cache-safety invariant Phase A enforces.
    //
    // Gate criteria (ALL true → buffered passthrough; otherwise stream):
    //
    //   - `state.config.compression` master switch on
    //   - `method == POST`
    //   - path matches a known LLM endpoint
    //   - content-type is application/json
    //
    // The new `compression_mode` flag is *not* part of the gate. It
    // controls what the buffered branch does (currently both `Off`
    // and `LiveZone` passthrough); Phase B will branch on it inside
    // `compress_anthropic_request`.
    // Phase 3: canonical input-side compression decision — the single source
    // of truth for the bypass / master-switch / no-messages / license gate
    // (ports Python `CompressionDecision`, replacing the ad hoc conjunctions
    // that four handler sites drifted on). Computed once here at ingestion.
    //
    // `has_messages` and `license_allows` need the parsed body / a licensing
    // system, neither of which exists at the gate (the body is not buffered
    // until inside the `should_intercept` branch). At the gate only the
    // header+config-derivable inputs matter — bypass and the master switch —
    // so we pass `has_messages=true`/`license_allows=true` and read
    // `should_compress`, which reduces to `!bypass && config.compression`.
    // This is what folds honoring of `x-headroom-bypass` /
    // `x-headroom-mode: passthrough` into the gate: such requests take the
    // streaming (byte-faithful) arm and are never buffered or mutated. The
    // decision is refined once the body is parsed (see `decision` below).
    // license_allows: TODO(license) — no license plumbing in Rust yet.
    let gate_decision = crate::compression_decision::CompressionDecision::decide(
        req.headers(),
        state.config.compression,
        true,
        true,
    );
    let should_intercept = gate_decision.should_compress
        && method == axum::http::Method::POST
        && compression::is_compressible_path(uri.path())
        && is_application_json(req.headers());

    // Intercepted requests tee the response into the SSE state machine
    // (usage observer, hit-rate metrics, re-cache watchdog). Those parsers
    // read the raw byte stream, so a gzip/br-encoded upstream response is
    // opaque to them — Claude Code sends `accept-encoding: gzip, deflate,
    // br, zstd` and Anthropic gzips SSE, which silently blinds all
    // response-side telemetry. Force identity upstream for intercepted
    // requests; the client receives the same uncompressed bytes (we never
    // re-encode), which HTTP permits regardless of what it advertised.
    if should_intercept {
        outgoing_headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("identity"),
        );
    }

    // PR-E6: capture a header snapshot BEFORE the body is consumed so
    // the drift detector can derive a per-session key from
    // `Authorization`/`x-api-key`/`User-Agent`. `req` will be moved
    // into either `to_bytes(req.into_body())` (buffered branch) or
    // `req.into_body().into_data_stream()` (streaming branch); both
    // discard the headers along with the body. Snapshot here keeps
    // both branches clean.
    let headers_snapshot = if should_intercept {
        Some(req.headers().clone())
    } else {
        None
    };

    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|e| ProxyError::InvalidHeader(e.to_string()))?;

    // Populated inside the intercept block; consumed by the SSE state-machine
    // task to build RequestOutcome at stream close.
    let mut outcome_ctx: Option<OutcomeContext> = None;

    // Saved copy of the original request body for semantic cache key
    // computation. Populated inside the `should_intercept` block after
    // the body is buffered; used by the cache SET after the upstream
    // response returns.
    let mut original_buffered: bytes::Bytes = bytes::Bytes::new();

    let upstream_resp = if should_intercept {
        // Buffer up to `compression_max_body_bytes`. If the body
        let max = state.config.compression_max_body_bytes as usize;
        if let Some(len) = body_bytes_hint {
            if len as usize > max {
                tracing::warn!(
                    request_id = %request_id,
                    path = %path_for_log,
                    limit_bytes = max,
                    content_length = len,
                    "compression: Content-Length exceeds buffer limit; \
                     returning 413 without consuming body"
                );
                return Err(ProxyError::PayloadTooLarge(format!(
                    "request Content-Length {len} exceeds compression \
                     buffer limit ({max} bytes)"
                )));
            }
        }
        let buffered = match to_bytes(req.into_body(), max).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    path = %path_for_log,
                    limit_bytes = max,
                    error = %e,
                    "compression: body exceeds buffer limit; failing loudly (cannot \
                     resume streaming once the body has been partially consumed)"
                );
                return Err(ProxyError::PayloadTooLarge(format!(
                    "request body exceeds compression buffer limit ({max} bytes): {e}"
                )));
            }
        };

        // Save the original buffer for semantic cache key computation.
        // CTX transforms and compression may modify `buffered`; the
        // cache key must reflect the original request.
        original_buffered = buffered.clone();

        // PR-C2: dispatch on the endpoint classification so each
        // provider hits its own live-zone walker. PR-B2/B3/B4 wired
        // the Anthropic dispatcher; PR-C2 adds the OpenAI Chat
        // Completions sibling. The classification was already
        // computed by `is_compressible_path` above; we re-classify
        // here so a single-source `match` decides which dispatcher
        // runs and what skip rules apply.
        //
        // Skip rules (per spec PR-C2):
        // - OpenAI Chat: `n > 1` skips compression entirely (multiple
        //   completions imply non-determinism scenarios). `tool_choice`
        //   and `stream_options` are NOT skip conditions — they
        //   round-trip byte-equal as a side effect of byte-range surgery.
        // - Anthropic: no extra skip rules at this layer.
        let endpoint = compression::classify_compressible_path(uri.path())
            .expect("is_compressible_path guarded above");

        // PR-2027: strip the `[1m]` context-window tier suffix from
        // the request body for Anthropic messages only. The
        // Headroom CLI appends `[1m]` to model IDs (e.g.
        // `glm-5.2[1m]`, `claude-3-7-sonnet[1m]`) to signal 1M
        // context to Claude Code; the upstream Anthropic API does
        // not recognize the suffix and rejects the request. The
        // suffix is an Anthropic/Claude Code compatibility marker,
        // so we must not silently mutate OpenAI-compatible
        // request model IDs. The sanitizer is gated on the
        // already-classified `endpoint`, which is the same source
        // of truth the dispatcher uses below — keeping the gate
        // and the dispatch in lockstep.
        let buffered = match endpoint {
            compression::CompressibleEndpoint::AnthropicMessages => {
                compression::sanitize_anthropic_model_id_in_body(buffered)
            }
            compression::CompressibleEndpoint::OpenAiChatCompletions
            | compression::CompressibleEndpoint::OpenAiResponses => buffered,
        };

        // PR-E5 + PR-E6: cache-stabilization observability hooks.
        // Both run READ-ONLY against the buffered body and emit
        // structured logs only — passthrough invariant from Phase A
        // is preserved. Parsing happens once and is shared. Cheap
        // parse failure (malformed JSON) silently skips both
        // detectors; the dispatcher below logs its own parse-error
        // decision. The hooks run regardless of whether the
        // dispatcher returns `NoCompression`, `Compressed`, or
        // `Passthrough`.
        //
        // Bedrock and other shape-mismatched paths skip the drift
        // detector specifically; their wire shape is different
        // enough that a canonical-bytes hash would compare apples
        // to oranges. The volatile detector handles its own
        // shape-dispatch via `ApiKind::from_endpoint`.
        // PR-J4: whether the drift detector saw a cache hot-zone rebuild on
        // this turn. Consumed below by the offload boundary gate.
        let mut rebuild_boundary = false;
        // One session key per request, derived here and reused by every
        // downstream consumer (ctx injection, the offload boundary gate,
        // prefix replay). `derive_session_key` fingerprints the
        // conversation's FIRST message when no `x-headroom-session-id`
        // header is present, and both ctx injection and CCR proactive
        // expansion rewrite that message further down. Re-deriving after
        // either would mint a fresh key every turn — recall content differs
        // each time — decoupling the offload gate from the drift detector
        // and stopping prefix replay from ever hitting its store. Assigned
        // inside the parse below so it shares that parse and is identical
        // to the drift detector's key by construction.
        let mut request_session_key = String::new();
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&buffered) {
            // PR-E5: volatile-content detector. Emits one WARN per
            // finding (capped at 10) for content that busts cache
            // (timestamps, UUIDs, ID-named fields).
            let volatile_kind =
                cache_stabilization::volatile_detector::ApiKind::from_endpoint(endpoint);
            let findings = cache_stabilization::volatile_detector::detect_volatile_content(
                &parsed,
                volatile_kind,
            );
            if !findings.is_empty() {
                cache_stabilization::volatile_detector::emit_volatile_warnings(
                    &findings,
                    &request_id,
                );
            }

            // PR-E6: cache-bust drift detector. SHA-256 fingerprints
            // the cache hot zone (system / tools / first 3 messages);
            // a mismatch between consecutive turns of the same session
            // emits a `cache_drift_observed` event so operators see
            // invisible cache busts.
            let drift_kind = match endpoint {
                compression::CompressibleEndpoint::AnthropicMessages => Some(ApiKind::Anthropic),
                compression::CompressibleEndpoint::OpenAiChatCompletions => {
                    Some(ApiKind::OpenAiChat)
                }
                compression::CompressibleEndpoint::OpenAiResponses => {
                    Some(ApiKind::OpenAiResponses)
                }
            };
            if let (Some(kind), Some(headers)) = (drift_kind, headers_snapshot.as_ref()) {
                let session_key = derive_session_key(headers, &client_addr, &parsed, kind);
                request_session_key = session_key.clone();
                let hash = compute_structural_hash(&parsed, kind);
                let drift_dims = observe_drift(&state.drift_state, &session_key, hash);
                rebuild_boundary = drift_dims.is_some();

                // CTX-7: park conversation identity + drift dims under
                // the request id so the response-side usage observer
                // can classify this turn's billed usage against the
                // conversation's previous turn.
                state.usage_observer.begin_request(
                    &request_id,
                    cache_stabilization::usage_observer::conversation_key(&parsed, &session_key),
                    drift_dims,
                );

                // PR-J0: env-gated request-body capture for the offload
                // simulator. Pure observer (no body mutation); no-op unless
                // HEADROOM_CAPTURE_DIR is set. Reuses the hashed session key
                // so the simulator can group + order turns per session.
                let endpoint_label = match endpoint {
                    compression::CompressibleEndpoint::AnthropicMessages => "anthropic",
                    compression::CompressibleEndpoint::OpenAiChatCompletions => "openai_chat",
                    compression::CompressibleEndpoint::OpenAiResponses => "openai_responses",
                };
                cache_stabilization::capture::maybe_capture(
                    &parsed,
                    endpoint_label,
                    &session_key,
                    &request_id,
                );

                // CTX-2: passive session capture. Same spot + inputs as
                // maybe_capture; same never-block rule — `observe` clones the
                // body once and hands it to a detached worker. No-op unless
                // `ctx_capture` is enabled (then `ctx_observer` is `Some`).
                if let Some(observer) = state.ctx_observer.as_ref() {
                    observer.observe(&parsed, &session_key);
                }
            }
        }

        // ─── SEMANTIC CACHE CHECK ──────────────────────────────────────
        //
        // Check the in-memory response cache for identical non-streaming
        // requests. On a hit, return the cached response directly — skip
        // CTX transforms, compression, and upstream entirely. Only applies
        // to the intercepted (buffered) path; streaming requests bypass
        // the cache.
        if let Some(ref cache) = state.semantic_cache {
            if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&buffered) {
                let is_streaming = parsed
                    .get("stream")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !is_streaming {
                    let model = parsed
                        .get("model")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    let messages = parsed
                        .get("messages")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    // Build extra key fields: system, tools, tool_choice,
                    // temperature, top_p, top_k, max_tokens, stop.
                    let mut extra = serde_json::Map::new();
                    for key in &[
                        "system",
                        "tools",
                        "tool_choice",
                        "temperature",
                        "top_p",
                        "top_k",
                        "max_tokens",
                        "stop",
                    ] {
                        if let Some(val) = parsed.get(*key) {
                            extra.insert(key.to_string(), val.clone());
                        }
                    }
                    if let Some(entry) = cache.get(&messages, model, &extra) {
                        tracing::info!(
                            event = "semantic_cache_hit",
                            request_id = %request_id,
                            path = %path_for_log,
                            model = model,
                            hit_count = entry.hit_count,
                            body_bytes = entry.response_body.len(),
                            "semantic cache hit; returning cached response"
                        );
                        // Build a synthetic Response from the cached entry.
                        let mut resp_headers = HeaderMap::new();
                        for (k, v) in &entry.response_headers {
                            if let (Ok(name), Ok(val)) = (
                                HeaderName::from_bytes(k.as_bytes()),
                                http::HeaderValue::from_str(v),
                            ) {
                                resp_headers.insert(name, val);
                            }
                        }
                        resp_headers.insert(
                            http::header::CONTENT_LENGTH,
                            http::HeaderValue::from(entry.response_body.len()),
                        );
                        return Ok(Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from(entry.response_body))
                            .unwrap());
                    }
                }
            }
        }

        // Mirror the enforcement-flag override already applied to
        // CompressionPolicy at request entry (line ~416): when
        // `--auth-mode-policy-enforcement disabled` is set, treat every
        // request as PAYG so Phase E byte-mutating passes (E1 tool-array
        // sort, E2 schema-key sort, E3 cache_control auto-placement) are
        // no longer skipped for subscription/OAuth callers. Token
        // reduction matters for subscription users too (rate limits,
        // context-window pressure) — the original guard was billing-only.
        let effective_auth_mode = if state.config.auth_mode_policy_enforcement.is_enabled() {
            auth_mode
        } else {
            AuthMode::Payg
        };

        // CTX-3 + CTX-4: byte-mutating context-mode transforms, applied BEFORE
        // the live-zone compressors, Anthropic only, in a single parse/serialize:
        //
        //   1. CTX-4 injection — prepends a once-decided, timestamp-free recall
        //      or resume block into the first user message and replays it
        //      verbatim every turn (I1/I4). Needs a synchronous sessions read;
        //      an in-memory LRU keeps steady-state turns off the DB.
        //   2. CTX-3 offload — replaces oversized tool_result blocks in ALL
        //      messages with a pure digest of the block bytes; exempt from the
        //      frozen-count floor precisely because the replacement is
        //      recomputable from the resent raw block (I1/I2).
        //
        // Both are pure w.r.t. their inputs, so the cached prefix never drifts.
        // Injection edits the first user message; offload edits tool_result
        // blocks — disjoint, order-independent. When neither changes anything we
        // forward the original bytes untouched (no gratuitous re-cache). Offload
        // originals are persisted on a detached worker; wire bytes never depend
        // on any store.

        let buffered = if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) && (state.ctx_inject.is_some() || state.ctx_offload.is_some())
        {
            match serde_json::from_slice::<serde_json::Value>(&buffered) {
                Ok(mut value) => {
                    let ctx_session_key = request_session_key.clone();
                    let mut changed = false;
                    let ccr_workspace = resolve_ccr_workspace(headers_snapshot.as_ref(), &value);
                    let latest_user_query = latest_user_query(&value);
                    let turn_number = anthropic_turn_number(&value);

                    if let Some((workspace_key, workspace_label)) = ccr_workspace.as_ref() {
                        if maybe_append_ccr_proactive_expansion(
                            &state,
                            &mut value,
                            &latest_user_query,
                            workspace_key,
                            workspace_label.as_deref(),
                            turn_number,
                            &request_id,
                        ) {
                            changed = true;
                        }
                    } else if state.ccr_context_tracker.is_some() {
                        tracing::info!(
                            request_id = %request_id,
                            "CCR Phase 4: workspace unresolved; proactive expansion disabled for this request"
                        );
                    }

                    if let Some(engine) = state.ctx_inject.as_ref() {
                        let session_key = ctx_session_key.clone();
                        if engine.maybe_inject(&mut value, &session_key) {
                            changed = true;
                        }
                    }

                    let offload_records = if let Some(runtime) = state.ctx_offload.as_ref() {
                        // PR-J4: boundary-gated policy — first conversions of
                        // frozen-history blocks only ride a rebuild boundary;
                        // live-tail blocks and re-applications always pass.
                        let session_key = ctx_session_key.clone();
                        let policy = crate::compression::ctx_offload::OffloadPolicy {
                            gate: &runtime.gate,
                            session_key: &session_key,
                            rebuild_boundary,
                        };
                        let out = crate::compression::ctx_offload::offload_anthropic_request(
                            &mut value,
                            &runtime.config,
                            Some(&policy),
                        );
                        // PR-J5 thrash guard: an I4 violation (frozen-history
                        // conversion on a steady-state turn) is a cache-thrash
                        // bug — page-worthy, per the Phase J plan §13.
                        if !rebuild_boundary && out.frozen_new_offloads > 0 {
                            tracing::warn!(
                                event = "ctx_offload_thrash_guard",
                                request_id = %request_id,
                                frozen_new_offloads = out.frozen_new_offloads,
                                "ctx_offload converted frozen history on a non-boundary turn (I4 violation)"
                            );
                        }
                        if out.changed() {
                            changed = true;
                            // CTX-6: offload metrics are recorded by the
                            // offload-store worker after persist_one confirms
                            // the record is durably recoverable, not here —
                            // see ctx/offload_store.rs.
                            tracing::debug!(
                                request_id = %request_id,
                                blocks_offloaded = out.blocks_offloaded,
                                blocks_deferred = out.blocks_deferred,
                                rebuild_boundary,
                                "ctx_offload rewrote tool_result blocks"
                            );
                            Some((runtime, out.records))
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Memory: inject tool definitions into the request body.
                    // Runs after CTX transforms, before output shaping.
                    if let Some(handler) = state.memory_handler.as_ref() {
                        let handler = handler.lock().await;
                        if handler.is_initialized() {
                            let provider = match endpoint {
                                compression::CompressibleEndpoint::AnthropicMessages => {
                                    crate::memory::tool_adapter::Provider::Anthropic
                                }
                                compression::CompressibleEndpoint::OpenAiChatCompletions
                                | compression::CompressibleEndpoint::OpenAiResponses => {
                                    crate::memory::tool_adapter::Provider::Openai
                                }
                            };
                            // Requests without a `tools` array still get the
                            // memory tools — create the array on demand.
                            let existing: Vec<serde_json::Value> = value
                                .get("tools")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let (new_tools, injected) =
                                handler.inject_memory_tools(Some(&existing), provider);
                            if injected {
                                if let Some(obj) = value.as_object_mut() {
                                    obj.insert(
                                        "tools".to_string(),
                                        serde_json::Value::Array(new_tools),
                                    );
                                    changed = true;
                                    tracing::debug!(
                                        request_id = %request_id,
                                        "memory: injected tool definitions"
                                    );
                                }
                            }
                        }
                    }

                    // CCR: inject the `headroom_retrieve` tool definition
                    // into the request body so the LLM can retrieve original
                    // uncompressed content by hash. Only when compression
                    // has produced CCR markers and the feature is enabled.
                    if state.config.ccr_inject_tool {
                        if let Some(tools) = value.get_mut("tools").and_then(|v| v.as_array_mut()) {
                            let already_has = tools.iter().any(|t| {
                                t.get("name").and_then(|n| n.as_str()) == Some("headroom_retrieve")
                            });
                            if !already_has {
                                let ccr_tool = match endpoint {
                                    compression::CompressibleEndpoint::AnthropicMessages => {
                                        serde_json::json!({
                                            "name": "headroom_retrieve",
                                            "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. The hash is provided in compression markers like [N items compressed... hash=abc123].",
                                            "input_schema": {
                                                "type": "object",
                                                "properties": {
                                                    "hash": {
                                                        "type": "string",
                                                        "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                                                    }
                                                },
                                                "required": ["hash"]
                                            }
                                        })
                                    }
                                    _ => {
                                        serde_json::json!({
                                            "type": "function",
                                            "function": {
                                                "name": "headroom_retrieve",
                                                "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. The hash is provided in compression markers like [N items compressed... hash=abc123].",
                                                "parameters": {
                                                    "type": "object",
                                                    "properties": {
                                                        "hash": {
                                                            "type": "string",
                                                            "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                                                        }
                                                    },
                                                    "required": ["hash"]
                                                }
                                            }
                                        })
                                    }
                                };
                                tools.push(ccr_tool);
                                changed = true;
                                tracing::debug!(
                                    request_id = %request_id,
                                    "ccr: injected headroom_retrieve tool definition"
                                );
                            }
                        }
                    }

                    // Output shaping: apply verbosity steering and effort
                    // routing to Anthropic-shaped request bodies. Only runs
                    // when the output shaper is enabled in config. The shaping
                    // is idempotent (steering text includes a sentinel prefix)
                    // so repeated applications are safe.
                    if state.config.output_shaper_enabled {
                        let shape_result = crate::output_shaper::shape_request(
                            &mut value,
                            true,
                            state.config.verbosity_level,
                            true,
                            &state.config.mechanical_effort,
                        );
                        if shape_result.changed {
                            changed = true;
                            tracing::debug!(
                                request_id = %request_id,
                                labels = ?shape_result.labels,
                                "output_shaper applied"
                            );
                        }
                    }

                    // Memory: search and inject context into user message tail.
                    // Runs after output shaping, before final serialization.
                    if let Some(handler) = state.memory_handler.as_ref() {
                        let handler = handler.lock().await;
                        if handler.is_initialized() {
                            let provider = match endpoint {
                                compression::CompressibleEndpoint::AnthropicMessages => {
                                    crate::memory::tool_adapter::Provider::Anthropic
                                }
                                compression::CompressibleEndpoint::OpenAiChatCompletions
                                | compression::CompressibleEndpoint::OpenAiResponses => {
                                    crate::memory::tool_adapter::Provider::Openai
                                }
                            };
                            if let Some(messages) = value.get("messages").and_then(|v| v.as_array())
                            {
                                let msgs: Vec<serde_json::Value> = messages.clone();
                                let user_id = headers_snapshot
                                    .as_ref()
                                    .and_then(|h| h.get("x-headroom-user-id"))
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("default");
                                if let Some(context) = handler
                                    .search_and_format_context(
                                        user_id, &msgs, None, // request_context
                                        None, // ranker
                                        None, // query
                                        None, // budget
                                    )
                                    .await
                                {
                                    let frozen = value
                                        .get("system")
                                        .and_then(|v| v.as_array())
                                        .map(|a| a.len())
                                        .unwrap_or(0);
                                    let (new_msgs, bytes) = crate::memory::handler::MemoryHandler::append_to_latest_user_tail(
                                        &msgs,
                                        &context,
                                        provider,
                                        frozen,
                                    );
                                    if bytes > 0 {
                                        if let Some(msgs_val) = value.get_mut("messages") {
                                            *msgs_val = serde_json::Value::Array(new_msgs);
                                            changed = true;
                                            tracing::debug!(
                                                request_id = %request_id,
                                                bytes_appended = bytes,
                                                "memory: injected context into user message tail"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if changed {
                        match serde_json::to_vec(&value) {
                            Ok(bytes) => {
                                if let Some((runtime, records)) = offload_records {
                                    if let Some((workspace_key, _)) = ccr_workspace.as_ref() {
                                        track_ccr_context_records(
                                            &state,
                                            &records,
                                            workspace_key,
                                            &latest_user_query,
                                            turn_number,
                                            &request_id,
                                        );
                                    } else if state.ccr_context_tracker.is_some() {
                                        tracing::info!(
                                            request_id = %request_id,
                                            "CCR Phase 4: workspace unresolved; skipping compression tracking"
                                        );
                                    }
                                    runtime.store.persist(records);
                                }
                                axum::body::Bytes::from(bytes)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    request_id = %request_id,
                                    error = %e,
                                    "ctx transform re-serialization failed; forwarding original body"
                                );
                                buffered
                            }
                        }
                    } else {
                        buffered
                    }
                }
                Err(_) => buffered,
            }
        } else {
            buffered
        };

        // Phase 3: refine the ingestion `gate_decision` now that the body is
        // parsed and `has_messages` is known. The header/config inputs are
        // unchanged from the gate (bypass + master switch already routed
        // bypass/off requests to the streaming arm — we only reach here when
        // both are open), so the only input that can flip the result now is
        // `has_messages` → a `no_messages` passthrough. This single
        // `CompressionDecision` drives BOTH the `compression_decision` tracing
        // event and whether the live-zone dispatchers run — the old ad hoc
        // string literals must not coexist with it.
        let empty_headers = HeaderMap::new();
        let decision_headers = headers_snapshot.as_ref().unwrap_or(&empty_headers);
        let has_messages = request_has_messages(&buffered, endpoint);

        // Validate message array size (mirrors Python MAX_MESSAGE_ARRAY_LENGTH).
        if let Some(count) = message_array_length(&buffered, endpoint) {
            if count > MAX_MESSAGE_ARRAY_LENGTH {
                tracing::warn!(
                    request_id = %request_id,
                    message_count = count,
                    max = MAX_MESSAGE_ARRAY_LENGTH,
                    "request rejected: message array too large"
                );
                return Err(ProxyError::PayloadTooLarge(format!(
                    "Message array too large ({count} messages). Maximum is {MAX_MESSAGE_ARRAY_LENGTH}."
                )));
            }
        }

        let decision = crate::compression_decision::CompressionDecision::decide(
            decision_headers,
            state.config.compression,
            true, // license_allows — TODO(license)
            has_messages,
        );

        // Per-request tags for RequestOutcome: operator `x-headroom-*` slicing
        // tags plus the canonical `passthrough_reason` when this request was
        // passed through uncompressed.
        let mut _tags = crate::headers::extract_tags(decision_headers);
        decision.apply_to_tags(&mut _tags);

        // Freeze-replay: snapshot the ORIGINAL client messages before
        // `buffered` is consumed by the dispatcher arms below. The
        // overlay stage after the dispatcher needs them to decide the
        // append-only guard (previous originals must be an exact
        // canonical prefix of these) and to record this turn for the
        // next one. Anthropic-only, and only when the flag is on so
        // the flag-off path pays zero parse cost.
        let replay_original_messages: Option<Vec<serde_json::Value>> = if state.config.prefix_replay
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            ) {
            serde_json::from_slice::<serde_json::Value>(&buffered)
                .ok()
                .and_then(|v| v.get("messages").and_then(|m| m.as_array().cloned()))
        } else {
            None
        };

        let compression_start = Instant::now();
        let outcome = if !decision.should_compress {
            tracing::info!(
                event = "compression_decision",
                request_id = %request_id,
                path = %path_for_log,
                method = "POST",
                compression_mode = state.config.compression_mode.as_str(),
                decision = "passthrough",
                reason = decision.passthrough_reason.map(|r| r.as_str()).unwrap_or(""),
                body_bytes = buffered.len(),
                "compression passthrough (input-side CompressionDecision)"
            );
            compression::Outcome::NoCompression
        } else {
            match endpoint {
                compression::CompressibleEndpoint::AnthropicMessages => {
                    // PR-E3: thread the F1-classified auth_mode into the
                    // dispatcher so cache_control auto-placement gates on
                    // PAYG only. Pulled from request extensions where it
                    // was stashed at request entry (line ~325 above).
                    // `effective_auth_mode` folds in the enforcement-flag
                    // override so `--auth-mode-policy-enforcement disabled`
                    // also unlocks the Phase E passes for non-PAYG callers.
                    let outcome = compression::compress_anthropic_request(
                        &buffered,
                        state.config.compression_mode,
                        state.config.cache_control_auto_frozen,
                        effective_auth_mode,
                        &request_id,
                        &state.config.exclude_tools,
                    );
                    // Cross-turn verbatim de-dup post-pass over the final
                    // block forms (no-op unless
                    // `--enable-cross-turn-dedup` is set).
                    compression::apply_cross_turn_dedup(
                        outcome,
                        &buffered,
                        &state.config,
                        "/v1/messages",
                        &request_id,
                    )
                }
                compression::CompressibleEndpoint::OpenAiChatCompletions => {
                    let skip = compression::should_skip_compression(&buffered);
                    if skip.is_skip() {
                        tracing::info!(
                            event = "compression_decision",
                            request_id = %request_id,
                            path = "/v1/chat/completions",
                            method = "POST",
                            compression_mode = state.config.compression_mode.as_str(),
                            decision = "passthrough",
                            reason = skip.as_log_str(),
                            body_bytes = buffered.len(),
                            "openai chat compression skipped pre-dispatch"
                        );
                        compression::Outcome::NoCompression
                    } else {
                        let outcome = compression::compress_openai_chat_request(
                            &buffered,
                            state.config.compression_mode,
                            auth_mode,
                            &request_id,
                        );
                        // Cross-turn verbatim de-dup post-pass over
                        // `role == "tool"` message content (no-op unless
                        // `--enable-cross-turn-dedup` is set).
                        compression::apply_cross_turn_dedup(
                            outcome,
                            &buffered,
                            &state.config,
                            "/v1/chat/completions",
                            &request_id,
                        )
                    }
                }
                // PR-C3: OpenAI Responses (`/v1/responses`). The Responses
                // dispatcher walks an explicitly-typed `input` array and
                // only rewrites the latest of each compressible `*_output`
                // kind plus the latest `message` text. Cache hot zone is
                // every other item type (passthrough verbatim).
                compression::CompressibleEndpoint::OpenAiResponses => {
                    compression::compress_openai_responses_request(
                        &buffered,
                        state.config.compression_mode,
                        auth_mode,
                        &request_id,
                    )
                }
            }
        };

        // C2 fix: snapshot the original buffered byte-length AND the
        // dispatcher's "is this a passthrough arm?" decision BEFORE
        // `outcome` is consumed by the match below. The
        // passthrough-bytes-modified alarm fires when a path that
        // promised byte-equal passthrough produces a different
        // length downstream.
        let original_buffered_len = buffered.len();
        let outcome_is_passthrough_class = matches!(
            outcome,
            compression::Outcome::NoCompression | compression::Outcome::Passthrough { .. }
        );
        // Capture compression metadata before the match consumes `outcome`.
        let (compress_tokens_before, compress_tokens_saved, compress_strategies) = match &outcome {
            compression::Outcome::Compressed {
                tokens_before,
                tokens_after,
                strategies_applied,
                ..
            } => (
                *tokens_before as i64,
                (*tokens_before as i64) - (*tokens_after as i64),
                strategies_applied
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            ),
            _ => (0i64, 0i64, Vec::new()),
        };

        // Build OutcomeContext for emit_request_outcome at SSE stream close.
        // Re-parses `buffered` for model/num_messages (cheap, happens once).
        {
            let parsed_body: serde_json::Value =
                serde_json::from_slice(&buffered).unwrap_or_default();
            let model = parsed_body
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            let provider_label = match endpoint {
                compression::CompressibleEndpoint::AnthropicMessages => "anthropic",
                compression::CompressibleEndpoint::OpenAiChatCompletions => "openai_chat",
                compression::CompressibleEndpoint::OpenAiResponses => "openai_responses",
            };
            let num_messages = message_array_length(&buffered, endpoint).unwrap_or(0);

            // Resolve project from headers + system prompt.
            let system_prompt = crate::memory::router::extract_system_prompt(&parsed_body);
            let hdrs = headers_snapshot
                .as_ref()
                .map(|h| {
                    h.iter()
                        .filter_map(|(k, v)| {
                            v.to_str()
                                .ok()
                                .map(|val| (k.as_str().to_lowercase(), val.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let project_ctx = crate::memory::router::RequestContext {
                headers: hdrs,
                system_prompt,
                base_user_id: String::new(),
                project_root_override: None,
            };
            let project = crate::memory::router::ProjectResolver::resolve(&project_ctx)
                .map(|(key, _display)| key);

            // Set thread-local project context for downstream modules
            // (compression feedback, audit, output shaping).
            crate::project_context::set_current_project(project.as_deref());

            // Taken before the struct literal moves `model`.
            let model_for_waste = model.clone();

            outcome_ctx = Some(OutcomeContext {
                sink: Arc::new(ProxyOutcomeSink {
                    cost_tracker: state.cost_tracker.clone(),
                    savings_tracker: state.savings_tracker.clone(),
                    request_logger: state.request_logger.clone(),
                }),
                model,
                provider: provider_label.to_string(),
                tags: _tags.clone(),
                client: None,
                project,
                original_tokens: compress_tokens_before,
                tokens_saved: compress_tokens_saved,
                transforms_applied: compress_strategies.clone(),
                num_messages: num_messages as i64,
                total_latency_ms: start.elapsed().as_millis() as f64,
                // Set once compression has run; see the `stage_timer.record`
                // call below.
                overhead_ms: 0.0,
                started_at: start,
                waste_signals: waste_signals_for_request(&parsed_body, &model_for_waste),
            });
        }

        // Probe recorder: log compression events when HEADROOM_PROBE_RECORD_DIR is set.
        if let Some(ref recorder) = state.probe_recorder {
            if compress_tokens_before > 0 {
                let event = crate::probe_recorder::CompressionEvent {
                    ts: start.elapsed().as_secs_f64(),
                    request_id: request_id.clone(),
                    provider: endpoint_str(&endpoint).to_string(),
                    model: String::new(),
                    tokens_before: Some(compress_tokens_before as u64),
                    tokens_after: Some((compress_tokens_before - compress_tokens_saved) as u64),
                    transforms_applied: compress_strategies.clone(),
                };
                recorder.record(&event);
            }
        }

        // Compression feedback: record per-tool compression patterns for learning.
        if let Some(ref feedback) = state.compression_feedback {
            if compress_tokens_saved > 0 {
                let tool_name = extract_tool_name(&buffered, endpoint);
                let hash = {
                    use sha2::{Digest, Sha256};
                    let digest = Sha256::digest(buffered.as_ref());
                    format!("{digest:x}")
                };
                feedback.record_compression(
                    tool_name.as_deref(),
                    compress_tokens_before as usize,
                    (compress_tokens_before - compress_tokens_saved) as usize,
                    compress_strategies.first().map(|s| s.as_str()),
                    Some(&hash),
                );
            }
        }

        let compression_ms = compression_start.elapsed().as_secs_f64() * 1000.0;
        stage_timer.record("compression", compression_ms);
        // Same number the stage timer just took: this is headroom's own cost,
        // which is what `overhead_ms` means everywhere it is reported.
        if let Some(ctx) = outcome_ctx.as_mut() {
            ctx.overhead_ms = compression_ms;
        }

        let body_to_send = match outcome {
            compression::Outcome::NoCompression => {
                // PR-B2: forward the *original* buffered bytes. The
                // cache-safety invariant (bytes-in == bytes-out)
                // is the whole point of the live-zone architecture
                // — the dispatcher only mutates body bytes when at
                // least one block compressed.
                buffered
            }
            // PR-B3+ produces `Compressed` from the live-zone
            // dispatcher when at least one per-type compressor
            // mutates a block. Already wired here so the next phase
            // is a pure addition.
            compression::Outcome::Compressed {
                body,
                tokens_before,
                tokens_after,
                strategies_applied,
                markers_inserted,
                per_strategy_tokens,
            } => {
                tracing::info!(
                    request_id = %request_id,
                    path = %path_for_log,
                    tokens_before = tokens_before,
                    tokens_after = tokens_after,
                    tokens_freed = tokens_before.saturating_sub(tokens_after),
                    strategies = ?strategies_applied,
                    markers = markers_inserted.len(),
                    "compression applied"
                );
                // Phase G PR-G3 + H1: emit one
                // `proxy_compression_ratio_by_strategy` sample per
                // strategy with the *strategy's own* before/after
                // token counts. The pre-H1 code emitted the same
                // aggregate ratio for every strategy in
                // `strategies_applied`, so Phase H per-strategy
                // dashboards read garbage when multiple strategies
                // ran on one body. We now plumb per-strategy tokens
                // from the manifest at the wrapper site
                // (`live_zone_anthropic`, `live_zone_openai`,
                // `live_zone_responses`).
                //
                // Fallback: when `per_strategy_tokens` is empty —
                // i.e. the Outcome came from a Phase E
                // normalization pass that doesn't track per-strategy
                // tokens — we emit one aggregate-labelled sample so
                // dashboards still see *that* a compression ran. We
                // log loudly so this is visible.
                if !per_strategy_tokens.is_empty() {
                    for entry in &per_strategy_tokens {
                        crate::observability::observe_compression_ratio(
                            entry.strategy,
                            "aggregate",
                            entry.original_tokens,
                            entry.compressed_tokens,
                        );
                    }
                } else if tokens_before > 0 && tokens_after < tokens_before {
                    tracing::debug!(
                        event = "compression_ratio_emit_aggregate_only",
                        request_id = %request_id,
                        path = %path_for_log,
                        strategies = ?strategies_applied,
                        reason = "no_per_strategy_tokens",
                        "emitting one aggregate-labelled compression_ratio sample because \
                         the dispatcher did not surface per-strategy token counts \
                         (Phase E normalization paths)"
                    );
                    crate::observability::observe_compression_ratio(
                        "aggregate",
                        "aggregate",
                        tokens_before,
                        tokens_after,
                    );
                }
                body
            }
            compression::Outcome::Passthrough { reason } => {
                tracing::warn!(
                    request_id = %request_id,
                    path = %path_for_log,
                    reason = ?reason,
                    "compression: passthrough on parse/serialize"
                );
                buffered
            }
        };

        // C2 fix: cache-safety alarm. When the dispatcher returned
        // `NoCompression` or `Passthrough`, the post-dispatcher body
        // MUST be byte-length-equal to the original buffered body.
        // Any delta is an accidental cache-poisoning regression and
        // the alarm metric `proxy_passthrough_bytes_modified_total{path}`
        // fires with the byte delta as its increment. We check BEFORE
        // the PR-E4 prompt_cache_key injector runs because that
        // injector is a legitimate, intentional byte mutation gated
        // on PAYG; it must not trip the alarm.
        if outcome_is_passthrough_class && body_to_send.len() != original_buffered_len {
            let delta = body_to_send.len().abs_diff(original_buffered_len) as u64;
            crate::observability::record_passthrough_bytes_modified(
                &path_for_log,
                delta,
                &request_id,
            );
        }

        // Freeze-replay overlay (ports Python `overlay_cached_prefix` —
        // spec commits #1850 / #1852 / #1868). Runs AFTER the C2
        // passthrough-bytes alarm above because, like PR-E4, it is a
        // legitimate intentional byte mutation: when this turn
        // append-only-extends the previous one, the previously-forwarded
        // (compressed) prefix is replayed byte-identical in place of
        // whatever the dispatcher just produced for the same leading
        // positions, so the provider's prompt cache keeps hitting.
        // `headers_snapshot` is always `Some` on this buffered branch;
        // `replay_original_messages` is `Some` only when the flag is on
        // and the body carried a messages array.
        let body_to_send = match (replay_original_messages, headers_snapshot.as_ref()) {
            // `_headers` is matched, not used: the key was derived once above
            // from the unmutated body. The arm still guards on `Some` because
            // `request_session_key` is empty without headers, which would key
            // every session into one replay slot.
            (Some(original_messages), Some(_headers)) => {
                let session_key = request_session_key.clone();
                apply_prefix_replay(
                    &state.replay_store,
                    &session_key,
                    &request_id,
                    original_messages,
                    body_to_send,
                )
            }
            _ => body_to_send,
        };

        // PR-E4: OpenAI `prompt_cache_key` auto-injection.
        //
        // Universal safety contract: only mutate when the caller
        // is on `AuthMode::Payg`. OAuth/Subscription bytes flow
        // through byte-equal — those clients cannot afford
        // synthesised cache keys (OAuth scopes pin to
        // `(account, model, session)` and subscription clients
        // are programmatically fingerprinted by the upstream).
        //
        // The injector also self-skips when the customer has
        // already set a non-empty `prompt_cache_key`. Every skip
        // path emits a structured `e4_skipped` event so cache-hit
        // dashboards can attribute miss rates to gating reasons
        // rather than guessing.
        let body_to_send = match endpoint {
            compression::CompressibleEndpoint::OpenAiChatCompletions
            | compression::CompressibleEndpoint::OpenAiResponses => {
                let shape = match endpoint {
                    compression::CompressibleEndpoint::OpenAiResponses => {
                        cache_stabilization::openai_cache_key::OpenAiShape::Responses
                    }
                    _ => cache_stabilization::openai_cache_key::OpenAiShape::ChatCompletions,
                };
                maybe_inject_openai_prompt_cache_key(
                    body_to_send,
                    shape,
                    auth_mode,
                    &request_id,
                    &path_for_log,
                )
            }
            compression::CompressibleEndpoint::AnthropicMessages => {
                // Cost-aware model routing (#1706). Runs before sanitisation so
                // a routed id is cleaned too, and before the upstream is chosen
                // so `config.model_routes` sees the model actually being sent.
                // No-op unless the operator configured routes.
                let body_to_send = crate::model_router::apply_to_anthropic_body(
                    body_to_send,
                    &crate::model_router::ModelRouter::new(Some(state.config.model_router.clone())),
                    &request_id,
                );
                // Strip terminal styling artifacts (e.g. a dangling
                // `[1m]` suffix) from `body["model"]` before forwarding;
                // Anthropic-compatible upstreams reject the decorated id.
                let body_to_send =
                    crate::model_sanitize::sanitize_anthropic_model_in_body(body_to_send);
                let body_to_send = if state.config.tool_prune_policy.is_noop() {
                    body_to_send
                } else {
                    maybe_prune_tools(body_to_send, &state.config.tool_prune_policy, &request_id)
                };
                if state.config.context_edit {
                    maybe_inject_context_management(body_to_send, &state.config, &request_id)
                } else {
                    body_to_send
                }
            }
        };

        // Tool schema compaction. Runs last, once tools are final for every
        // endpoint (routing, sanitising, pruning and CCR injection are all
        // done), so what goes on the wire is what gets compacted. Byte-
        // identical passthrough when there is nothing to strip.
        let body_to_send = maybe_compact_tool_schemas(body_to_send, &request_id);

        // B2 tool-order stabilization. Must follow every other tool mutation
        // above, so the order we record is the order the provider caches.
        let body_to_send = if state.config.cache_stable_tool_order
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            ) {
            maybe_stabilize_tool_order(
                body_to_send,
                &state.tool_order_state,
                &request_session_key,
                &request_id,
            )
        } else {
            body_to_send
        };

        // B1 cache-TTL pin. Last of all, so every marker any earlier stage
        // placed or moved is covered. Skipped on PAYG, where a 1h write is
        // priced 60% above a 5m one and the operator pays the difference in
        // dollars rather than in a token-counted usage window.
        let body_to_send = if state.config.force_1h_cache_ttl
            && auth_mode != AuthMode::Payg
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            ) {
            maybe_force_1h_cache_ttl(body_to_send, &request_id)
        } else {
            body_to_send
        };

        // Context-editing: when injecting `context_management` directives we
        // must also advertise the beta so the upstream honours them.
        if state.config.context_edit
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            )
        {
            append_anthropic_beta(
                &mut outgoing_headers,
                crate::compression::context_editing::CONTEXT_MANAGEMENT_BETA,
            );
        }

        // Memory native tool (Anthropic `memory_20250818`): the injected tool
        // is only honoured when the context-management beta is advertised.
        if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
            if let Some(handler) = state.memory_handler.as_ref() {
                let handler = handler.lock().await;
                if handler.is_initialized() {
                    for (name, value) in handler.get_beta_headers() {
                        if name.eq_ignore_ascii_case("anthropic-beta") {
                            append_anthropic_beta(&mut outgoing_headers, &value);
                        }
                    }
                }
            }
        }

        // Turn hooks: pre-send `on_request` seam. Inert (byte-identical)
        // unless a hook is registered — the empty-registry check avoids
        // touching the body at all, matching Python's "inert unless a hook is
        // registered" contract.
        let body_to_send = if crate::turn_hooks::registered_turn_hooks().is_empty() {
            body_to_send
        } else {
            apply_request_hooks(body_to_send, endpoint, &request_id)
        };

        // Forward the request with retry on transient errors (429, 529, 5xx).
        let max_attempts = if state.config.retry_enabled {
            state.config.retry_max_attempts.max(1)
        } else {
            1
        };
        let mut last_err: Option<ProxyError> = None;
        {
            let mut result = None;
            for attempt in 0..max_attempts {
                let resp = state
                    .client
                    .request(reqwest_method.clone(), upstream_url.clone())
                    .headers(outgoing_headers.clone())
                    .body(body_to_send.clone())
                    .send()
                    .await;
                match resp {
                    Ok(r) => {
                        let status = r.status().as_u16();
                        // 429 = rate-limited, 529 = Anthropic overloaded, 5xx = server error
                        let is_retryable =
                            status == 429 || status == 529 || (500..600).contains(&status);
                        if is_retryable && attempt + 1 < max_attempts {
                            let max_delay = state.config.retry_max_delay_ms;
                            let retry_after = r
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| {
                                    // Try numeric seconds first (most common)
                                    if let Ok(secs) = v.parse::<u64>() {
                                        return Some((secs * 1000).min(max_delay));
                                    }
                                    // Try HTTP-date format (RFC 7231 §7.1.3)
                                    // e.g. "Wed, 21 Oct 2015 07:28:00 GMT"
                                    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(v) {
                                        let now = chrono::Utc::now();
                                        let diff = date
                                            .signed_duration_since(now)
                                            .num_milliseconds()
                                            .max(0)
                                            as u64;
                                        return Some(diff.min(max_delay));
                                    }
                                    None
                                });
                            let delay_ms = retry_after.unwrap_or_else(|| {
                                let base = state.config.retry_base_delay_ms;
                                let max = state.config.retry_max_delay_ms;
                                let backoff = base.saturating_mul(1u64 << attempt).min(max);
                                // Apply 50-150% jitter to prevent thundering-herd
                                let jitter = 50u64
                                    + (std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .subsec_nanos()
                                        as u64
                                        % 101);
                                backoff.saturating_mul(jitter) / 100
                            });
                            tracing::warn!(
                                request_id = %request_id,
                                status = status,
                                attempt = attempt + 1,
                                max_attempts = max_attempts,
                                delay_ms = delay_ms,
                                "upstream returned retryable status; retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            continue;
                        }
                        result = Some(r);
                        break;
                    }
                    Err(e) => {
                        let is_retryable = is_retryable_transport_error(&e);
                        if is_retryable && attempt + 1 < max_attempts {
                            let base = state.config.retry_base_delay_ms;
                            let max = state.config.retry_max_delay_ms;
                            let backoff = base.saturating_mul(1u64 << attempt).min(max);
                            // Apply 50-150% jitter to prevent thundering-herd
                            let jitter = 50u64
                                + (std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .subsec_nanos() as u64
                                    % 101);
                            let delay_ms = backoff.saturating_mul(jitter) / 100;
                            tracing::warn!(
                                request_id = %request_id,
                                error = %e,
                                attempt = attempt + 1,
                                max_attempts = max_attempts,
                                delay_ms = delay_ms,
                                "upstream error retryable; retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            last_err = Some(ProxyError::Upstream(e));
                            continue;
                        }
                        return Err(ProxyError::Upstream(e));
                    }
                }
            }
            result.ok_or_else(|| {
                last_err.unwrap_or_else(|| {
                    ProxyError::InvalidUpstream("retry loop exhausted".to_string())
                })
            })?
        }
    } else {
        // Pure streaming path — the original passthrough behaviour.
        let body_stream =
            TryStreamExt::map_err(req.into_body().into_data_stream(), std::io::Error::other);
        let reqwest_body = reqwest::Body::wrap_stream(body_stream);
        state
            .client
            .request(reqwest_method, upstream_url.clone())
            .headers(outgoing_headers.clone())
            .body(reqwest_body)
            .send()
            .await?
    };

    let upstream_status = upstream_resp.status();
    let mut status =
        StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    // PR-A8 / P5-57: capture the upstream request id BEFORE we move
    // `upstream_resp.headers()` into the response filter. Anthropic
    // emits `request-id` (lowercase, no `x-`); OpenAI emits
    // `x-request-id`. We forward both to the client unchanged in
    // `resp_headers` and additionally surface a side-channel
    // `headroom-request-id` header so callers can correlate proxy
    // logs without conflating with the proxy's own `x-request-id`.
    let upstream_request_id_anthropic = upstream_resp
        .headers()
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let upstream_request_id_openai = upstream_resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // Prefer the provider-specific id whichever was set. Both
    // present is unusual but legal; prefer Anthropic since it's the
    // path-shape we lockdown with cache invariants.
    let upstream_request_id = upstream_request_id_anthropic
        .clone()
        .or_else(|| upstream_request_id_openai.clone());

    // PR-C1: detect SSE responses so the state machine can run in
    // parallel with the byte-passthrough. We classify ONCE here and
    // pick the response provider arm based on the request path —
    // bytes flow to the client unchanged; the state machine sinks
    // bytes into a `tokio::sync::mpsc` and runs in a spawned task
    // that can never block the byte path.
    //
    // PR-C4: the OpenAI Responses arm is gated by
    // `enable_responses_streaming`. When that flag is false the
    // tee is short-circuited to `None` so the framer + state
    // machine don't spin up and bytes flow opaquely. Other
    // providers' state machines are unaffected.
    let is_sse = is_sse_response(upstream_resp.headers());
    let sse_kind = if is_sse {
        let kind = SseStreamKind::for_request_path(&path_for_log);
        if matches!(kind, SseStreamKind::OpenAiResponses)
            && !state.config.enable_responses_streaming
        {
            tracing::info!(
                request_id = %request_id,
                path = %path_for_log,
                event = "responses_streaming_state_machine_skipped",
                reason = "enable_responses_streaming=false",
                "PR-C4 streaming pipeline disabled; SSE bytes pass through without telemetry"
            );
            SseStreamKind::None
        } else {
            kind
        }
    } else {
        SseStreamKind::None
    };

    let resp_headers = filter_response_headers(upstream_resp.headers());

    // Phase G PR-G3: extract upstream rate-limit headers from this
    // response and record them as gauges. The `provider` label is
    // chosen by which of the upstream `request-id` shapes we saw
    // (Anthropic vs OpenAI). When neither shape was detected we
    // skip emission rather than guessing — per realignment build-
    // constraint "no silent fallbacks".
    let rate_limit_snapshot =
        crate::observability::extract_rate_limit_snapshot(upstream_resp.headers());
    let rate_limit_provider: Option<&'static str> = if upstream_request_id_anthropic.is_some() {
        Some(crate::observability::cache_hit_rate_provider::ANTHROPIC)
    } else if upstream_request_id_openai.is_some() {
        // We can't distinguish chat vs responses purely from the
        // request-id header; the `path_for_log` is more specific.
        Some(if path_for_log.contains("/v1/responses") {
            crate::observability::cache_hit_rate_provider::OPENAI_RESPONSES
        } else {
            crate::observability::cache_hit_rate_provider::OPENAI_CHAT
        })
    } else {
        None
    };
    if let Some(provider) = rate_limit_provider {
        crate::observability::record_rate_limit_snapshot(
            provider,
            &rate_limit_snapshot,
            &request_id,
        );
    } else if rate_limit_snapshot.remaining_requests.is_some()
        || rate_limit_snapshot.remaining_tokens.is_some()
        || rate_limit_snapshot.remaining_input_tokens.is_some()
        || rate_limit_snapshot.remaining_output_tokens.is_some()
    {
        // Headers present but provider unattributable. Log loud so
        // operators see the wire-format drift; do not emit unlabelled
        // metrics.
        tracing::debug!(
            event = "rate_limit_snapshot_unattributable",
            request_id = %request_id,
            path = %path_for_log,
            "rate-limit headers present but provider couldn't be inferred; skipping gauge emit"
        );
    }

    // Subscription / OAuth traffic carries the `anthropic-ratelimit-
    // unified-*` family instead of `*-remaining` — the headers above
    // stay None on a Claude-subscription plan, so the `*-remaining`
    // gauges never populate. Parse + record the unified family too so
    // subscription headroom (utilization per 5h/7d window) is visible.
    // Provider-agnostic: the unified prefix is Anthropic-specific, so a
    // non-empty snapshot is self-attributing.
    let unified_snapshot =
        crate::observability::extract_unified_rate_limit(upstream_resp.headers());
    if !unified_snapshot.windows.is_empty()
        || unified_snapshot.overall_status.is_some()
        || unified_snapshot.fallback_percentage.is_some()
    {
        crate::observability::record_unified_rate_limit(&unified_snapshot, &request_id);
    }

    // Stream response body back without buffering. Wrap errors so mid-stream
    // upstream failures are logged rather than silently truncating the client.
    //
    // PR-C1: when this is an SSE response, tee each chunk into a
    // bounded mpsc so the spawned state-machine task can update
    // telemetry without ever holding up the client. The mpsc is
    // bounded; if the parser falls behind, `try_send` fails and we
    // log + drop — the byte path is not affected. This is the
    // explicit "never block on parser readiness" contract.
    let rid = request_id.clone();
    let parser_tx = if !matches!(sse_kind, SseStreamKind::None) {
        let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(SSE_PARSER_QUEUE_DEPTH);
        let rid_for_parser = request_id.clone();
        // Freeze-replay: hand the state machine a store handle so the
        // Anthropic arm can feed the final usage's cache tokens back
        // into the session tracker (`SessionReplayStore::complete`) —
        // the request→response correlation the Python handler did
        // inline. `None` when the feature is off so the flag-off path
        // is observably unchanged.
        let replay_store_for_parser = if state.config.prefix_replay {
            Some(state.replay_store.clone())
        } else {
            None
        };
        tokio::spawn(run_sse_state_machine(
            sse_kind,
            rx,
            rid_for_parser,
            state.usage_observer.clone(),
            outcome_ctx.clone(),
            replay_store_for_parser,
        ));
        Some(tx)
    } else {
        None
    };
    let resp_stream = upstream_resp.bytes_stream().map(move |r| match r {
        Ok(b) => {
            if let Some(tx) = &parser_tx {
                if let Err(e) = tx.try_send(b.clone()) {
                    tracing::debug!(
                        request_id = %rid,
                        error = %e,
                        "sse parser queue full or closed; skipping telemetry chunk"
                    );
                }
            }
            Ok(b)
        }
        Err(e) => {
            tracing::warn!(request_id = %rid, error = %e, "upstream stream error mid-response");
            Err(e)
        }
    });

    // For non-SSE successful responses, buffer the body so we can
    // cache it. SSE responses stream through without buffering.
    // NOTE: `sse_kind == None` also covers SSE streams on paths with no
    // telemetry parser (e.g. arbitrary upstream SSE endpoints), so gate on
    // `is_sse` — buffering an unbounded SSE stream never completes and
    // breaks client-disconnect propagation.
    let should_buffer_for_cache = !is_sse && status.is_success();
    let body = if should_buffer_for_cache {
        // Wrap the mapped stream into a hyper Body so BodyExt::collect can
        // buffer it. This is only for non-SSE success responses where we
        // want to cache the full body.
        let body_stream = Body::from_stream(resp_stream);
        match http_body_util::BodyExt::collect(body_stream).await {
            Ok(collected) => {
                let mut body_bytes = collected.to_bytes();

                // CCR response handling: detect headroom_retrieve tool
                // calls, fetch from CCR store, and continue conversation.
                if state.config.ccr_handle_responses
                    && !body_bytes.is_empty()
                    && state.ctx_offload.is_some()
                {
                    // Derive the CCR provider shape from the request path so
                    // interception fires for all three provider shapes:
                    // Anthropic messages, OpenAI chat-completions, and OpenAI
                    // Responses. Each has a distinct request/response layout.
                    let ccr_provider = if path_for_log.contains("/v1/messages") {
                        Some("anthropic")
                    } else if path_for_log.contains("/v1/chat/completions") {
                        Some("openai")
                    } else if path_for_log.contains("/v1/responses") {
                        Some("openai_responses")
                    } else {
                        None
                    };
                    if let Some(ccr_provider) = ccr_provider {
                        let ccr_store = state.ctx_offload.as_ref().unwrap().store.ccr();
                        body_bytes = handle_ccr_response(
                            &body_bytes,
                            &original_buffered,
                            &upstream_url,
                            &state.client,
                            ccr_store.as_ref(),
                            &state.config,
                            &request_id,
                            &outgoing_headers,
                            ccr_provider,
                        )
                        .await;
                    }
                }

                // Turn hooks: post-response `on_response` seam. Inert
                // (byte-identical) unless a hook is registered — the
                // empty-registry check skips parsing entirely. Covers the
                // buffered non-SSE success path for every provider.
                if !body_bytes.is_empty() && !crate::turn_hooks::registered_turn_hooks().is_empty()
                {
                    let provider = if path_for_log.contains("/v1/messages") {
                        "anthropic"
                    } else {
                        "openai"
                    };
                    body_bytes = apply_response_hooks(
                        body_bytes,
                        &original_buffered,
                        provider,
                        &upstream_url,
                        &state.client,
                        &outgoing_headers,
                        &request_id,
                    )
                    .await;
                }

                // Cache the response for future identical requests.
                if let Some(ref cache) = state.semantic_cache {
                    if let Ok(parsed) =
                        serde_json::from_slice::<serde_json::Value>(&original_buffered)
                    {
                        let is_streaming = parsed
                            .get("stream")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if !is_streaming && !body_bytes.is_empty() {
                            let model = parsed
                                .get("model")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown");
                            let messages = parsed
                                .get("messages")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let mut extra = serde_json::Map::new();
                            for key in &[
                                "system",
                                "tools",
                                "tool_choice",
                                "temperature",
                                "top_p",
                                "top_k",
                                "max_tokens",
                                "stop",
                            ] {
                                if let Some(val) = parsed.get(*key) {
                                    extra.insert(key.to_string(), val.clone());
                                }
                            }
                            let response_headers: std::collections::HashMap<String, String> =
                                resp_headers
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        v.to_str()
                                            .ok()
                                            .map(|val| (k.as_str().to_string(), val.to_string()))
                                    })
                                    .collect();
                            cache.set(
                                &messages,
                                model,
                                body_bytes.to_vec(),
                                response_headers,
                                0,
                                &extra,
                            );
                            tracing::debug!(
                                event = "semantic_cache_set",
                                request_id = %request_id,
                                model = model,
                                body_bytes = body_bytes.len(),
                                "cached non-streaming response"
                            );
                        }
                    }
                }

                // Non-streaming outcome recording. The SSE state machine
                // emits a `RequestOutcome` at stream close for streaming
                // responses; the buffered (non-streaming) path had no
                // equivalent, so PERF/savings/cost/cache metrics were all
                // silently dropped for backend-routed non-streaming traffic.
                // Parse the buffered body's `usage` block (shape depends on
                // provider) and emit the same outcome the SSE sites build.
                if let Some(ref ctx) = outcome_ctx {
                    if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                        let usage = parsed.get("usage");
                        let get_i64 = |u: Option<&serde_json::Value>, key: &str| -> i64 {
                            u.and_then(|v| v.get(key))
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0)
                        };
                        // (attempted_input, output, cache_read, cache_write)
                        let (attempted_input, output_tok, cache_read, cache_write) =
                            match ctx.provider.as_str() {
                                "anthropic" => (
                                    get_i64(usage, "input_tokens"),
                                    get_i64(usage, "output_tokens"),
                                    get_i64(usage, "cache_read_input_tokens"),
                                    get_i64(usage, "cache_creation_input_tokens"),
                                ),
                                "openai_responses" => {
                                    let cached = usage
                                        .and_then(|u| u.get("input_tokens_details"))
                                        .and_then(|d| d.get("cached_tokens"))
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or(0);
                                    (
                                        get_i64(usage, "input_tokens"),
                                        get_i64(usage, "output_tokens"),
                                        cached,
                                        0,
                                    )
                                }
                                // openai_chat (and any other) shape.
                                _ => {
                                    let cached = usage
                                        .and_then(|u| u.get("prompt_tokens_details"))
                                        .and_then(|d| d.get("cached_tokens"))
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or(0);
                                    (
                                        get_i64(usage, "prompt_tokens"),
                                        get_i64(usage, "completion_tokens"),
                                        cached,
                                        0,
                                    )
                                }
                            };
                        // Anthropic's `input_tokens` already excludes cache
                        // reads and writes, so it *is* the uncached count.
                        // Both OpenAI shapes report a total that includes the
                        // cached prefix, so there the read has to come off.
                        let uncached_input = if ctx.provider == "anthropic" {
                            attempted_input
                        } else {
                            attempted_input.saturating_sub(cache_read)
                        };
                        let outcome = headroom_core::request_outcome::RequestOutcome {
                            request_id: request_id.clone(),
                            provider: ctx.provider.clone(),
                            model: ctx.model.clone(),
                            status_code: status.as_u16() as i64,
                            original_tokens: ctx.original_tokens,
                            optimized_tokens: ctx.original_tokens.saturating_sub(ctx.tokens_saved),
                            output_tokens: output_tok,
                            tokens_saved: ctx.tokens_saved,
                            attempted_input_tokens: attempted_input,
                            cache_read_tokens: cache_read,
                            cache_write_tokens: cache_write,
                            uncached_input_tokens: uncached_input,
                            total_latency_ms: ctx.total_latency_ms,
                            overhead_ms: ctx.overhead_ms,
                            // `ttfb_ms` stays at its 0 default: the convention
                            // is 0 for non-streaming, and this path has the
                            // whole body buffered before it runs.
                            transforms_applied: ctx.transforms_applied.clone(),
                            num_messages: ctx.num_messages,
                            tags: ctx.tags.clone(),
                            client: ctx.client.clone(),
                            project: ctx.project.clone(),
                            ..Default::default()
                        };
                        headroom_core::request_outcome::emit_request_outcome(
                            ctx.sink.as_ref(),
                            &outcome,
                        );
                    }
                }

                Body::from(body_bytes)
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    error = %e,
                    "failed to buffer non-SSE response"
                );
                // Can't recover the stream after partial consumption. Surface
                // a gateway error rather than the upstream 2xx with a
                // plain-text body that JSON clients would choke on.
                status = StatusCode::BAD_GATEWAY;
                Body::from(format!("upstream response buffering failed: {e}"))
            }
        }
    } else {
        Body::from_stream(resp_stream)
    };

    let mut response = Response::builder().status(status);
    {
        let h = response.headers_mut().expect("builder has headers");
        h.extend(resp_headers);
        // Echo X-Request-Id back to the client.
        if let Ok(v) = http::HeaderValue::from_str(&request_id) {
            h.insert(HeaderName::from_static("x-request-id"), v);
        }
        // PR-A8 / P5-57: surface the upstream id in a distinct
        // header so it's never conflated with the proxy's own.
        if let Some(uid) = upstream_request_id.as_deref() {
            if let Ok(v) = http::HeaderValue::from_str(uid) {
                h.insert(HeaderName::from_static("headroom-upstream-request-id"), v);
            }
        }
    }
    let response = response
        .body(body)
        .map_err(|e| ProxyError::InvalidHeader(e.to_string()))?;

    tracing::info!(
        request_id = %request_id,
        upstream_request_id = upstream_request_id.as_deref().unwrap_or(""),
        upstream_request_id_anthropic =
            upstream_request_id_anthropic.as_deref().unwrap_or(""),
        upstream_request_id_openai =
            upstream_request_id_openai.as_deref().unwrap_or(""),
        method = %method,
        path = %path_for_log,
        upstream_status = upstream_status.as_u16(),
        latency_ms = start.elapsed().as_millis() as u64,
        protocol = "http",
        "forwarded"
    );

    // Emit stage timings for observability.
    crate::stage_timer::emit_stage_timings_log(
        &path_for_log,
        &request_id,
        "",
        &stage_timer,
        &["buffer", "compression", "upstream"],
    );

    Ok(response)
}

/// Bound on the in-flight queue between the byte-passthrough and the
/// SSE state-machine task. Picked so that under steady-state streaming
/// load (~5 events/100ms typical) the parser is never blocked on
/// queue space, yet a stalled parser can't grow memory unboundedly.
/// Tunable via `proxy.toml` if a deployment finds this insufficient.
const SSE_PARSER_QUEUE_DEPTH: usize = 256;

/// Which provider's state machine should run on this stream. Picked
/// from the *request* path because the response content-type
/// (`text/event-stream`) is identical across providers.
#[derive(Debug, Clone, Copy)]
enum SseStreamKind {
    None,
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl SseStreamKind {
    fn for_request_path(path: &str) -> Self {
        match path {
            "/v1/messages" => Self::Anthropic,
            "/v1/chat/completions" => Self::OpenAiChat,
            "/v1/responses" => Self::OpenAiResponses,
            // No telemetry parser registered for this endpoint.
            // We still pass bytes through unchanged.
            _ => Self::None,
        }
    }
}

/// True if the upstream response is an SSE stream. Compares
/// `content-type` against `text/event-stream` (with optional
/// parameters). RFC 7231 §3.1.1.1: media types compare
/// case-insensitive on the type/subtype tokens.
fn is_sse_response(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let media_type = s.split(';').next().unwrap_or("").trim();
            media_type.eq_ignore_ascii_case("text/event-stream")
        })
        .unwrap_or(false)
}

/// Freeze-replay request stage (Anthropic `/v1/messages` buffered path).
///
/// Rust port of the Python handler's overlay call site
/// (`headroom/proxy/handlers/anthropic.py`, around the
/// `overlay_cached_prefix` / `normalize_message_cache_control` pair):
///
/// 1. Overlay the previously-forwarded prefix byte-identical onto this
///    turn's dispatcher output when this turn append-only-extends the
///    previous one (#1850). Idempotent and append-only-guarded, so it
///    is safe to run unconditionally on the flagged path.
/// 2. Own message-level `cache_control` placement (#1852): the client
///    moves its breakpoint every turn and the overlay replays past
///    markers, so without normalization they accumulate ~1/turn and
///    Anthropic hard-errors at >4. Applied last so the forwarded AND
///    recorded (next turn's replay source) messages stay bounded.
/// 3. Park `(original, forwarded)` under `request_id` so the SSE
///    completion side can feed the response's cache tokens back via
///    [`SessionReplayStore::complete`].
///
/// The body is re-serialized only when the replay actually changed the
/// messages; otherwise the dispatcher's bytes forward untouched.
/// (`serde_json` runs with `preserve_order`, so a re-serialization
/// keeps key order — the same property Python gets from `dict`.)
///
/// Visible crate-wide so the routed-model translate path replays its prefix
/// through this exact code rather than a parallel implementation — the whole
/// value of the stage is that the replayed bytes are byte-identical, which a
/// second implementation would be one refactor away from breaking.
pub(crate) fn apply_prefix_replay(
    store: &SessionReplayStore,
    session_key: &str,
    request_id: &str,
    original_messages: Vec<serde_json::Value>,
    body: bytes::Bytes,
) -> bytes::Bytes {
    use cache_stabilization::prefix_replay::{
        normalize_message_cache_control, overlay_cached_prefix,
    };

    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "prefix_replay_skipped",
                request_id = %request_id,
                error = %e,
                "prefix replay: post-dispatch body is not JSON; forwarding unchanged"
            );
            return body;
        }
    };
    let Some(optimized) = parsed.get("messages").and_then(|m| m.as_array()).cloned() else {
        tracing::debug!(
            event = "prefix_replay_skipped",
            request_id = %request_id,
            reason = "no_messages_array",
            "prefix replay: post-dispatch body has no messages array; forwarding unchanged"
        );
        return body;
    };

    let (prev_orig, prev_fwd) = match store.previous_turn(session_key) {
        Some((o, f)) => (Some(o), Some(f)),
        None => (None, None),
    };
    let overlaid = overlay_cached_prefix(
        optimized.clone(),
        &original_messages,
        prev_orig.as_deref(),
        prev_fwd.as_deref(),
    );
    let replayed_prefix = overlaid != optimized;
    let normalized = normalize_message_cache_control(overlaid);
    let changed = normalized != optimized;

    let (final_body, forwarded_messages) = if changed {
        parsed["messages"] = serde_json::Value::Array(normalized.clone());
        match serde_json::to_vec(&parsed) {
            Ok(b) => {
                tracing::info!(
                    event = "prefix_replay_applied",
                    request_id = %request_id,
                    replayed_prefix = replayed_prefix,
                    "prefix replay: forwarded messages rewritten \
                     (prefix replay and/or cache_control normalization)"
                );
                (bytes::Bytes::from(b), normalized)
            }
            Err(e) => {
                // Record what we ACTUALLY forward (the pre-replay
                // bytes), never the messages we failed to serialize —
                // the store must mirror the wire.
                tracing::warn!(
                    event = "prefix_replay_serialize_failed",
                    request_id = %request_id,
                    error = %e,
                    "prefix replay: re-serialization failed; forwarding pre-replay body"
                );
                (body, optimized)
            }
        }
    } else {
        (body, optimized)
    };

    store.begin_request(
        request_id,
        session_key,
        original_messages,
        forwarded_messages,
    );
    final_body
}

/// PR-E4: OpenAI `prompt_cache_key` auto-injection helper.
///
/// Gates on [`AuthMode::Payg`] and the in-body
/// `prompt_cache_key` skip rule, parses the body once, mutates if
/// appropriate, and re-serialises. Returns the original `body` on
/// any non-applicable path — every error / skip leaves the bytes
/// untouched (Phase A passthrough invariant).
///
/// Logs `e4_skipped` for each skip reason and `e4_applied` with
/// only the first [`KEY_PREFIX_LOG_LEN`] hex chars of the key
/// (never the full key, which is identifying material).
///
/// [`KEY_PREFIX_LOG_LEN`]: cache_stabilization::openai_cache_key::KEY_PREFIX_LOG_LEN
fn maybe_inject_openai_prompt_cache_key(
    body: bytes::Bytes,
    shape: cache_stabilization::openai_cache_key::OpenAiShape,
    auth_mode: AuthMode,
    request_id: &str,
    path: &str,
) -> bytes::Bytes {
    use cache_stabilization::openai_cache_key::{
        inject_prompt_cache_key, InjectOutcome, SkipReason,
    };

    // Auth-mode gate: only PAYG bodies are eligible. OAuth /
    // Subscription requests pass through byte-equal — synthesised
    // cache keys would look like cache-evasion to the upstream
    // and could void OAuth scopes pinned to `(account, model,
    // session)`.
    if !matches!(auth_mode, AuthMode::Payg) {
        tracing::info!(
            event = "e4_skipped",
            request_id = %request_id,
            path = %path,
            reason = "auth_mode",
            auth_mode = auth_mode.as_str(),
            "PR-E4: skipped prompt_cache_key injection (non-PAYG auth mode)"
        );
        return body;
    }

    // Parse for the inject step. Failure here is silent — the
    // dispatcher above already logged the parse outcome on its
    // own decision path; we don't want to double-log. The body
    // round-trips unchanged.
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return body;
        }
    };

    match inject_prompt_cache_key(&mut parsed, shape) {
        InjectOutcome::Applied { key_prefix } => {
            // Re-serialise. If serialization fails (would be very
            // unusual — we just successfully parsed), fall back
            // to the original bytes. No-silent-fallback rule: log
            // it loudly so a regression can't hide.
            match serde_json::to_vec(&parsed) {
                Ok(buf) => {
                    tracing::info!(
                        event = "e4_applied",
                        request_id = %request_id,
                        path = %path,
                        key_prefix = %key_prefix,
                        body_bytes_in = body.len(),
                        body_bytes_out = buf.len(),
                        "PR-E4: injected prompt_cache_key"
                    );
                    bytes::Bytes::from(buf)
                }
                Err(e) => {
                    tracing::error!(
                        event = "e4_serialize_error",
                        request_id = %request_id,
                        path = %path,
                        error = %e,
                        "PR-E4: re-serialize after injection failed; forwarding original bytes"
                    );
                    body
                }
            }
        }
        InjectOutcome::Skipped { reason } => {
            // Log only the customer-visible KeyPresent skip; the
            // NotAnObject skip is structurally impossible past
            // the dispatcher gate but is surfaced separately for
            // operators chasing pathological inputs.
            match reason {
                SkipReason::KeyPresent => {
                    tracing::info!(
                        event = "e4_skipped",
                        request_id = %request_id,
                        path = %path,
                        reason = SkipReason::KeyPresent.as_str(),
                        "PR-E4: skipped prompt_cache_key injection (customer-set value preserved)"
                    );
                }
                SkipReason::NotAnObject => {
                    tracing::warn!(
                        event = "e4_skipped",
                        request_id = %request_id,
                        path = %path,
                        reason = SkipReason::NotAnObject.as_str(),
                        "PR-E4: body is not a JSON object; passthrough"
                    );
                }
            }
            body
        }
    }
}

/// Metadata threaded from `forward_http` into the SSE state-machine task so
/// it can build a [`headroom_core::request_outcome::RequestOutcome`] and call
/// [`headroom_core::request_outcome::emit_request_outcome`] at stream close.
#[derive(Clone)]
struct OutcomeContext {
    sink: Arc<ProxyOutcomeSink>,
    model: String,
    provider: String,
    tags: std::collections::HashMap<String, String>,
    client: Option<String>,
    project: Option<String>,
    original_tokens: i64,
    tokens_saved: i64,
    transforms_applied: Vec<String>,
    num_messages: i64,
    total_latency_ms: f64,
    /// Time headroom itself spent on this request (compression and transforms),
    /// as distinct from time waiting on the upstream. Filled in after the
    /// compression stage completes, so it is 0 on paths that never compress.
    overhead_ms: f64,
    /// When the request entered the proxy. Used to derive TTFB at the moment
    /// the first upstream byte arrives, which is the only place that is
    /// observable.
    started_at: std::time::Instant,
    /// Per-signal waste token counts for this request, if the message body
    /// could be parsed. `None` means "not measured", which is distinct from
    /// "measured and found nothing".
    waste_signals: Option<Vec<(String, i64)>>,
}

/// Latch time-to-first-byte on the first upstream chunk. Every SSE arm calls
/// this from its receive loop; the value is written once and never overwritten.
fn latch_ttfb(ttfb_ms: &mut f64, outcome_ctx: &Option<OutcomeContext>) {
    if *ttfb_ms == 0.0 {
        if let Some(ctx) = outcome_ctx.as_ref() {
            *ttfb_ms = ctx.started_at.elapsed().as_secs_f64() * 1000.0;
        }
    }
}

/// Drive the per-provider state machine over a stream of byte chunks.
/// Lives in its own task; the byte path never waits on it.
async fn run_sse_state_machine(
    kind: SseStreamKind,
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    request_id: String,
    usage_observer: Arc<cache_stabilization::usage_observer::UsageObserver>,
    outcome_ctx: Option<OutcomeContext>,
    replay_store: Option<SessionReplayStore>,
) {
    use crate::sse::framing::SseFramer;

    let mut framer = SseFramer::new();
    // Time to first byte from upstream. Only the first chunk marks it, so it is
    // latched once and never overwritten. Declared outside the match because
    // every arm needs it — leaving it in one arm made the other providers
    // report a 0 that the histogram then silently dropped.
    let mut ttfb_ms: f64 = 0.0;
    // The state machines are different types; rather than introducing
    // a trait object dance, run each variant in its own arm. The dead
    // branches compile out cleanly and the hot path stays monomorphic.
    match kind {
        SseStreamKind::Anthropic => {
            let mut state = crate::sse::anthropic::AnthropicStreamState::new();
            while let Some(chunk) = rx.recv().await {
                latch_ttfb(&mut ttfb_ms, &outcome_ctx);
                framer.push(&chunk);
                while let Some(ev_result) = framer.next_event() {
                    match ev_result {
                        Ok(ev) => {
                            if let Err(e) = state.apply(ev) {
                                tracing::warn!(
                                    request_id = %request_id,
                                    error = %e,
                                    "sse anthropic state-machine apply error"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                request_id = %request_id,
                                error = %e,
                                "sse framer error"
                            );
                        }
                    }
                }
            }
            // Phase G PR-G3 + H2: emit per-session cache-hit-rate
            // ONLY when the stream completed cleanly with
            // `message_stop`. The gate is encapsulated by the
            // pure function `compute_anthropic_session_hit_rate`
            // so the H2 contract has a unit-testable surface.
            match crate::observability::cache_hit_rate::compute_anthropic_session_hit_rate(&state) {
                Some(rate) => {
                    crate::observability::observe_cache_hit_rate(
                        crate::observability::cache_hit_rate_provider::ANTHROPIC,
                        &request_id,
                        rate,
                    );
                }
                None => {
                    tracing::debug!(
                        event = "cache_hit_rate_skipped",
                        request_id = %request_id,
                        provider = "anthropic",
                        status = ?state.status,
                        input_tokens = state.usage.input_tokens,
                        cache_read_input_tokens = state.usage.cache_read_input_tokens,
                        cache_creation_input_tokens = state.usage.cache_creation_input_tokens,
                        "skipping proxy_cache_hit_rate_per_session: H2 gate or zero denominator"
                    );
                }
            }
            // CTX-7: feed the re-cache watchdog with this turn's
            // billed usage. Same H2 gate as the hit-rate metric: only
            // a cleanly completed stream (`message_stop`) carries
            // trustworthy final usage.
            if state.status == crate::sse::anthropic::StreamStatus::MessageStop {
                usage_observer.complete(
                    &request_id,
                    state.usage.input_tokens,
                    state.usage.cache_read_input_tokens,
                    state.usage.cache_creation_input_tokens,
                );
            }
            // Freeze-replay: feed the completed turn's cache tokens
            // back into the session tracker so the next turn can
            // replay the forwarded prefix (Python parity:
            // `update_from_response` after every API call). Gated on
            // clean completion for the same reason as the H2 gate
            // above — a half-finished stream may be a client
            // disconnect with unreliable usage totals. `complete` is
            // a no-op when this request was never parked
            // (non-Anthropic, or the buffered path didn't run).
            if let Some(store) = &replay_store {
                if state.status == crate::sse::anthropic::StreamStatus::MessageStop {
                    store.complete(
                        &request_id,
                        state.usage.cache_read_input_tokens,
                        state.usage.cache_creation_input_tokens,
                    );
                }
            }
            tracing::info!(
                request_id = %request_id,
                provider = "anthropic",
                input_tokens = state.usage.input_tokens,
                output_tokens = state.usage.output_tokens,
                cache_creation_input_tokens = state.usage.cache_creation_input_tokens,
                cache_read_input_tokens = state.usage.cache_read_input_tokens,
                stop_reason = state.stop_reason.as_deref().unwrap_or(""),
                blocks = state.blocks.len(),
                "sse stream closed"
            );
            if let Some(ref ctx) = outcome_ctx {
                let outcome = headroom_core::request_outcome::RequestOutcome {
                    request_id: request_id.clone(),
                    provider: ctx.provider.clone(),
                    model: ctx.model.clone(),
                    original_tokens: ctx.original_tokens,
                    optimized_tokens: ctx.original_tokens.saturating_sub(ctx.tokens_saved),
                    output_tokens: state.usage.output_tokens as i64,
                    tokens_saved: ctx.tokens_saved,
                    attempted_input_tokens: state.usage.input_tokens as i64,
                    cache_read_tokens: state.usage.cache_read_input_tokens as i64,
                    cache_write_tokens: state.usage.cache_creation_input_tokens as i64,
                    cache_write_5m_tokens: state.usage.cache_creation_5m_input_tokens as i64,
                    cache_write_1h_tokens: state.usage.cache_creation_1h_input_tokens as i64,
                    // Anthropic's `input_tokens` already excludes cache reads
                    // and writes, so it *is* the uncached count. Python's
                    // Bedrock path has to subtract instead, because there
                    // `input_tokens` is the total — do not copy that formula
                    // here.
                    uncached_input_tokens: state.usage.input_tokens as i64,
                    waste_signals: ctx.waste_signals.clone(),
                    total_latency_ms: ctx.total_latency_ms,
                    overhead_ms: ctx.overhead_ms,
                    ttfb_ms,
                    transforms_applied: ctx.transforms_applied.clone(),
                    num_messages: ctx.num_messages,
                    tags: ctx.tags.clone(),
                    client: ctx.client.clone(),
                    project: ctx.project.clone(),
                    ..Default::default()
                };
                headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
            }
        }
        SseStreamKind::OpenAiChat => {
            let mut state = crate::sse::openai_chat::ChunkState::new();
            while let Some(chunk) = rx.recv().await {
                latch_ttfb(&mut ttfb_ms, &outcome_ctx);
                framer.push(&chunk);
                while let Some(ev_result) = framer.next_event() {
                    match ev_result {
                        Ok(ev) => {
                            if let Err(e) = state.apply(ev) {
                                tracing::warn!(
                                    request_id = %request_id,
                                    error = %e,
                                    "sse openai_chat state-machine apply error"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                request_id = %request_id,
                                error = %e,
                                "sse framer error"
                            );
                        }
                    }
                }
            }
            // Phase G PR-G3: emit cache-hit-rate from the final usage
            // chunk. OpenAI only emits this when
            // `stream_options.include_usage = true`; absence is a
            // signal, not a fallback condition — `usage = None` →
            // skip. The H2 gate is implicit here: the final usage
            // chunk only arrives when the stream completed (it's
            // OpenAI's terminal-status equivalent).
            if let Some(usage) = &state.usage {
                let input_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cached_tokens = usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                // M1: `cached_tokens > input_tokens` is a wire-
                // format pathology — log + skip instead of silently
                // clamping (saturating_sub would yield 0 → fake 1.0
                // hit-rate sample).
                if cached_tokens > input_tokens {
                    tracing::warn!(
                        event = "cache_hit_rate_skipped",
                        request_id = %request_id,
                        provider = "openai_chat",
                        reason = "cached_gt_input",
                        input_tokens = input_tokens,
                        cached_tokens = cached_tokens,
                        "skipping proxy_cache_hit_rate_per_session: cached_tokens > prompt_tokens \
                         (wire-format pathology; clamping would synthesise a bad sample)"
                    );
                } else {
                    // OpenAI's `prompt_tokens` already INCLUDES cached
                    // tokens (per Chat Completions API docs), so the
                    // denominator is `prompt_tokens`, not the sum. The
                    // numerator is `cached_tokens`; `input_tokens` arg
                    // to `compute_cache_hit_rate` carries the
                    // *non-cached* portion (denom-only), so we
                    // synthesise that here.
                    let non_cached = input_tokens - cached_tokens;
                    match crate::observability::compute_cache_hit_rate(non_cached, cached_tokens, 0)
                    {
                        Some(rate) => {
                            crate::observability::observe_cache_hit_rate(
                                crate::observability::cache_hit_rate_provider::OPENAI_CHAT,
                                &request_id,
                                rate,
                            );
                        }
                        None => {
                            tracing::debug!(
                                event = "cache_hit_rate_skipped",
                                request_id = %request_id,
                                provider = "openai_chat",
                                reason = "zero_denominator",
                                "skipping proxy_cache_hit_rate_per_session: no input tokens"
                            );
                        }
                    }
                }
            } else {
                tracing::debug!(
                    event = "cache_hit_rate_skipped",
                    request_id = %request_id,
                    provider = "openai_chat",
                    reason = "no_usage_chunk",
                    "skipping proxy_cache_hit_rate_per_session: stream_options.include_usage=false"
                );
            }
            tracing::info!(
                request_id = %request_id,
                provider = "openai_chat",
                choices = state.choices.len(),
                has_usage = state.usage.is_some(),
                "sse stream closed"
            );
            if let Some(ref ctx) = outcome_ctx {
                let (input_tok, cached_tok, output_tok) = if let Some(ref usage) = state.usage {
                    let input = usage
                        .get("prompt_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let cached = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let output = usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    (input, cached, output)
                } else {
                    (0, 0, 0)
                };
                let outcome = headroom_core::request_outcome::RequestOutcome {
                    request_id: request_id.clone(),
                    provider: ctx.provider.clone(),
                    model: ctx.model.clone(),
                    original_tokens: ctx.original_tokens,
                    optimized_tokens: ctx.original_tokens.saturating_sub(ctx.tokens_saved),
                    output_tokens: output_tok,
                    tokens_saved: ctx.tokens_saved,
                    attempted_input_tokens: input_tok,
                    cache_read_tokens: cached_tok,
                    // `prompt_tokens` is the total and includes the cached
                    // prefix, unlike Anthropic's `input_tokens`.
                    uncached_input_tokens: input_tok.saturating_sub(cached_tok),
                    total_latency_ms: ctx.total_latency_ms,
                    overhead_ms: ctx.overhead_ms,
                    ttfb_ms,
                    transforms_applied: ctx.transforms_applied.clone(),
                    num_messages: ctx.num_messages,
                    tags: ctx.tags.clone(),
                    client: ctx.client.clone(),
                    project: ctx.project.clone(),
                    ..Default::default()
                };
                headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
            }
        }
        SseStreamKind::OpenAiResponses => {
            let mut state = crate::sse::openai_responses::ResponseState::new();
            while let Some(chunk) = rx.recv().await {
                latch_ttfb(&mut ttfb_ms, &outcome_ctx);
                framer.push(&chunk);
                while let Some(ev_result) = framer.next_event() {
                    match ev_result {
                        Ok(ev) => {
                            if let Err(e) = state.apply(ev) {
                                tracing::warn!(
                                    request_id = %request_id,
                                    error = %e,
                                    "sse openai_responses state-machine apply error"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                request_id = %request_id,
                                error = %e,
                                "sse framer error"
                            );
                        }
                    }
                }
            }
            // Phase G PR-G3 + H2: cache hit rate + service_tier +
            // response status emit ONLY when the stream reached a
            // terminal status (`response.completed/failed/incomplete`).
            // Mid-stream client disconnects close the channel without
            // a terminal — `terminal_status().is_none()` then guards
            // emit so we don't observe garbage samples.
            //
            // The Responses API uses `input_tokens` /
            // `cached_input_tokens` shape (Responses-specific —
            // distinct from Chat Completions' `prompt_tokens`).
            let stream_completed = state.terminal_status().is_some();
            if stream_completed {
                if let Some(usage) = &state.usage {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cached_tokens = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    // M1: a cached count greater than input is a
                    // wire-format pathology — usage shouldn't have
                    // `cached > input` for OpenAI Responses. Per
                    // "no silent fallbacks", log + skip the emit
                    // instead of silently clamping.
                    if cached_tokens > input_tokens {
                        tracing::warn!(
                            event = "cache_hit_rate_skipped",
                            request_id = %request_id,
                            provider = "openai_responses",
                            reason = "cached_gt_input",
                            input_tokens = input_tokens,
                            cached_tokens = cached_tokens,
                            "skipping proxy_cache_hit_rate_per_session: cached_tokens > input_tokens \
                             (wire-format pathology; clamping would synthesise a bad sample)"
                        );
                    } else {
                        // Like Chat, `input_tokens` already INCLUDES cached
                        // tokens, so split for the helper.
                        let non_cached = input_tokens - cached_tokens;
                        match crate::observability::compute_cache_hit_rate(
                            non_cached,
                            cached_tokens,
                            0,
                        ) {
                            Some(rate) => {
                                crate::observability::observe_cache_hit_rate(
                                    crate::observability::cache_hit_rate_provider::OPENAI_RESPONSES,
                                    &request_id,
                                    rate,
                                );
                            }
                            None => {
                                tracing::debug!(
                                    event = "cache_hit_rate_skipped",
                                    request_id = %request_id,
                                    provider = "openai_responses",
                                    reason = "zero_denominator",
                                    "skipping proxy_cache_hit_rate_per_session: no input tokens"
                                );
                            }
                        }
                    }
                }
            } else {
                tracing::debug!(
                    event = "cache_hit_rate_skipped",
                    request_id = %request_id,
                    provider = "openai_responses",
                    reason = "stream_did_not_complete",
                    "skipping proxy_cache_hit_rate_per_session: no terminal status seen"
                );
            }
            // Service tier + status are sourced from
            // `state.last_response_envelope` populated by the
            // ResponseState on `response.completed/failed/incomplete`.
            //
            // C1 fix: the tier value comes from the upstream response
            // body; even though the upstream is more trustworthy than
            // a client-side header, an unrecognised value would still
            // grow the metric vector unboundedly. We bucket through
            // the same validator the request-side handler uses.
            if let Some(tier) = state.service_tier.as_deref() {
                let bucketed = crate::observability::metric_names::service_tier::validate(tier);
                crate::observability::record_service_tier(bucketed, &request_id);
            }
            if let Some(status) = state.terminal_status() {
                crate::observability::record_response_status(
                    status,
                    state.incomplete_reason.as_deref(),
                    &request_id,
                );
            }
            tracing::info!(
                request_id = %request_id,
                provider = "openai_responses",
                items = state.items.len(),
                has_usage = state.usage.is_some(),
                service_tier = state.service_tier.as_deref().unwrap_or(""),
                terminal_status = state.terminal_status().unwrap_or(""),
                incomplete_reason = state.incomplete_reason.as_deref().unwrap_or(""),
                "sse stream closed"
            );
            if let Some(ref ctx) = outcome_ctx {
                let (input_tok, cached_tok, output_tok) = if let Some(ref usage) = state.usage {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let cached = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    (input, cached, output)
                } else {
                    (0, 0, 0)
                };
                let outcome = headroom_core::request_outcome::RequestOutcome {
                    request_id: request_id.clone(),
                    provider: ctx.provider.clone(),
                    model: ctx.model.clone(),
                    original_tokens: ctx.original_tokens,
                    optimized_tokens: ctx.original_tokens.saturating_sub(ctx.tokens_saved),
                    output_tokens: output_tok,
                    tokens_saved: ctx.tokens_saved,
                    attempted_input_tokens: input_tok,
                    cache_read_tokens: cached_tok,
                    // `input_tokens` here is the total and includes the cached
                    // prefix, unlike Anthropic's field of the same name.
                    uncached_input_tokens: input_tok.saturating_sub(cached_tok),
                    total_latency_ms: ctx.total_latency_ms,
                    overhead_ms: ctx.overhead_ms,
                    ttfb_ms,
                    transforms_applied: ctx.transforms_applied.clone(),
                    num_messages: ctx.num_messages,
                    tags: ctx.tags.clone(),
                    client: ctx.client.clone(),
                    project: ctx.project.clone(),
                    ..Default::default()
                };
                headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
            }
        }
        SseStreamKind::None => {}
    }
}

pub(crate) fn ensure_request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

// ─── Turn hooks ───────────────────────────────────────────────────────────

/// Provider label for a compressible endpoint, as `turn_hooks::TurnContext`
/// expects it.
fn turn_hook_provider(endpoint: compression::CompressibleEndpoint) -> &'static str {
    match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => "anthropic",
        compression::CompressibleEndpoint::OpenAiChatCompletions
        | compression::CompressibleEndpoint::OpenAiResponses => "openai",
    }
}

/// Pre-send `on_request` seam. Parses the outbound body, lets registered hooks
/// inspect/mutate `messages`/`tools`, and re-serializes only if a hook changed
/// them. Returns the body unchanged on any parse/serialize failure. Callers
/// MUST gate on a non-empty registry so the empty-registry path is a
/// byte-identical no-op (this fn re-serializes and would perturb bytes).
fn apply_request_hooks(
    body: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    request_id: &str,
) -> bytes::Bytes {
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let messages = parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tools = parsed.get("tools").cloned();

    let mut ctx = crate::turn_hooks::TurnContext {
        provider: turn_hook_provider(endpoint).to_string(),
        model,
        messages,
        tools,
        config: None,
    };
    crate::turn_hooks::run_request_hooks(&mut ctx);

    // Write mutated messages/tools back onto the body.
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert(
            "messages".to_string(),
            serde_json::Value::Array(ctx.messages),
        );
        match ctx.tools {
            Some(t) => {
                obj.insert("tools".to_string(), t);
            }
            None => {
                obj.remove("tools");
            }
        }
    }
    match serde_json::to_vec(&parsed) {
        Ok(v) => bytes::Bytes::from(v),
        Err(e) => {
            tracing::warn!(request_id = %request_id, error = %e, "turn hooks: re-serialize failed; forwarding original body");
            body
        }
    }
}

/// `call_model` implementation for turn hooks: re-drives the upstream model via
/// the same buffered POST path the CCR continuation loop uses. Built from the
/// original request body (used as a template — its `messages` array is replaced
/// with whatever the hook passes) plus the live upstream url/client/headers.
struct ProxyCallModel {
    template: serde_json::Value,
    upstream_url: url::Url,
    client: reqwest::Client,
    headers: http::HeaderMap,
    request_id: String,
}

#[async_trait::async_trait]
impl crate::turn_hooks::CallModel for ProxyCallModel {
    async fn call(&self, messages: Vec<serde_json::Value>) -> serde_json::Value {
        let mut body = self.template.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("messages".to_string(), serde_json::Value::Array(messages));
        }
        let body_bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(request_id = %self.request_id, error = %e, "turn hooks call_model: serialize failed");
                return serde_json::Value::Null;
            }
        };
        let resp = self
            .client
            .post(self.upstream_url.clone())
            .headers(self.headers.clone())
            .body(body_bytes)
            .send()
            .await;
        match resp {
            Ok(r) => match r.bytes().await {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    tracing::warn!(request_id = %self.request_id, error = %e, "turn hooks call_model: read body failed");
                    serde_json::Value::Null
                }
            },
            Err(e) => {
                tracing::warn!(request_id = %self.request_id, error = %e, "turn hooks call_model: upstream request failed");
                serde_json::Value::Null
            }
        }
    }
}

/// Post-response `on_response` seam. Runs registered hooks over the buffered
/// upstream response, giving them a `call_model` that re-drives this same turn.
/// Returns the (possibly replaced) body bytes; unchanged on parse/serialize
/// failure. Callers MUST gate on a non-empty registry (byte-identical no-op).
async fn apply_response_hooks(
    body_bytes: bytes::Bytes,
    original_request: &bytes::Bytes,
    provider: &str,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    headers: &http::HeaderMap,
    request_id: &str,
) -> bytes::Bytes {
    let response: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => return body_bytes,
    };
    let template: serde_json::Value = match serde_json::from_slice(original_request) {
        Ok(v) => v,
        Err(_) => return body_bytes,
    };
    let model = template
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let messages = template
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tools = template.get("tools").cloned();

    let ctx = crate::turn_hooks::TurnContext {
        provider: provider.to_string(),
        model,
        messages,
        tools,
        config: None,
    };
    let call_model = ProxyCallModel {
        template,
        upstream_url: upstream_url.clone(),
        client: client.clone(),
        headers: headers.clone(),
        request_id: request_id.to_string(),
    };
    let out = crate::turn_hooks::run_response_hooks(&ctx, response, &call_model).await;
    match serde_json::to_vec(&out) {
        Ok(v) => bytes::Bytes::from(v),
        Err(_) => body_bytes,
    }
}

// ─── CCR Response Handling ────────────────────────────────────────────────

/// Detect CCR tool calls in a buffered upstream response, fetch original
/// content from the CCR store, and continue the conversation until the
/// LLM produces a response without CCR calls (or max rounds is hit).
///
/// Returns the final response body bytes. Only operates on non-streaming,
/// Anthropic-shaped responses for now (the primary CCR path).
///
/// NOTE: The compression pipeline is NOT re-applied to continuation
/// requests. The original request was already compressed before being
/// sent upstream; continuation requests append tool results to the
/// already-compressed message array. Re-compressing would be wasteful
/// and could break cache keys.
/// Append a CCR continuation entry to the running item array.
///
/// Most provider shapes return a single message dict, which is pushed as-is.
/// Some providers (OpenAI chat-completions tool results, OpenAI Responses
/// turns/tool results) return a sentinel-keyed wrapper `{ "_sentinel": [..] }`
/// whose list must be spliced into the array. If `entry` is such a wrapper for
/// any of `sentinel_keys`, its list is extended in; otherwise `entry` is pushed.
fn extend_or_push(
    items: &mut Vec<serde_json::Value>,
    entry: serde_json::Value,
    sentinel_keys: &[&str],
) {
    if let Some(obj) = entry.as_object() {
        for key in sentinel_keys {
            if let Some(list) = obj.get(*key).and_then(|v| v.as_array()) {
                items.extend(list.iter().cloned());
                return;
            }
        }
    }
    items.push(entry);
}

async fn handle_ccr_response(
    body_bytes: &bytes::Bytes,
    original_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    ccr_store: &dyn headroom_core::ccr::CcrStore,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
) -> bytes::Bytes {
    // The continuation-array field name varies by provider shape: Anthropic
    // and OpenAI chat-completions both use `messages`; OpenAI Responses uses
    // `input`.
    let items_field = if provider == "openai_responses" {
        "input"
    } else {
        "messages"
    };
    use headroom_core::ccr::response_handler::{CCRResponseHandler, CcrToolResult};

    let handler = CCRResponseHandler::new(Some(
        headroom_core::ccr::response_handler::ResponseHandlerConfig {
            enabled: true,
            max_retrieval_rounds: config.ccr_max_retrieval_rounds,
            strip_ccr_from_response: false,
        },
    ));

    // Parse the response body.
    let response: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(
                request_id = %request_id,
                error = %e,
                "ccr: failed to parse upstream response as JSON; skipping CCR handling"
            );
            return body_bytes.clone();
        }
    };

    if !handler.has_ccr_tool_calls(&response, provider) {
        return body_bytes.clone();
    }

    let mut current_response = response.clone();
    let mut current_request: serde_json::Value = match serde_json::from_slice(original_request) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                request_id = %request_id,
                error = %e,
                "ccr: failed to parse original request; skipping CCR handling"
            );
            return body_bytes.clone();
        }
    };

    let max_rounds = config.ccr_max_retrieval_rounds;
    let mut rounds = 0;

    loop {
        if rounds >= max_rounds {
            tracing::warn!(
                request_id = %request_id,
                rounds = rounds,
                "ccr: max retrieval rounds reached; returning partial response"
            );
            break;
        }

        let (ccr_calls, other_calls) = handler.parse_ccr_tool_calls(&current_response, provider);

        if ccr_calls.is_empty() {
            break;
        }

        // If there are mixed CCR + real tool calls, we can't fabricate
        // results for the real tools — return as-is.
        if !other_calls.is_empty() {
            tracing::info!(
                request_id = %request_id,
                ccr_count = ccr_calls.len(),
                other_count = other_calls.len(),
                "ccr: mixed CCR and real tool calls; cannot auto-resolve"
            );
            break;
        }

        // Fetch original content for each CCR call.
        let mut results: Vec<CcrToolResult> = Vec::new();
        for call in &ccr_calls {
            match ccr_store.get(&call.hash_key) {
                Some(content) => {
                    results.push(CcrToolResult {
                        tool_call_id: call.tool_call_id.clone(),
                        content,
                        success: true,
                        items_retrieved: 1,
                    });
                    tracing::debug!(
                        request_id = %request_id,
                        hash = %call.hash_key,
                        "ccr: retrieved original content"
                    );
                }
                None => {
                    results.push(CcrToolResult {
                        tool_call_id: call.tool_call_id.clone(),
                        content: format!(
                            "Error: CCR content not found for hash '{}'. The compressed data may have been evicted.",
                            call.hash_key
                        ),
                        success: false,
                        items_retrieved: 0,
                    });
                    tracing::warn!(
                        request_id = %request_id,
                        hash = %call.hash_key,
                        "ccr: content not found in store"
                    );
                }
            }
        }

        // Build continuation messages: append assistant message + tool results.
        //
        // Some providers return sentinel-keyed shapes (a wrapper dict holding a
        // list of items) rather than a single message dict, because their turn
        // history is a flat item array rather than one role/content entry. When
        // we see such a sentinel, extend the continuation array with its list
        // instead of pushing the whole wrapper as one entry.
        let assistant_msg = handler.extract_assistant_message(&current_response, provider);
        let tool_result_msg = handler.create_tool_result_message(&results, provider);

        if let Some(items) = current_request
            .get_mut(items_field)
            .and_then(|v| v.as_array_mut())
        {
            extend_or_push(items, assistant_msg, &["_openai_responses_output_items"]);
            extend_or_push(
                items,
                tool_result_msg,
                &["_openai_tool_results", "_openai_responses_tool_results"],
            );
        } else {
            tracing::warn!(
                request_id = %request_id,
                field = items_field,
                "ccr: no continuation array in request; cannot continue"
            );
            break;
        }

        // Re-send to upstream.
        let continuation_body = match serde_json::to_vec(&current_request) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    error = %e,
                    "ccr: failed to serialize continuation request"
                );
                break;
            }
        };

        tracing::info!(
            request_id = %request_id,
            round = rounds + 1,
            max_rounds = max_rounds,
            results_count = results.len(),
            "ccr: sending continuation request"
        );

        let resp = match client
            .post(upstream_url.clone())
            .headers(outgoing_headers.clone())
            .body(continuation_body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    error = %e,
                    "ccr: upstream request failed during continuation"
                );
                break;
            }
        };

        if !resp.status().is_success() {
            tracing::warn!(
                request_id = %request_id,
                status = %resp.status(),
                "ccr: upstream returned error during continuation"
            );
            break;
        }

        match resp.bytes().await {
            Ok(bytes) => {
                current_response = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            request_id = %request_id,
                            error = %e,
                            "ccr: failed to parse continuation response"
                        );
                        break;
                    }
                };
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    error = %e,
                    "ccr: failed to read continuation response body"
                );
                break;
            }
        }

        rounds += 1;
    }

    // Classify the outcome before returning. Only claim success when no
    // `headroom_retrieve` remains: on an intentional mixed-tool skip (#839) the
    // response still carries one for the CLIENT to resolve, and logging
    // "handled successfully" there would misreport correct behaviour as a clean
    // resolution. A retrieve left with no client tool call is a genuine failure.
    match handler.residual_ccr_status(&current_response, provider) {
        headroom_core::ccr::response_handler::RESIDUAL_CCR_RESOLVED => {
            tracing::info!(request_id = %request_id, "ccr: retrieval handled successfully");
        }
        headroom_core::ccr::response_handler::RESIDUAL_CCR_SKIPPED_MIXED => {
            tracing::info!(
                request_id = %request_id,
                "ccr: skipped retrieval — headroom_retrieve returned alongside a \
                 client tool for the client to resolve"
            );
        }
        status => {
            tracing::warn!(
                request_id = %request_id,
                status = %status,
                "ccr: headroom_retrieve remains unresolved with no client tool call"
            );
        }
    }

    match serde_json::to_vec(&current_response) {
        Ok(bytes) => bytes::Bytes::from(bytes),
        Err(_) => body_bytes.clone(),
    }
}

/// Test-only helper: drain a body to bytes (uses BodyExt).
#[cfg(test)]
pub async fn body_to_bytes(body: Body) -> Result<Bytes, axum::Error> {
    use axum::Error;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(Error::new)
}

fn endpoint_str(endpoint: &compression::CompressibleEndpoint) -> &'static str {
    match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => "anthropic",
        compression::CompressibleEndpoint::OpenAiChatCompletions => "openai_chat",
        compression::CompressibleEndpoint::OpenAiResponses => "openai_responses",
    }
}

fn extract_tool_name(body: &[u8], endpoint: compression::CompressibleEndpoint) -> Option<String> {
    use sha2::{Digest, Sha256};
    match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => {
            let v: serde_json::Value = serde_json::from_slice(body).ok()?;
            let tool = v.get("tool")?;
            tool.get("name")?.as_str().map(String::from)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end unit test for `handle_ccr_response` on the OpenAI Responses
    /// shape: a `function_call` for `headroom_retrieve` in the upstream
    /// `output[]` must be intercepted server-side, resolved against the CCR
    /// store, and a continuation request re-sent (with `input[]` extended by
    /// the assistant output items + `function_call_output` items) whose reply
    /// is returned to the client. Mirrors the Anthropic CCR interception path.
    #[tokio::test]
    async fn handle_ccr_response_openai_responses_runs_continuation() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
        use headroom_core::ccr::CcrStore;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Store some original content the model will retrieve.
        let store = InMemoryCcrStore::new();
        let hash = "abc123def456abc123def456";
        store.put(hash, "the original large content");

        // Mock upstream: the continuation call returns a plain Responses reply
        // with no further CCR calls, so the loop terminates after one round.
        let server = MockServer::start().await;
        let final_body = serde_json::json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]}
            ]
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(final_body.clone()))
            .mount(&server)
            .await;

        // Original request (Responses shape uses `input[]`).
        let original_request = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-x",
                "input": [{"type": "message", "role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );

        // Upstream's first reply: a headroom_retrieve function_call.
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "output": [
                    {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                     "arguments": format!("{{\"hash\":\"{hash}\"}}")}
                ]
            }))
            .unwrap(),
        );

        let config = Config::for_test(server.uri().parse().unwrap());
        let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
        let client = reqwest::Client::new();
        let headers = http::HeaderMap::new();

        let out = handle_ccr_response(
            &upstream_reply,
            &original_request,
            &upstream_url,
            &client,
            &store as &dyn headroom_core::ccr::CcrStore,
            &config,
            "req-test",
            &headers,
            "openai_responses",
        )
        .await;

        // The returned body is the continuation reply (no CCR calls), proving
        // interception happened rather than passing the retrieve call through.
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["output"][0]["content"][0]["text"], "done");
    }

    #[test]
    fn extend_or_push_splices_sentinel_and_pushes_plain() {
        let mut items = vec![serde_json::json!({"role": "user"})];
        // Plain entry is pushed as one.
        extend_or_push(
            &mut items,
            serde_json::json!({"role": "assistant"}),
            &["_openai_responses_output_items"],
        );
        // Sentinel wrapper is spliced.
        extend_or_push(
            &mut items,
            serde_json::json!({"_openai_responses_tool_results": [{"a": 1}, {"a": 2}]}),
            &["_openai_tool_results", "_openai_responses_tool_results"],
        );
        assert_eq!(items.len(), 4);
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[2]["a"], 1);
        assert_eq!(items[3]["a"], 2);
    }

    #[test]
    fn url_build_basic() {
        let base: url::Url = "http://up:8080".parse().unwrap();
        let uri: Uri = "/v1/messages?stream=true".parse().unwrap();
        let out = build_upstream_url(&base, &uri).unwrap();
        assert_eq!(out.as_str(), "http://up:8080/v1/messages?stream=true");
    }

    /// Annotation keys are billed on every turn because tools are resent
    /// each request. Compaction strips them before the body goes upstream.
    #[test]
    fn compact_tool_schemas_strips_annotation_keys() {
        let body = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "m",
                "tools": [{
                    "name": "search",
                    "description": "Search  the\tweb.",
                    "input_schema": {
                        "type": "object",
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "title": "SearchArgs",
                        "properties": {
                            "query": {"type": "string", "title": "Query", "examples": ["a"]}
                        }
                    }
                }]
            }))
            .unwrap(),
        );
        let out = maybe_compact_tool_schemas(body.clone(), "req-test");
        assert!(out.len() < body.len(), "compaction must shrink the body");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let schema = &v["tools"][0]["input_schema"];
        assert!(schema.get("$schema").is_none(), "$schema must be stripped");
        assert!(schema.get("title").is_none(), "title must be stripped");
        assert!(schema["properties"]["query"].get("examples").is_none());
        // Non-tool fields are untouched.
        assert_eq!(v["model"], "m");
    }

    /// A request with nothing to strip must forward the ORIGINAL bytes, not a
    /// re-serialized equivalent — re-serializing perturbs the cache prefix.
    #[test]
    fn compact_tool_schemas_is_byte_identical_passthrough_when_clean() {
        for payload in [
            serde_json::json!({"model": "m", "messages": []}),
            serde_json::json!({
                "model": "m",
                "tools": [{
                    "name": "x",
                    "description": "Clean desc.",
                    "input_schema": {"type": "object"}
                }]
            }),
        ] {
            let original = bytes::Bytes::from(serde_json::to_vec(&payload).unwrap());
            let out = maybe_compact_tool_schemas(original.clone(), "req-test");
            assert_eq!(out, original, "clean body must pass through byte-identical");
        }
    }

    #[test]
    fn compact_tool_schemas_passthrough_on_unparseable_body() {
        let original = bytes::Bytes::from(b"not json at all".to_vec());
        assert_eq!(
            maybe_compact_tool_schemas(original.clone(), "req-test"),
            original
        );
    }

    #[test]
    fn maybe_prune_tools_drops_and_reserializes() {
        use crate::cache_stabilization::tool_prune::PrunePolicy;
        let body = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "m",
                "tools": [
                    {"name": "Read", "input_schema": {}},
                    {"name": "mcp__chrome__click", "input_schema": {}}
                ]
            }))
            .unwrap(),
        );
        let policy = PrunePolicy {
            drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let out = maybe_prune_tools(body, &policy, "req-test");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "Read");
    }

    /// B2 end to end through the wiring function: turn one records, turn two
    /// pushes a late-arriving tool to the tail. The rest of the body must
    /// survive the reserialize untouched.
    #[test]
    fn maybe_stabilize_tool_order_replays_then_appends() {
        use crate::cache_stabilization::tool_order::ToolOrderStore;
        let store = ToolOrderStore::default();
        let body = |tools: serde_json::Value| {
            bytes::Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "model": "claude-opus-4-8",
                    "system": "s",
                    "max_tokens": 64,
                    "tools": tools,
                }))
                .unwrap(),
            )
        };
        let names = |b: &bytes::Bytes| {
            let v: serde_json::Value = serde_json::from_slice(b).unwrap();
            v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        let first = body(serde_json::json!([{"name": "a"}, {"name": "b"}]));
        let out = maybe_stabilize_tool_order(first.clone(), &store, "sess", "req-test");
        assert_eq!(out, first, "first turn only records; bytes must not move");

        let second = body(serde_json::json!([{"name": "a"}, {"name": "late"}, {"name": "b"}]));
        let out = maybe_stabilize_tool_order(second, &store, "sess", "req-test");
        assert_eq!(names(&out), ["a", "b", "late"]);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["max_tokens"], 64);
        assert_eq!(v["system"], "s");
    }

    /// A different model on the same session key must not inherit the other
    /// model's order — the credential-derived session key is shared between a
    /// main agent and its subagents.
    #[test]
    fn maybe_stabilize_tool_order_keys_on_model() {
        use crate::cache_stabilization::tool_order::ToolOrderStore;
        let store = ToolOrderStore::default();
        let body = |model: &str, tools: serde_json::Value| {
            bytes::Bytes::from(
                serde_json::to_vec(&serde_json::json!({"model": model, "tools": tools})).unwrap(),
            )
        };
        let sub = body(
            "claude-sonnet-4-6",
            serde_json::json!([{"name": "b"}, {"name": "a"}]),
        );
        maybe_stabilize_tool_order(sub, &store, "sess", "req-test");

        let main = body(
            "claude-opus-4-8",
            serde_json::json!([{"name": "a"}, {"name": "b"}, {"name": "c"}]),
        );
        let out = maybe_stabilize_tool_order(main.clone(), &store, "sess", "req-test");
        assert_eq!(out, main, "subagent order must not leak across models");
    }

    /// No `tools` array — nothing to stabilize, and the body must not even be
    /// reserialized.
    #[test]
    fn maybe_stabilize_tool_order_passthrough_without_tools() {
        use crate::cache_stabilization::tool_order::ToolOrderStore;
        let store = ToolOrderStore::default();
        let original = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model": "m", "messages": []})).unwrap(),
        );
        assert_eq!(
            maybe_stabilize_tool_order(original.clone(), &store, "sess", "req-test"),
            original
        );
    }

    /// Without a session key every conversation would share one store slot and
    /// replay each other's tool order. Passthrough instead.
    #[test]
    fn maybe_stabilize_tool_order_needs_a_session_key() {
        use crate::cache_stabilization::tool_order::ToolOrderStore;
        let store = ToolOrderStore::default();
        let body = |tools: serde_json::Value| {
            bytes::Bytes::from(
                serde_json::to_vec(&serde_json::json!({"model": "m", "tools": tools})).unwrap(),
            )
        };
        maybe_stabilize_tool_order(
            body(serde_json::json!([{"name": "a"}, {"name": "b"}])),
            &store,
            "",
            "req-test",
        );
        let shuffled = body(serde_json::json!([{"name": "b"}, {"name": "a"}]));
        assert_eq!(
            maybe_stabilize_tool_order(shuffled.clone(), &store, "", "req-test"),
            shuffled
        );
    }

    #[test]
    fn maybe_prune_tools_passthrough_when_no_tools_field() {
        use crate::cache_stabilization::tool_prune::PrunePolicy;
        let original = bytes::Bytes::from(br#"{"model":"m","messages":[]}"#.to_vec());
        let policy = PrunePolicy {
            drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let out = maybe_prune_tools(original.clone(), &policy, "req-test");
        assert_eq!(out, original, "no tools[] -> byte-identical passthrough");
    }

    #[test]
    fn maybe_prune_tools_passthrough_when_nothing_removed() {
        use crate::cache_stabilization::tool_prune::PrunePolicy;
        let original = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "tools": [{"name": "Read", "input_schema": {}}]
            }))
            .unwrap(),
        );
        let policy = PrunePolicy {
            drop_mcp_servers: ["chrome"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let out = maybe_prune_tools(original.clone(), &policy, "req-test");
        assert_eq!(
            out, original,
            "nothing matched -> byte-identical passthrough"
        );
    }

    #[test]
    fn url_build_with_base_path() {
        let base: url::Url = "http://up:8080/api".parse().unwrap();
        let uri: Uri = "/v1/messages".parse().unwrap();
        let out = build_upstream_url(&base, &uri).unwrap();
        assert_eq!(out.as_str(), "http://up:8080/api/v1/messages");
    }

    #[test]
    fn url_build_root() {
        let base: url::Url = "http://up:8080/".parse().unwrap();
        let uri: Uri = "/".parse().unwrap();
        let out = build_upstream_url(&base, &uri).unwrap();
        assert_eq!(out.as_str(), "http://up:8080/");
    }

    // ── Phase 3: request_has_messages (CompressionDecision `has_messages`) ──

    use compression::CompressibleEndpoint;

    #[test]
    fn has_messages_true_for_nonempty_anthropic_messages() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(request_has_messages(
            body,
            CompressibleEndpoint::AnthropicMessages
        ));
        assert!(request_has_messages(
            body,
            CompressibleEndpoint::OpenAiChatCompletions
        ));
    }

    #[test]
    fn has_messages_false_for_empty_messages_array() {
        let body = br#"{"messages":[]}"#;
        assert!(!request_has_messages(
            body,
            CompressibleEndpoint::AnthropicMessages
        ));
    }

    #[test]
    fn has_messages_false_when_field_missing() {
        let body = br#"{"model":"m"}"#;
        assert!(!request_has_messages(
            body,
            CompressibleEndpoint::AnthropicMessages
        ));
    }

    #[test]
    fn has_messages_uses_input_field_for_responses() {
        let body = br#"{"input":[{"role":"user","content":"hi"}]}"#;
        assert!(request_has_messages(
            body,
            CompressibleEndpoint::OpenAiResponses
        ));
        // `messages` on a Responses body is not the field consulted.
        let wrong = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(!request_has_messages(
            wrong,
            CompressibleEndpoint::OpenAiResponses
        ));
    }

    #[test]
    fn has_messages_false_on_parse_failure() {
        assert!(!request_has_messages(
            b"not json",
            CompressibleEndpoint::AnthropicMessages
        ));
    }

    #[test]
    fn ccr_workspace_project_id_wins() {
        let mut headers = HeaderMap::new();
        headers.insert("x-headroom-project-id", "my-project".parse().unwrap());
        let body = serde_json::json!({});

        let (key, label) = resolve_ccr_workspace(Some(&headers), &body).unwrap();
        assert_eq!(key, "my-project");
        assert_eq!(label.as_deref(), Some("my-project"));
    }

    #[test]
    fn ccr_workspace_empty_when_unresolved() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(resolve_ccr_workspace(None, &body).is_none());
    }

    #[test]
    fn ccr_workspace_system_prompt_cwd_fallback() {
        let body = serde_json::json!({
            "system": "You are helpful.\ncwd: /home/user/code/my-project\n",
            "messages": []
        });

        let (key, label) = resolve_ccr_workspace(None, &body).unwrap();
        assert!(key.starts_with("my-project-"));
        assert_eq!(label.as_deref(), Some("my-project"));
    }

    #[test]
    fn latest_user_query_reads_latest_text_block() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": [
                    {"type": "image", "source": {}},
                    {"type": "text", "text": "new query"}
                ]}
            ]
        });

        assert_eq!(latest_user_query(&body), "new query");
    }

    #[test]
    fn append_context_adds_text_block_to_latest_user_only() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "old"}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": [{"type": "text", "text": "new"}]}
            ]
        });

        assert!(append_context_to_latest_user_turn(
            &mut body,
            "expanded".to_string()
        ));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 1);
        let latest_blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(latest_blocks.len(), 2);
        assert_eq!(latest_blocks[1]["text"], "expanded");
    }

    #[test]
    fn ccr_context_tracker_filters_cross_workspace() {
        let mut tracker = headroom_core::ccr::context_tracker::ContextTracker::new(Some(
            headroom_core::ccr::context_tracker::ContextTrackerConfig {
                relevance_threshold: 0.1,
                ..Default::default()
            },
        ));
        tracker.track_compression(
            "abc123",
            1,
            Some("Bash"),
            100,
            1,
            "workspace-a",
            "find auth middleware",
            "auth_middleware.py login handler",
        );

        assert!(tracker
            .analyze_query("auth middleware", Some(2), "workspace-b")
            .is_empty());
        let recs = tracker.analyze_query("auth middleware", Some(2), "workspace-a");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].hash_key, "abc123");
    }

    // ── is_application_json ──────────────────────────────────────────

    #[test]
    fn is_application_json_plain() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        assert!(is_application_json(&h));
    }

    #[test]
    fn is_application_json_with_charset() {
        let mut h = HeaderMap::new();
        h.insert(
            "content-type",
            "application/json; charset=utf-8".parse().unwrap(),
        );
        assert!(is_application_json(&h));
    }

    #[test]
    fn is_application_json_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "Application/JSON".parse().unwrap());
        assert!(is_application_json(&h));
    }

    #[test]
    fn is_application_json_missing_header() {
        let h = HeaderMap::new();
        assert!(!is_application_json(&h));
    }

    #[test]
    fn is_application_json_wrong_type() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "text/plain".parse().unwrap());
        assert!(!is_application_json(&h));
    }

    // ── is_websocket_upgrade ─────────────────────────────────────────

    #[test]
    fn is_websocket_upgrade_both_headers() {
        let mut h = HeaderMap::new();
        h.insert("upgrade", "websocket".parse().unwrap());
        h.insert("connection", "Upgrade".parse().unwrap());
        assert!(is_websocket_upgrade(&h));
    }

    #[test]
    fn is_websocket_upgrade_missing_upgrade_header() {
        let mut h = HeaderMap::new();
        h.insert("connection", "Upgrade".parse().unwrap());
        assert!(!is_websocket_upgrade(&h));
    }

    #[test]
    fn is_websocket_upgrade_missing_connection_header() {
        let mut h = HeaderMap::new();
        h.insert("upgrade", "websocket".parse().unwrap());
        assert!(!is_websocket_upgrade(&h));
    }

    #[test]
    fn is_websocket_upgrade_connection_with_other_tokens() {
        let mut h = HeaderMap::new();
        h.insert("upgrade", "websocket".parse().unwrap());
        h.insert("connection", "keep-alive, Upgrade".parse().unwrap());
        assert!(is_websocket_upgrade(&h));
    }

    // ── is_sse_response ──────────────────────────────────────────────

    #[test]
    fn is_sse_response_plain() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "text/event-stream".parse().unwrap());
        assert!(is_sse_response(&h));
    }

    #[test]
    fn is_sse_response_with_charset() {
        let mut h = HeaderMap::new();
        h.insert(
            "content-type",
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        assert!(is_sse_response(&h));
    }

    #[test]
    fn is_sse_response_missing() {
        let h = HeaderMap::new();
        assert!(!is_sse_response(&h));
    }

    #[test]
    fn is_sse_response_wrong_type() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        assert!(!is_sse_response(&h));
    }

    // ── append_anthropic_beta ────────────────────────────────────────

    #[test]
    fn append_anthropic_beta_to_empty() {
        let mut h = HeaderMap::new();
        append_anthropic_beta(&mut h, "prompt-caching-2024-07-31");
        assert_eq!(
            h.get("anthropic-beta").unwrap().to_str().unwrap(),
            "prompt-caching-2024-07-31"
        );
    }

    #[test]
    fn append_anthropic_beta_deduplicates() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-beta",
            "prompt-caching-2024-07-31".parse().unwrap(),
        );
        append_anthropic_beta(&mut h, "prompt-caching-2024-07-31");
        assert_eq!(
            h.get("anthropic-beta").unwrap().to_str().unwrap(),
            "prompt-caching-2024-07-31"
        );
    }

    #[test]
    fn append_anthropic_beta_merges() {
        let mut h = HeaderMap::new();
        h.insert("anthropic-beta", "existing-beta".parse().unwrap());
        append_anthropic_beta(&mut h, "new-beta");
        assert_eq!(
            h.get("anthropic-beta").unwrap().to_str().unwrap(),
            "existing-beta,new-beta"
        );
    }
}

#[cfg(test)]
mod inbound_metrics_tests {
    use super::*;
    use crate::observability::proxy_counters;
    use axum::routing::get;
    use tower::ServiceExt;

    /// The inbound counters are process-global, so these tests would race each
    /// other's increments if they ran concurrently.
    fn inbound_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
        let _guard = inbound_test_lock();
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
        let _guard = inbound_test_lock();
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
        let _guard = inbound_test_lock();
        let before = proxy_counters::inbound_total_for_test();

        run_once("/nope", Router::new().route("/ok", get(|| async { "hi" }))).await;

        assert_eq!(proxy_counters::inbound_total_for_test(), before + 1);
    }
}

/// Message array for an inbound request body, across the shapes the proxy sees.
///
/// Anthropic and OpenAI Chat both use `messages`; the OpenAI Responses API uses
/// `input`. Returns `None` when the body carries neither, which is the signal to
/// skip waste measurement rather than measure an empty conversation.
fn request_message_array(parsed_body: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    parsed_body
        .get("messages")
        .or_else(|| parsed_body.get("input"))
        .and_then(|v| v.as_array())
}

/// Measure waste signals for one request body.
///
/// Returns `None` when there is nothing to measure — no message array, or a
/// parse that produced no signals. `None` means "not measured"; an empty vec
/// would claim "measured, found nothing".
fn waste_signals_for_request(
    parsed_body: &serde_json::Value,
    model: &str,
) -> Option<Vec<(String, i64)>> {
    let messages = request_message_array(parsed_body)?;
    let tokenizer = headroom_core::tokenizer::get_tokenizer(model);
    let (_blocks, _breakdown, waste) =
        headroom_core::parser::parse_messages(messages, tokenizer.as_ref(), None);
    let signals = waste.non_zero();
    if signals.is_empty() {
        None
    } else {
        Some(signals)
    }
}

/// Emit one request's waste signals to Prometheus.
///
/// Only signals that fired are emitted — a zero for every signal on every
/// request would inflate the label space without adding information.
fn record_waste_signals(signals: &[(String, i64)]) {
    for (signal, tokens) in signals {
        if *tokens > 0 {
            crate::observability::proxy_counters::record_waste_signal_tokens(
                signal,
                *tokens as u64,
            );
        }
    }
}

#[cfg(test)]
mod waste_signal_wiring_tests {
    use super::*;
    use serde_json::json;

    /// Prometheus state is process-global; these read counter deltas.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Anthropic and OpenAI Chat carry `messages`; OpenAI Responses uses
    /// `input`. Both have to be found, or waste goes unmeasured on that route.
    #[test]
    fn both_request_shapes_expose_their_messages() {
        let chat = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(request_message_array(&chat).map(|m| m.len()), Some(1));

        let responses = json!({"input": [{"role": "user", "content": "hi"}]});
        assert_eq!(request_message_array(&responses).map(|m| m.len()), Some(1));

        assert!(request_message_array(&json!({"model": "x"})).is_none());
    }

    /// A body with no message array is "not measured", not "measured zero".
    #[test]
    fn a_body_without_messages_is_not_measured() {
        assert!(
            waste_signals_for_request(&json!({"model": "x"}), "claude-3-5-sonnet-20241022")
                .is_none()
        );
    }

    /// Clean prose has no waste, and that must read as `None` rather than a
    /// vector of zeros — otherwise every request would emit every signal.
    #[test]
    fn a_clean_request_reports_no_signals() {
        let body = json!({"messages": [{"role": "user", "content": "hello there friend"}]});
        assert!(waste_signals_for_request(&body, "claude-3-5-sonnet-20241022").is_none());
    }

    /// A base64 blob is waste, and only the signals that fired are returned.
    #[test]
    fn a_blob_is_detected_and_only_fired_signals_are_returned() {
        let body = json!({
            "messages": [{"role": "user", "content": format!("data: {}==", "A".repeat(400))}]
        });
        let signals = waste_signals_for_request(&body, "claude-3-5-sonnet-20241022")
            .expect("a base64 blob should register as waste");

        assert!(signals
            .iter()
            .any(|(name, tokens)| name == "base64" && *tokens > 0));
        assert!(
            signals.iter().all(|(_, tokens)| *tokens > 0),
            "only fired signals should be present, got {signals:?}"
        );
    }

    #[test]
    fn recorded_signals_reach_the_counter() {
        let _g = lock();
        let before = crate::observability::proxy_counters::waste_signal_tokens_for_test("base64");

        record_waste_signals(&[("base64".to_string(), 25)]);

        assert_eq!(
            crate::observability::proxy_counters::waste_signal_tokens_for_test("base64"),
            before + 25
        );
    }

    /// A zero or negative count must not be emitted at all.
    #[test]
    fn non_positive_counts_are_not_recorded() {
        let _g = lock();
        let before =
            crate::observability::proxy_counters::waste_signal_tokens_for_test("html_noise");

        record_waste_signals(&[
            ("html_noise".to_string(), 0),
            ("html_noise".to_string(), -5),
        ]);

        assert_eq!(
            crate::observability::proxy_counters::waste_signal_tokens_for_test("html_noise"),
            before
        );
    }
}

#[cfg(test)]
mod timing_field_tests {
    use super::*;
    use crate::observability::proxy_counters;
    use headroom_core::request_outcome::RequestOutcome;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Overhead and TTFB only update their bounds when positive. A zero means
    /// "not measured" — treating it as a sample would pin the minimum at 0
    /// forever and report a floor that never happened.
    #[test]
    fn a_zero_timing_never_lowers_the_minimum() {
        let _g = lock();
        proxy_counters::record_request("t-probe", "m", 1, 1, 0, 40.0, false, 25.0, 60.0);
        let after_real = (
            proxy_counters::overhead_min_for_test(),
            proxy_counters::ttfb_min_for_test(),
        );
        assert!(
            after_real.0 > 0.0,
            "a real overhead sample must set the min"
        );
        assert!(after_real.1 > 0.0, "a real ttfb sample must set the min");

        // A follow-up request that measured neither must not drag them to 0.
        proxy_counters::record_request("t-probe", "m", 1, 1, 0, 40.0, false, 0.0, 0.0);
        assert_eq!(proxy_counters::overhead_min_for_test(), after_real.0);
        assert_eq!(proxy_counters::ttfb_min_for_test(), after_real.1);
    }

    /// The sink is the single funnel every handler's outcome passes through, so
    /// the timing fields have to survive the trip into it.
    #[test]
    fn the_outcome_carries_the_timing_fields() {
        let outcome = RequestOutcome {
            provider: "anthropic".to_string(),
            model: "m".to_string(),
            overhead_ms: 12.5,
            ttfb_ms: 340.0,
            total_latency_ms: 900.0,
            ..Default::default()
        };
        assert_eq!(outcome.overhead_ms, 12.5);
        assert_eq!(outcome.ttfb_ms, 340.0);
        // Overhead is headroom's own cost and must not exceed the wall clock.
        assert!(outcome.overhead_ms <= outcome.total_latency_ms);
    }
}
