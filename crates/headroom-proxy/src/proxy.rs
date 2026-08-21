//! Core reverse-proxy router and HTTP forwarding handler.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use url::Url;

use axum::body::{to_bytes, Body};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, State, WebSocketUpgrade};
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
use crate::cache_stabilization::beta_sticky::BetaProvider;
use crate::cache_stabilization::drift_detector::{
    compute_structural_hash, derive_session_key, observe_drift, ApiKind, DriftState,
};
use crate::cache_stabilization::prefix_replay::{SessionReplayStore, REPLAY_STORE_CAPACITY};
use crate::compression;
use crate::config::Config;
use crate::error::ProxyError;
use crate::headers::{build_forward_request_headers, filter_response_headers};
use crate::health::{healthz, healthz_upstream, rollout_status};
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
    /// Session-sticky beta-header tracker (parity port of the Python
    /// `SessionBetaTracker`, PR-A6): per-`(provider, session)` LRU of
    /// `anthropic-beta` / `openai-beta` tokens, unioned across turns
    /// so a client dropping a token mid-conversation doesn't rotate
    /// the upstream prefix-cache key. Shares the drift detector's
    /// session identity (same `derive_session_key` output).
    pub beta_sticky: cache_stabilization::beta_sticky::BetaStickyState,
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
    /// Per-conversation working-directory pins. Read and written only when
    /// `Config::hold_working_directory` is on; see
    /// [`cache_stabilization::working_dir`] for why the pin outlives the replay
    /// store's session TTL.
    pub working_dir_pins: cache_stabilization::working_dir::WorkingDirPins,
    /// When this process started. The replay store is in-memory, so every
    /// restart empties it and the first turn of every live conversation then
    /// finds no prefix. Without this, that expected gap is indistinguishable
    /// from a session key that is not stable — a distinction five false alarms
    /// were spent on before it was recorded here.
    pub started_at: std::time::Instant,
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

/// Extra attempts for a CCR continuation before giving up on the retrieval.
/// The content is already fetched by this point, so the only thing a failure
/// costs is the model's answer; three attempts covers the overload bursts that
/// produced every observed continuation failure.
const CCR_CONTINUATION_RETRIES: u32 = 2;

impl AppState {
    /// The CCR store, when offload is configured and `--ccr-inject-marker` is on.
    ///
    /// Compression emits a `<<ccr:HASH>>` marker only when handed one of
    /// these, so passing `None` makes compression one-way. Pass it only on
    /// paths that also inject `headroom_retrieve` — a marker the model cannot
    /// act on is worse than no marker, since it spends tokens advertising a
    /// recovery route that does not exist.
    ///
    /// `--ccr-inject-marker=false` withholds the store here rather than at the
    /// injection site, so marker text and store writes stop together.
    /// Suppressing only the text would offload blocks the model has no handle
    /// to ask back — the same dangling pointer from the other end. Python
    /// pairs them the same way: every `ccr_inject_marker=False` call site also
    /// passes `ccr_enabled=False`.
    pub(crate) fn ccr_store(&self) -> Option<std::sync::Arc<dyn headroom_core::ccr::CcrStore>> {
        if !self.config.ccr_inject_marker {
            return None;
        }
        self.ctx_offload.as_ref().map(|r| r.store.ccr())
    }
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

        // CTX-2b: one registry of per-project stores, shared by capture,
        // offload and recall so all three read and write the same file for a
        // given project. Nothing is opened here — the project is a property of
        // a request, not of the process, so handles are opened on first sight.
        let ctx_base = if config.ctx_capture || config.ctx_offload || config.ctx_inject {
            let base = config
                .ctx_store_dir
                .clone()
                .or_else(headroom_core::ctx::default_base_dir);
            if base.is_none() {
                tracing::warn!(
                    event = "ctx_no_store_dir",
                    "ctx features enabled but no store dir and $HOME unset; disabled"
                );
            }
            base
        } else {
            None
        };
        let ctx_stores = ctx_base
            .clone()
            .map(|dir| Arc::new(crate::ctx::projects::ProjectStores::new(dir)));

        // CTX-2: construct the passive-capture observer only when enabled.
        // A failure to spawn the worker is logged loudly and disables capture
        // (a broken observer must never take down the proxy) — the request path
        // is unaffected either way.
        let ctx_observer = match (config.ctx_capture, ctx_stores.clone()) {
            (true, Some(stores)) => match crate::ctx::observer::CtxObserver::start(stores) {
                Ok(obs) => Some(Arc::new(obs)),
                Err(e) => {
                    tracing::warn!(
                        event = "ctx_observer_start_failed",
                        error = %e,
                        "CTX-2 capture disabled: could not start the capture worker"
                    );
                    None
                }
            },
            _ => None,
        };

        // CTX-3: construct the offload runtime only when enabled. Independent
        // of `ctx_capture` — offload is its own flag. A failure to open the CCR
        // store is logged loudly and disables offload (a broken sink must never
        // take down the proxy).
        let ctx_offload = if config.ctx_offload {
            match (ctx_base.clone(), ctx_stores.clone()) {
                (Some(dir), Some(stores)) => {
                    match crate::ctx::offload_store::OffloadStore::start(
                        &dir,
                        config.ctx_offload_ttl_seconds,
                        stores,
                    ) {
                        Ok(store) => Some(CtxOffloadRuntime {
                            config: crate::compression::ctx_offload::CtxOffloadConfig {
                                min_bytes: config.ctx_offload_min_bytes,
                                stale_margin: config.ctx_offload_stale_messages,
                                stale_window: config.ctx_offload_stale_window,
                            },
                            store: Arc::new(store),
                            // Under the offload store's own directory, so it needs
                            // no flag of its own and lives beside the originals it
                            // refers to. Forgetting a conversion does not merely
                            // defer it — the block forwards raw where the provider
                            // cached a digest. See `OffloadGate`.
                            gate: Arc::new(
                                crate::compression::ctx_offload::OffloadGate::with_persistence(
                                    DRIFT_DETECTOR_CAPACITY,
                                    dir.join("offload-gate"),
                                ),
                            ),
                        }),
                        Err(e) => {
                            tracing::warn!(
                                event = "ctx_offload_start_failed",
                                error = %e,
                                "CTX-3 offload disabled: could not open the CCR store"
                            );
                            None
                        }
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        // CTX-4: recall/resume injection. Requires ctx_capture (the identity +
        // sessions layer) — enforce loudly, no silent dependency. Shares the
        // store registry with capture and offload, so it recalls from the same
        // per-project files those two write.
        let ctx_inject = if config.ctx_inject {
            if !config.ctx_capture {
                return Err(ProxyError::Config(
                    "--ctx-inject requires --ctx-capture (the sessions/identity layer); \
                     enable ctx_capture or disable ctx_inject"
                        .to_string(),
                ));
            }
            match (ctx_observer.as_ref(), ctx_stores.clone()) {
                (Some(_), Some(stores)) => {
                    Some(Arc::new(crate::ctx::inject::InjectEngine::new(stores)))
                }
                _ => {
                    // ctx_capture was on but capture failed to start; without
                    // the sessions layer injection cannot run. Log and disable.
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
            // Prefer the FTS5-backed store: BM25 with stemming, and memories
            // that survive a restart. The in-memory backend it replaces scored
            // by counting overlapping words and lost everything on exit; it
            // stays as the fallback for the case where no store dir resolves,
            // because a degraded memory beats none.
            let memory_dir = config
                .ctx_store_dir
                .clone()
                .or_else(headroom_core::ctx::default_base_dir)
                .map(|base| base.join("memory"));
            match memory_dir
                .as_deref()
                .map(crate::memory::ctx_backend::CtxMemoryBackend::open)
            {
                Some(Ok(backend)) => {
                    handler.set_backend(Arc::new(backend));
                    tracing::info!(
                        event = "memory_backend_started",
                        backend = "ctx_fts",
                        dir = ?memory_dir,
                        "memory backend initialized (FTS5, persistent)"
                    );
                }
                other => {
                    if let Some(Err(e)) = other {
                        tracing::warn!(
                            event = "memory_backend_fallback",
                            error = %e,
                            "could not open the FTS memory store; using the in-memory backend"
                        );
                    } else {
                        tracing::warn!(
                            event = "memory_backend_fallback",
                            "no memory store dir and $HOME unset; using the in-memory backend"
                        );
                    }
                    handler.set_backend(Arc::new(
                        crate::memory::local_backend::LocalMemoryBackend::new(),
                    ));
                }
            }
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

        let replay_store = if config.replay_store_dir.is_empty() {
            SessionReplayStore::new(REPLAY_STORE_CAPACITY)
        } else {
            SessionReplayStore::with_persistence(
                REPLAY_STORE_CAPACITY,
                std::path::PathBuf::from(&config.replay_store_dir),
            )
        };

        // Read before `config` moves into the Arc below.
        let observed_cache_ttl = if config.force_1h_cache_ttl || config.split_cache_ttl {
            cache_stabilization::usage_observer::ANTHROPIC_CACHE_TTL_1H
        } else {
            cache_stabilization::usage_observer::ANTHROPIC_CACHE_TTL
        };

        Ok(Self {
            config: Arc::new(config),
            client,
            bedrock_credentials: None,
            drift_state: DriftState::new(DRIFT_DETECTOR_CAPACITY),
            tool_order_state: cache_stabilization::tool_order::ToolOrderStore::default(),
            replay_store,
            working_dir_pins: cache_stabilization::working_dir::WorkingDirPins::new(
                REPLAY_STORE_CAPACITY,
            ),
            started_at: std::time::Instant::now(),
            beta_sticky: cache_stabilization::beta_sticky::BetaStickyState::new(
                cache_stabilization::beta_sticky::BETA_TRACKER_CAPACITY,
            ),
            vertex_token_source,
            // Tell the classifier which TTL we actually pin. Left at the
            // 5-minute default it files every bust in a 5m..1h gap as a
            // legitimate expiry, which hid ~3% of daily creation.
            usage_observer: Arc::new(
                cache_stabilization::usage_observer::UsageObserver::new()
                    .with_cache_ttl(observed_cache_ttl),
            ),
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

/// Tokens the upstream actually processed for billing, when its usage block
/// supplied the cache breakdown. Anthropic reports uncached input, cache reads,
/// and cache creation as disjoint values, so their sum is the post-transform
/// request the proxy sent — not the pre-compression baseline used to measure
/// savings.
///
/// A provider that returns no input usage leaves us without a billable source
/// of truth. In that exceptional case use the post-transform compression
/// estimate rather than the original pre-transform size.
fn provider_billed_input_tokens(outcome: &headroom_core::request_outcome::RequestOutcome) -> i64 {
    let billed = outcome
        .uncached_input_tokens
        .max(0)
        .saturating_add(outcome.cache_read_tokens.max(0))
        .saturating_add(outcome.cache_write_tokens.max(0));
    if billed > 0 {
        billed
    } else {
        outcome.optimized_tokens.max(0)
    }
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
        let billed_input_tokens = provider_billed_input_tokens(outcome);
        let rec = headroom_core::savings_tracker::RequestRecord {
            model: &outcome.model,
            // `original_tokens` is a savings baseline. Cost and usage must
            // instead follow the request Anthropic received after every proxy
            // transform, as reported in the response usage breakdown.
            input_tokens: billed_input_tokens,
            tokens_saved: outcome.tokens_saved,
            compression_savings_cost_usd: Some(outcome.compression_savings_cost_usd()),
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
            // Durable lifetime metrics only. The outcome has carried these all
            // along; forwarding them is what lets the persisted blob say
            // whether the cache is working, not just whether we compressed.
            output_tokens: outcome.output_tokens,
            attempted_input_tokens: outcome.attempted_input_tokens,
            cache_write_5m_tokens: outcome.cache_write_5m_tokens,
            cache_write_1h_tokens: outcome.cache_write_1h_tokens,
            cached: outcome.cache_hit(),
            stack: outcome.client.as_deref(),
            waste_signals: outcome.waste_signals.clone(),
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
        crate::observability::proxy_counters::record_request(
            &outcome.provider,
            &outcome.model,
            billed_input_tokens as u64,
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
            tokens_sent: provider_billed_input_tokens(outcome),
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

    fn record_failed(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        crate::observability::proxy_counters::record_failed();
        self.savings_tracker.record_failed_work(
            &headroom_core::savings_tracker::FailedWorkRecord {
                status_code: outcome.status_code,
                upstream_attempts: outcome.upstream_attempts,
                forwarded_tokens: outcome.optimized_tokens,
                provider_input_tokens: outcome.provider_input_tokens,
                provider_output_tokens: outcome.provider_output_tokens,
                timestamp: None,
            },
        );
    }

    fn record_cache_outcome(&self, provider: &str, reason: &str, wasted_tokens: i64) {
        self.savings_tracker
            .record_cache_miss(Some(provider), Some(reason));
        if wasted_tokens > 0 {
            self.savings_tracker.record_cache_bust(wasted_tokens);
        }
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
        let priced_cost = outcome.compression_savings_cost_usd();
        let priced_basis = outcome.compression_savings_cost_basis().to_string();
        let pricing = headroom_core::pricing::lookup(&model);
        let fresh_rate = pricing
            .map(|p| p.input_cost_per_token)
            .unwrap_or(headroom_core::savings_ledger::DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN);
        let cache_read_rate = pricing
            .and_then(|p| p.cache_read_cost_per_token)
            .unwrap_or(fresh_rate);
        let fresh_counterfactual = saved.max(0) as f64 * fresh_rate;
        let cache_counterfactual = saved.max(0) as f64 * cache_read_rate;
        tracing::info!(
            event = "savings_pricing_counterfactual",
            request_id = %outcome.request_id,
            model = %model,
            tokens_saved = saved,
            cache_read_tokens = outcome.cache_read_tokens,
            cache_write_tokens = outcome.cache_write_tokens,
            fresh_input_rate = fresh_rate,
            cache_read_rate,
            fresh_input_usd = fresh_counterfactual,
            cache_read_usd = cache_counterfactual,
            priced_cost_basis = %priced_basis,
            priced_cost_usd = priced_cost,
            "savings ledger pricing counterfactuals"
        );
        tokio::task::spawn_blocking(move || {
            headroom_core::savings_ledger::record_from_forwarded_with_cost(
                forwarded,
                saved,
                Some(&model),
                client.as_deref(),
                Some(priced_cost),
                Some(&priced_basis),
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
        .route("/rollout/status", get(rollout_status))
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
        // in axum's `{param}` syntax, so we capture the entire trailing
        // segment as `{model_action}` and split on the last `:` inside
        // the dispatcher. Both verbs share the same axum route shape
        // — matchit can't distinguish two patterns that overlap on the
        // literal parameter. The verb dispatch lives in
        // [`crate::vertex::handle_vertex_predict_dispatch`].
        .route(
            "/v1beta1/projects/{project}/locations/{location}/publishers/anthropic/models/{model_action}",
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
                "/model/{model_id}/invoke",
                post(crate::bedrock::invoke::handle_invoke),
            )
            .route(
                "/model/{model_id}/converse",
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
                "/model/{model_id}/invoke-with-response-stream",
                post(crate::bedrock::invoke_streaming::handle_invoke_streaming),
            )
            .route(
                "/model/{model_id}/converse-stream",
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
                "/v1/conversations/{conversation_id}",
                get(crate::handlers::conversations::handle_conversations_get)
                    .post(crate::handlers::conversations::handle_conversations_update)
                    .delete(crate::handlers::conversations::handle_conversations_delete),
            )
            .route(
                "/v1/conversations/{conversation_id}/items",
                post(crate::handlers::conversations::handle_conversations_items_create)
                    .get(crate::handlers::conversations::handle_conversations_items_list),
            )
            .route(
                "/v1/conversations/{conversation_id}/items/{item_id}",
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
                "/v1/batches/{batch_id}",
                get(crate::handlers::batch::openai_batch_get),
            )
            .route(
                "/v1/batches/{batch_id}/cancel",
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
                "/v1/messages/batches/{batch_id}",
                get(crate::handlers::batch_anthropic::anthropic_batch_get),
            )
            .route(
                "/v1/messages/batches/{batch_id}/cancel",
                post(crate::handlers::batch_anthropic::anthropic_batch_cancel),
            )
            .route(
                "/v1/messages/batches/{batch_id}/results",
                get(crate::handlers::batch_anthropic::anthropic_batch_results),
            );
    }

    // Gemini native API routes. These handle the Gemini-specific format
    // (contents[] with parts[], systemInstruction) and apply compression
    // via the OpenAI pipeline after format conversion.
    router = router.route(
        "/v1beta/models/{*model_action}",
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

    // WEB-02: drop a caller-supplied memory identity unless the caller is on
    // loopback. Applied before the counter so it wraps every route, and done
    // here rather than at each reader so a new reader cannot miss it.
    router = router.layer(axum::middleware::from_fn(strip_untrusted_identity));

    // Count every inbound request, including ones that fall through to the
    // catch-all. Applied last so it wraps the whole router.
    router = router.layer(axum::middleware::from_fn(track_inbound_request));

    router.fallback(any(catch_all)).with_state(state)
}

/// Whether a caller at `client_ip` may choose its own memory partition.
///
/// `None` means the peer address is unknown, which is not evidence of loopback,
/// so it fails closed — unlike [`crate::loopback_guard::is_loopback_host`],
/// which treats `None` as local for the benefit of test clients.
fn identity_header_is_trusted(client_ip: Option<&str>) -> bool {
    client_ip.is_some_and(|ip| crate::loopback_guard::is_loopback_host(Some(ip)))
}

/// Remove `x-headroom-user-id` unless the caller is on loopback.
///
/// The header picks the memory partition, and a caller cannot prove its own
/// authority to select one, so honoring it from a remote caller lets anyone
/// read or write another user's memories. Mirrors Python's
/// `resolve_memory_identity`, which trusts the header only from loopback.
/// Remote callers fall back to the reader's own default partition.
///
/// Missing peer metadata is not evidence of loopback, so it fails closed —
/// unlike [`crate::loopback_guard::is_loopback_host`], which treats `None` as
/// local for the benefit of test clients.
async fn strip_untrusted_identity(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    const USER_ID_HEADER: &str = "x-headroom-user-id";
    if req.headers().contains_key(USER_ID_HEADER) {
        let client_ip = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip().to_string());
        if !identity_header_is_trusted(client_ip.as_deref()) {
            req.headers_mut().remove(USER_ID_HEADER);
            tracing::warn!(
                event = "identity_header_stripped",
                header = USER_ID_HEADER,
                client_ip = client_ip.as_deref().unwrap_or("unknown"),
                "ignoring x-headroom-user-id from a non-loopback caller"
            );
        }
    }
    next.run(req).await
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
    req: Request<Body>,
) -> Response<Body> {
    let (mut parts, body) = req.into_parts();
    if is_websocket_upgrade(&parts.headers) {
        // axum 0.8 requires optional extractors to opt in explicitly, and
        // WebSocketUpgrade intentionally does not. Extract it only after the
        // upgrade headers have identified this as a WebSocket request.
        if let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            let req = Request::from_parts(parts, body);
            return ws_handler(ws, state, client_addr, req).await;
        }
        // Header says websocket but axum didn't extract it (likely missing
        // Sec-WebSocket-Key) — fall through to HTTP forwarding which will
        // surface the upstream error.
    }
    let req = Request::from_parts(parts, body);
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// True if `Content-Type` is `application/json` (with any optional
/// parameters like `; charset=utf-8`). Compression only inspects JSON
/// bodies — multipart uploads, form-encoded posts, and binary
/// payloads stream through untouched.
/// CTX-7: serve the re-cache watchdog snapshot as JSON.
///
/// The upstream-rejection summary rides along under `upstream`. A refused turn
/// costs more than any cache miss, so it belongs on the endpoint the statusline
/// already polls rather than on one nobody watches.
async fn cache_health(State(state): State<AppState>) -> impl IntoResponse {
    let mut snapshot = serde_json::to_value(state.usage_observer.snapshot())
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert(
            "upstream".to_string(),
            crate::observability::upstream_health::snapshot(),
        );
    }
    axum::Json(snapshot)
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
pub(crate) fn resolve_ccr_workspace(
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

/// Resolve the project directory used to pick this request's ctx stores.
///
/// Same tier order as [`resolve_ccr_workspace`], but returns the canonical
/// directory rather than a display key, because that is what
/// `hash_project_dir_canonical` names the DB files after.
///
/// Falls back to [`crate::ctx::projects::UNRESOLVED_PROJECT`] instead of
/// failing closed: capture and recall have to go *somewhere*, and the shared
/// bucket is where every request already landed before sharding existed.
pub(crate) fn resolve_ctx_project(headers: Option<&HeaderMap>, body: &serde_json::Value) -> String {
    let ctx = crate::memory::router::RequestContext {
        headers: header_map_to_lowercase_strings(headers),
        system_prompt: crate::memory::router::extract_system_prompt(body),
        base_user_id: String::new(),
        project_root_override: None,
    };
    crate::memory::router::ProjectResolver::resolve_project_dir(&ctx)
        .unwrap_or_else(|| crate::ctx::projects::UNRESOLVED_PROJECT.to_string())
}

pub(crate) fn latest_user_query(body: &serde_json::Value) -> String {
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

pub(crate) fn anthropic_turn_number(body: &serde_json::Value) -> u32 {
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

// Eight parameters, one over the lint's threshold. Grouping them into a struct
// would mean a type used at exactly two call sites, both of which pass every
// field, so the indirection would cost more reading than it saves.
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_append_ccr_proactive_expansion(
    state: &AppState,
    body: &mut serde_json::Value,
    user_query: &str,
    workspace_key: &str,
    workspace_label: Option<&str>,
    turn_number: u32,
    request_id: &str,
    budget: &crate::injection_budget::InjectionBudget,
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
    // Charge the shared budget. Expansion appends to the live tail, which is
    // re-sent every turn, so clipping it here is cache-safe.
    let Some(expansion_text) = budget.take(
        crate::injection_budget::InjectionStage::ProactiveExpansion,
        expansion_text,
    ) else {
        return false;
    };
    // Measure before the move: this is what the request grows by, and it is
    // the only number that says whether expansion is worth what offload saved.
    let expansion_bytes = expansion_text.len() as u64;
    let changed = append_context_to_latest_user_turn(body, expansion_text);
    if changed {
        crate::observability::ctx_metrics::observe_proactive_expansion(expansion_bytes);
        tracing::info!(
            request_id = %request_id,
            expansions = expansions.len(),
            expansion_bytes = expansion_bytes,
            "CCR Phase 4: proactively expanded relevant offloaded context"
        );
    }
    changed
}

pub(crate) fn track_ccr_context_records(
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
        config.context_edit_min_messages,
        config.context_edit_trigger_tokens,
        config.context_edit_clear_at_least,
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
                min_messages = config.context_edit_min_messages,
                trigger_tokens = config.context_edit_trigger_tokens,
                clear_at_least = ?config.context_edit_clear_at_least,
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
/// Resize oversized images down to Anthropic's own limits before forwarding.
///
/// Anthropic bills images by **dimensions, not bytes** — `(w * h) / 750`, capped
/// at 1568px on the long edge and 1.15MP — so re-encoding alone saves nothing
/// and only a resize moves the number. Measured over 800 live bodies: images are
/// 9.2% of the prompt at 16,228 tok/body, and 13 of 13 distinct images exceeded
/// 1.15MP at a mean 2,877 tokens each. They sit just under the 1568px edge cap,
/// so the provider does not shrink them for us.
///
/// The transform is a pure function of the source bytes and memoised on their
/// hash, so a given image forwards identically on every turn and the cached
/// prefix holds. Enabling it re-keys live conversations once, like any change to
/// content already inside a cached prefix.
pub(crate) fn maybe_optimize_images(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(messages) = value.get("messages").and_then(|m| m.as_array()) else {
        return body;
    };
    let (optimized, results) =
        crate::tile_optimizer::optimize_images_in_messages_cached(messages, "anthropic");
    if results.is_empty() {
        return body;
    }
    let saved: u32 = results
        .iter()
        .map(super::tile_optimizer::TileOptResult::tokens_saved)
        .sum();
    if saved == 0 {
        return body;
    }
    value["messages"] = serde_json::Value::Array(optimized);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "image_optimize",
                request_id = %request_id,
                images = results.len(),
                tokens_before = results.iter().map(|r| r.tokens_before).sum::<u32>(),
                tokens_saved = saved,
                "resized oversized images"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

pub(crate) fn maybe_prune_tools(
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
pub(crate) fn maybe_stabilize_tool_order(
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
/// Put the message breakpoint on the last content block. See
/// [`cache_stabilization::message_breakpoints`] for the measurement.
fn maybe_push_tail_breakpoint(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    if !cache_stabilization::message_breakpoints::push_marker_to_tail(&mut value) {
        crate::observability::tail_breakpoint::observe(false);
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            crate::observability::tail_breakpoint::observe(true);
            tracing::debug!(
                request_id = %request_id,
                event = "cache_tail_breakpoint",
                "moved the message breakpoint to the tail"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

fn maybe_pin_cache_ttl(body: bytes::Bytes, request_id: &str, split: bool) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let changed = if split {
        cache_stabilization::cache_ttl::tail_5m_prefix_1h(&mut value)
    } else {
        cache_stabilization::cache_ttl::force_1h_ttl(&mut value)
    };
    if !changed {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                event = if split { "split_cache_ttl" } else { "force_1h_cache_ttl" },
                "pinned cache_control ttl"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Hold this conversation's working-directory line still, restating the live
/// directory at the message tail.
///
/// Byte-equal passthrough — the same cache-safety invariant the other body
/// rewrites keep — when the body is not JSON, `system` names no working
/// directory, this is the conversation's first sight, or the live directory
/// already matches the pin. See [`cache_stabilization::working_dir`].
fn hold_working_directory(
    body: bytes::Bytes,
    pins: &cache_stabilization::working_dir::WorkingDirPins,
    session_key: &str,
    request_id: &str,
) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(live) = pins.hold(&mut value, session_key) else {
        return body;
    };
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            // The path is the operator's own filesystem, and the session key is
            // hashed for the same reason every other event here hashes it.
            tracing::info!(
                event = "working_directory_held",
                request_id = %request_id,
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
                live_directory = %live,
                "held the system preamble's working directory and restated the live one at the tail"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Upstream usage from CCR continuation rounds the client never sees.
///
/// `handle_ccr_response` resolves a `headroom_retrieve` call server-side by
/// re-POSTing to the real upstream, up to `--ccr-max-retrieval-rounds` times,
/// and returns only the last response. Every earlier round is a real billed
/// call whose `usage` block would otherwise be dropped on the floor — the
/// client sees one turn, the bill has several.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CcrRoundUsage {
    /// Continuation rounds whose usage this carries. Zero on the common path.
    pub rounds: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Usage of the first upstream response that the proxy replaced with an
    /// internal continuation. This is the cache footprint of the client's
    /// original request; the next client turn does not contain proxy-private
    /// retrieval/tool-result messages and must be compared with this baseline.
    pub client_input_tokens: u64,
    pub client_cache_read_tokens: u64,
    pub client_cache_write_tokens: u64,
}

impl CcrRoundUsage {
    /// Fold in one response's `usage` block.
    fn add_response(&mut self, response: &serde_json::Value) {
        let Some(usage) = response.get("usage") else {
            return;
        };
        let get = |key: &str| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        };
        if self.rounds == 0 {
            self.client_input_tokens = get("input_tokens").max(0) as u64;
            self.client_cache_read_tokens = get("cache_read_input_tokens").max(0) as u64;
            self.client_cache_write_tokens = get("cache_creation_input_tokens").max(0) as u64;
        }
        self.rounds += 1;
        self.input_tokens += get("input_tokens");
        self.output_tokens += get("output_tokens");
        self.cache_read_tokens += get("cache_read_input_tokens");
        self.cache_write_tokens += get("cache_creation_input_tokens");
    }

    /// Fold another set of rounds in. A turn can spend rounds on more than one
    /// proxy-owned tool family, and both were billed.
    pub fn absorb(&mut self, other: CcrRoundUsage) {
        if self.rounds == 0 && other.rounds > 0 {
            self.client_input_tokens = other.client_input_tokens;
            self.client_cache_read_tokens = other.client_cache_read_tokens;
            self.client_cache_write_tokens = other.client_cache_write_tokens;
        }
        self.rounds += other.rounds;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// True when there is nothing extra to account for.
    pub fn is_empty(&self) -> bool {
        self.rounds == 0
    }

    /// Cache counters that describe the request the client actually made.
    /// Hidden continuation rounds still remain in the billing totals above.
    fn client_cache_baseline(
        &self,
        final_input: u64,
        final_cache_read: u64,
        final_cache_write: u64,
    ) -> (u64, u64, u64) {
        if self.rounds > 0 {
            (
                self.client_input_tokens,
                self.client_cache_read_tokens,
                self.client_cache_write_tokens,
            )
        } else {
            (final_input, final_cache_read, final_cache_write)
        }
    }
}

/// Serialized bytes of `tools` + `system`.
///
/// These are the parts the injection stages write to and the message
/// compressors never touch, so comparing the figure at request entry against
/// the same figure on the wire isolates what the proxy *added* from what
/// compression *removed*. Everything else in the body mixes the two.
fn prefix_head_bytes(value: &serde_json::Value) -> i64 {
    let of = |key: &str| {
        value
            .get(key)
            .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
            .unwrap_or(0) as i64
    };
    of("tools") + of("system")
}

/// Tool definitions (name → serialized bytes) and the calls the model made
/// (name → count), for the durable tool inventory.
///
/// Calls come from `tool_use` blocks in the history the client just resent, so
/// a name accumulates across the turns it survives in that history rather than
/// once per call — the inventory is read as "used / never used", not as an
/// exact call count.
fn tool_inventory_of(value: &serde_json::Value) -> (Vec<(String, i64)>, Vec<(String, i64)>) {
    let mut definitions = Vec::new();
    if let Some(tools) = value.get("tools").and_then(serde_json::Value::as_array) {
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| tool.get("function")?.get("name")?.as_str());
            if let Some(name) = name {
                let bytes = serde_json::to_string(tool).map(|s| s.len()).unwrap_or(0);
                definitions.push((name.to_string(), bytes as i64));
            }
        }
    }

    let mut calls: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    if let Some(messages) = value.get("messages").and_then(serde_json::Value::as_array) {
        for message in messages {
            let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
                    continue;
                }
                if let Some(name) = block.get("name").and_then(serde_json::Value::as_str) {
                    *calls.entry(name.to_string()).or_default() += 1;
                }
            }
        }
    }
    (definitions, calls.into_iter().collect())
}

/// Record what the proxy added to this request and which tools it carried.
///
/// Costs two JSON parses of the request body, which is why it runs once, last,
/// and only on the buffered Anthropic path. Against an upstream call measured
/// in seconds it does not register; on a passthrough request it never runs.
fn record_request_footprint(
    tracker: &headroom_core::savings_tracker::SavingsTracker,
    original: &bytes::Bytes,
    on_the_wire: &bytes::Bytes,
) {
    let Ok(before) = serde_json::from_slice::<serde_json::Value>(original) else {
        return;
    };
    let Ok(after) = serde_json::from_slice::<serde_json::Value>(on_the_wire) else {
        return;
    };
    tracker.record_proxy_overhead(prefix_head_bytes(&before), prefix_head_bytes(&after));
    let (definitions, calls) = tool_inventory_of(&after);
    tracker.record_tools(&definitions, &calls);
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
pub(crate) fn is_retryable_transport_error(e: &reqwest::Error) -> bool {
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
            // WEB-01: the header is client-controlled, so a destination that
            // resolves into private/loopback/link-local/metadata space would
            // make the proxy a confused deputy. Ignore the override and use the
            // configured upstream, as the Python proxy does; set
            // HEADROOM_ALLOWED_BASE_URLS to permit specific internal endpoints.
            Some(u) if crate::upstream_guard::is_safe_upstream_url(&u).await => u,
            Some(u) => {
                tracing::warn!(
                    event = "upstream_override_rejected",
                    header = crate::headers::UPSTREAM_OVERRIDE_HEADER,
                    value = %u,
                    "ignoring unsafe x-headroom-base-url; using default upstream"
                );
                state.effective_upstream().await
            }
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

    // The body as it goes on the wire, kept for the mid-stream retry below.
    // Only the intercepting branch has one; the passthrough branch consumes the
    // client's stream and cannot be re-sent.
    let mut retry_body: Option<bytes::Bytes> = None;
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

        // Pin the billing header in `system[0]` to one string per proxy
        // process. It is cached content, not a header, and Claude Code changes
        // it on every self-update and every new process — which resets a cache
        // we are paying to keep. This has to run here, ahead of the fingerprint
        // below and the prefix-replay capture further down, so every stage sees
        // the pinned form and stores the bytes we will actually forward.
        let buffered = match endpoint {
            compression::CompressibleEndpoint::AnthropicMessages => {
                cache_stabilization::billing_header::pin_billing_header_in_body(buffered)
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
        // Tokens removed by transforms that run *outside* the compression
        // pipeline (ctx_offload). Compression reports its own savings; these
        // reached no metric at all, so a body that genuinely shrank still
        // showed `tok_saved=0` in /stats and the dashboard.
        let mut ctx_transform_tokens_saved: i64 = 0;
        // The response-side usage block is the only authoritative cache-write
        // total. Retain whether this request injected a proactive expansion so
        // it can be attributed there without estimating from added bytes.
        let mut proactive_expansion_applied = false;
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

            // Derived once and shared by the volatile warnings below and the
            // drift detector after them. `derive_session_key` costs up to six
            // SHA-256 digests over canonicalized subtree clones, so deriving it
            // per consumer would put that on the hot path of every request
            // twice over — for a log field.
            let session_identity = match (drift_kind, headers_snapshot.as_ref()) {
                (Some(kind), Some(headers)) => {
                    let key = derive_session_key(headers, &client_addr, &parsed, kind);
                    let conversation =
                        cache_stabilization::usage_observer::conversation_key(&parsed, &key);
                    Some((kind, key, conversation))
                }
                _ => None,
            };

            if !findings.is_empty() {
                // Same identity the drift and recache events carry, so a
                // volatile finding can be joined to the bust it is suspected of
                // causing. Item 4 cannot be settled without it: the warning
                // fires on static sample text as readily as on real per-request
                // churn, and only a per-conversation join tells the two apart.
                let session_hash = session_identity.as_ref().map(|(_, key, _)| {
                    cache_stabilization::drift_detector::session_key_log_prefix(key)
                });
                cache_stabilization::volatile_detector::emit_volatile_warnings(
                    &findings,
                    &request_id,
                    session_hash.as_deref(),
                    session_identity.as_ref().map(|(_, _, conv)| conv.as_str()),
                );
            }

            if let Some((kind, session_key, conversation)) = session_identity {
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
                    conversation,
                    // The drift detector's own hash, not a re-derivation:
                    // a recache event is only joinable to the drift event
                    // that explains it if both print the same value.
                    Some(cache_stabilization::drift_detector::session_key_log_prefix(
                        &session_key,
                    )),
                    drift_dims,
                    Some(cache_stabilization::usage_observer::prefix_fingerprint(
                        &parsed,
                    )),
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
                    let project_dir = resolve_ctx_project(headers_snapshot.as_ref(), &parsed);
                    observer.observe(&parsed, &session_key, &project_dir);
                }

                // Session-sticky provider beta headers — port of the
                // Python PR-A6 `SessionBetaTracker`. Beta headers are
                // part of the bytes that determine the upstream
                // prefix-cache key; a client dropping a token between
                // turns rotates the key and re-writes the whole
                // prefix at the customer's cost. Forward the
                // per-conversation union instead. See
                // `cache_stabilization::beta_sticky` for the behavior
                // contract, the auth-mode rationale (applies to every
                // mode, like the Python handler), and the one
                // documented divergence from Python (per-conversation
                // keying). Reuses the drift detector's `session_key`
                // so both cache-stability subsystems agree on
                // conversation identity. Mutates upstream-bound
                // HEADERS only; body bytes stay untouched (Phase-A
                // cache-safety invariant).
                if state.config.beta_header_sticky.is_enabled() {
                    let provider = match endpoint {
                        compression::CompressibleEndpoint::AnthropicMessages => {
                            BetaProvider::Anthropic
                        }
                        compression::CompressibleEndpoint::OpenAiChatCompletions
                        | compression::CompressibleEndpoint::OpenAiResponses => {
                            BetaProvider::OpenAi
                        }
                    };
                    cache_stabilization::beta_sticky::apply_sticky_betas(
                        &state.beta_sticky,
                        provider,
                        &session_key,
                        &mut outgoing_headers,
                        &request_id,
                    );
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
                    // `None` when the body carries no turn array we can key
                    // on, in which case the request is neither served from
                    // the cache nor stored in it.
                    if let Some(entry) = crate::semantic_cache::cache_key_inputs(&parsed)
                        .and_then(|(messages, extra)| cache.get(&messages, model, &extra))
                    {
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

        // Freeze-replay: snapshot the ORIGINAL CLIENT messages — before the CTX
        // stage below rewrites `buffered`, and before the dispatcher consumes
        // it. The overlay stage needs them for the append-only guard (the
        // previous turn's originals must be an exact canonical prefix of these)
        // and to record this turn for the next one.
        //
        // Capturing this AFTER the CTX rewrite was a defect, and an expensive
        // one: `ctx_offload` collapses and restores `tool_result` blocks, so
        // the "originals" moved whenever our own offload decisions moved. The
        // guard then compared our output against our output, saw a difference
        // we had introduced, declined to replay, and busted the very cache the
        // replay exists to protect — blaming the client for it in the log.
        // Measured 2026-08-09: content-array length changes were 6 of 7 sampled
        // divergences, and one such bust cost 452,172 tokens on a single turn.
        //
        // The invariant this restores: `original` is what the CLIENT sent and
        // nothing else; `forwarded` is whatever we produced. Replay already
        // guarantees the forwarded side is byte-stable turn to turn, so our own
        // rewrites can no longer trip the guard. Both sides of the comparison
        // are captured here, so the stored and current flavours always match.
        // History keeps its own `<system-reminder>` spans. Lifting them onto the
        // newest user message used to run here, and it put the whole accumulated
        // block past the last cache breakpoint, where the provider bills it fresh
        // every turn — the block moves with the tail, so it can never sit at the
        // same prefix offset twice. Measured 2026-08-16: 22.0M uncached input
        // tokens over 1093 turns, 64% of all billed weight, spent to avoid drift
        // that wasted 346k. Roughly 20:1 against.
        //
        // Replay covers what the lift covered, and the lift only ever ran when
        // replay was on. `canonicalize_for_prefix_compare` already drops these
        // spans from the append-only guard's key, and the overlay forwards the
        // PREVIOUS turn's bytes for the whole stored prefix — the bytes the
        // provider actually cached. A span the client attaches to or withdraws
        // from a message inside that prefix therefore never reaches the wire,
        // which is the guarantee the lift existed to provide. Past the stored
        // prefix the client's own bytes go out either way, so there is nothing
        // there to protect.

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

        let buffered = if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) && (state.ctx_inject.is_some() || state.ctx_offload.is_some())
        {
            match serde_json::from_slice::<serde_json::Value>(&buffered) {
                Ok(mut value) => {
                    let ctx_session_key = request_session_key.clone();
                    let mut changed = false;
                    // Which project's stores recall reads and offload writes.
                    let ctx_project = resolve_ctx_project(headers_snapshot.as_ref(), &value);
                    let ccr_workspace = resolve_ccr_workspace(headers_snapshot.as_ref(), &value);
                    let latest_user_query = latest_user_query(&value);
                    let turn_number = anthropic_turn_number(&value);

                    // One ceiling for every stage that appends to this turn.
                    // Drawn down in the order the stages run below; without it
                    // three independently-capped appenders could inflate the
                    // request while each looked small on its own counter.
                    let injection_budget = crate::injection_budget::InjectionBudget::for_request(
                        state.config.max_injection_bytes,
                        &request_id,
                    );

                    if let Some((workspace_key, workspace_label)) = ccr_workspace.as_ref() {
                        if maybe_append_ccr_proactive_expansion(
                            &state,
                            &mut value,
                            &latest_user_query,
                            workspace_key,
                            workspace_label.as_deref(),
                            turn_number,
                            &request_id,
                            &injection_budget,
                        ) {
                            changed = true;
                            proactive_expansion_applied = true;
                        }
                    } else if state.ccr_context_tracker.is_some() {
                        tracing::info!(
                            request_id = %request_id,
                            "CCR Phase 4: workspace unresolved; proactive expansion disabled for this request"
                        );
                    }

                    if let Some(engine) = state.ctx_inject.as_ref() {
                        let session_key = ctx_session_key.clone();
                        if engine.maybe_inject_for_request(
                            &mut value,
                            &session_key,
                            &ctx_project,
                            &injection_budget,
                            &request_id,
                        ) {
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
                        // Logged whenever a block QUALIFIED, converted or not.
                        // `changed()` is only true for conversions, and this
                        // event used to sit behind it — so a turn that deferred
                        // every candidate logged nothing at all, which reads
                        // exactly like a turn with no candidates. Offload can
                        // sit idle for an entirely healthy reason (no rebuild
                        // boundary yet) and there was no way to tell that from
                        // being switched off, which cost an hour of looking for
                        // a fault in a working build.
                        if out.blocks_offloaded > 0 || out.blocks_deferred > 0 {
                            // CTX-6: offload metrics are recorded by the
                            // offload-store worker after persist_one confirms
                            // the record is durably recoverable, not here —
                            // see ctx/offload_store.rs.
                            tracing::info!(
                                event = "ctx_offload_accounting",
                                request_id = %request_id,
                                blocks_offloaded = out.blocks_offloaded,
                                blocks_deferred = out.blocks_deferred,
                                window_offloads = out.window_offloads,
                                tokens_saved = out.tokens_saved,
                                rebuild_boundary,
                                "ctx_offload considered tool_result blocks"
                            );
                        }
                        if out.changed() {
                            changed = true;
                            ctx_transform_tokens_saved += out.tokens_saved;
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
                            // Info, not debug: in tool mode this is the only
                            // proof the model was ever offered memory. Without
                            // it, "the tools never arrived" and "the model
                            // chose not to call them" both read as an empty
                            // log. Logged on both branches for that reason.
                            let added = new_tools.len().saturating_sub(existing.len());
                            if injected {
                                if let Some(obj) = value.as_object_mut() {
                                    obj.insert(
                                        "tools".to_string(),
                                        serde_json::Value::Array(new_tools),
                                    );
                                    changed = true;
                                    tracing::info!(
                                        request_id = %request_id,
                                        event = "memory_tools_injected",
                                        tools_added = added,
                                        tools_total = existing.len() + added,
                                    );
                                }
                            } else {
                                tracing::info!(
                                    request_id = %request_id,
                                    event = "memory_tools_not_injected",
                                    tools_present = existing.len(),
                                );
                            }
                        }
                    }

                    // CCR: inject the `headroom_retrieve` tool definition
                    // into the request body so the LLM can retrieve original
                    // uncompressed content by hash. Only when compression
                    // has produced CCR markers and the feature is enabled.
                    //
                    // `can_resolve` states the invariant rather than fixing a
                    // live bug: the enclosing block already runs for Anthropic
                    // only, so the non-Anthropic arm of the `match endpoint`
                    // below is currently unreachable. It is kept because the
                    // gate is what makes lifting that outer restriction safe —
                    // whoever does it gets the OpenAI shapes excluded from
                    // *streaming* injection for free, since only the buffered
                    // arm of `forward_http` can answer those. Bedrock takes the
                    // same line from the other end by passing no CCR store at
                    // all (`bedrock/invoke.rs`).
                    let client_streams = value
                        .get("stream")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let can_resolve = matches!(
                        endpoint,
                        compression::CompressibleEndpoint::AnthropicMessages
                    ) || !client_streams;
                    // A request with no `tools` key still needs the retrieval
                    // tool once its content has been offloaded, otherwise the
                    // digest points at a tool the model was never given. The
                    // memory injector above takes the same line.
                    if state.config.ccr_inject_tool
                        && can_resolve
                        && ctx_transform_tokens_saved > 0
                        && value.get("tools").is_none()
                    {
                        value["tools"] = serde_json::json!([]);
                    }
                    if state.config.ccr_inject_tool && can_resolve {
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

                    // Memory: put back any answer held from a turn that also
                    // called a client tool. This request carries the client's
                    // `tool_result`, so the turn can finally be completed —
                    // and because it goes out as part of this request, the
                    // cache write it causes is the one this turn needed
                    // anyway. Prefix replay carries the repaired history
                    // forward from here.
                    if let Some(messages) = value.get_mut("messages").and_then(|v| v.as_array_mut())
                    {
                        let applied = match crate::memory::deferred::store().lock() {
                            Ok(mut held) if !held.is_empty() => held.apply(messages),
                            _ => 0,
                        };
                        if applied > 0 {
                            changed = true;
                            tracing::info!(
                                request_id = %request_id,
                                event = "memory_answer_restored",
                                restored = applied,
                                "memory: held answer returned to its turn"
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
                                let base_user_id = headers_snapshot
                                    .as_ref()
                                    .and_then(|h| h.get("x-headroom-user-id"))
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("default");
                                // Same partition as the tool path, so switching
                                // modes cannot make one project's memories
                                // visible to another.
                                let user_id = crate::memory::router::scoped_user_id(
                                    base_user_id,
                                    &crate::memory::router::RequestContext {
                                        headers: header_map_to_lowercase_strings(
                                            headers_snapshot.as_ref(),
                                        ),
                                        system_prompt: crate::memory::router::extract_system_prompt(
                                            &value,
                                        ),
                                        base_user_id: base_user_id.to_string(),
                                        project_root_override: None,
                                    },
                                );
                                // Memory runs last, so it sees whatever the
                                // expansion and recall stages left. Clipping
                                // here is cache-safe: this appends to the live
                                // tail, which is re-sent every turn anyway.
                                if let Some(context) = handler
                                    .search_and_format_context(
                                        &user_id, &msgs, None, // request_context
                                        None, // ranker
                                        None, // query
                                        None, // budget
                                    )
                                    .await
                                    .and_then(|context| {
                                        injection_budget.take(
                                            crate::injection_budget::InjectionStage::Memory,
                                            context,
                                        )
                                    })
                                {
                                    // `frozen_message_count` indexes into
                                    // `messages`. This passed the length of the
                                    // *system* array instead — a count of system
                                    // blocks standing in for a count of messages.
                                    // With two system blocks the callee skipped
                                    // `messages[0..2]`, so a conversation one or
                                    // two messages long had no eligible tail and
                                    // got no memory at all.
                                    //
                                    // Zero is the honest value here. The real
                                    // frozen boundary comes from the prefix-replay
                                    // tracker, which does not run until
                                    // `apply_prefix_replay` further down. The
                                    // guard is inert regardless: the callee walks
                                    // backwards for the last user message, and the
                                    // turn being sent is by definition not in the
                                    // cached prefix.
                                    let (new_msgs, bytes) = crate::memory::handler::MemoryHandler::append_to_latest_user_tail(
                                        &msgs,
                                        &context,
                                        provider,
                                        0,
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
                                    runtime.store.persist(records, &ctx_project);
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

        // Captured above, BEFORE the CTX stage rewrote `buffered` — see the
        // snapshot next to that reassignment for why the ordering is the whole
        // point.

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
            let ccr_store_for_compression = state.ccr_store();
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
                        // Compression stores the original here, so a
                        // `headroom_retrieve` call can bring it back. Without
                        // it the lossy path is one-way.
                        ccr_store_for_compression.as_deref(),
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
                // Keep PERF savings scoped to the compression dispatcher. CTX
                // offload has its own per-request accounting below; folding it
                // into this field makes a prior offload re-application look like
                // compression savings and can re-emit a conversation-sized value.
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
                proactive_expansion_applied,
                // Filled in at the send point, where the final body exists.
                wire_bytes: None,
                forwarded_tokens_estimate: 0,
                upstream_attempts: 1,
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
                    hex::encode(digest)
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
                // Park the saving so the response side can price it against
                // the billed usage — the two halves of "is this worth running"
                // are produced on opposite sides of the request.
                state.usage_observer.note_compression(
                    &request_id,
                    tokens_before as u64,
                    tokens_after as u64,
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
        // Hold the working-directory line in `system` still. Runs BEFORE prefix
        // replay so the note it adds is part of the tail the overlay stores, and
        // so breakpoint placement inside `apply_prefix_replay` sees it. The
        // comparison the overlay makes is against the client's own
        // `replay_original_messages`, captured earlier, so the note cannot make
        // this turn look like a divergence.
        //
        // Gated on `prefix_replay` because the note only stays in history if the
        // next turn replays what we forwarded. Without replay the client re-sends
        // that message without the note every turn, and the prefix breaks at the
        // tail each time — the hold would then cause the churn it exists to stop.
        let body_to_send = if state.config.hold_working_directory
            && state.config.prefix_replay
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            )
            && !request_session_key.is_empty()
        {
            hold_working_directory(
                body_to_send,
                &state.working_dir_pins,
                &request_session_key,
                &request_id,
            )
        } else {
            body_to_send
        };

        // Ahead of prefix replay, which parks the forwarded message array in
        // the replay store to overlay onto the next turn. Stripping after that
        // would leave the store holding a block that never went on the wire,
        // and every later turn would overlay it back in — the proxy's idea of
        // the cached prefix drifting from the provider's, which is the shape
        // of a `cache_recache_observed` mismatch. Running here also means the
        // tail-breakpoint stage below lands its marker on the block that
        // really ends the message.
        let body_to_send = if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
            drop_unsigned_reasoning_blocks(body_to_send, &request_id)
        } else {
            body_to_send
        };

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
                    Some(&state.usage_observer),
                    state.started_at.elapsed().as_secs(),
                    state.config.cache_tail_breakpoints as usize,
                    state.config.strip_system_cache_breakpoints,
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
                let body_to_send = if state.config.image_optimize {
                    maybe_optimize_images(body_to_send, &request_id)
                } else {
                    body_to_send
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

        // Tail breakpoint. Before the TTL pin, so the moved marker is one of
        // the markers that pin covers.
        let body_to_send = if state.config.cache_tail_breakpoint
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            ) {
            maybe_push_tail_breakpoint(body_to_send, &request_id)
        } else {
            body_to_send
        };

        // B1 cache-TTL pin. Last of all, so every marker any earlier stage
        // placed or moved is covered. Skipped on PAYG, where a 1h write is
        // priced 60% above a 5m one and the operator pays the difference in
        // dollars rather than in a token-counted usage window.
        let body_to_send = if (state.config.force_1h_cache_ttl || state.config.split_cache_ttl)
            && auth_mode != AuthMode::Payg
            && matches!(
                endpoint,
                compression::CompressibleEndpoint::AnthropicMessages
            ) {
            // The split takes precedence: pinning the moving message tail to 1h
            // buys an hour of retention for content the next turn supersedes in
            // seconds, at 2.0x base input against 5m's 1.25x.
            maybe_pin_cache_ttl(body_to_send, &request_id, state.config.split_cache_ttl)
        } else {
            body_to_send
        };

        // Footprint accounting. Last, so `body_to_send` is what actually goes
        // on the wire. `tokens_saved` is measured after the injection stages
        // have already run, so without this the bytes the proxy adds are baked
        // into its own baseline and never appear as a cost.
        if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
            record_request_footprint(&state.savings_tracker, &original_buffered, &body_to_send);
        }

        cache_stabilization::capture::maybe_capture_outbound(&body_to_send, &request_id);

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

        // Signed reasoning blocks, checked after every stage that can rewrite
        // the message array has run.
        let body_to_send = if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
            restore_client_reasoning_blocks(body_to_send, &original_buffered, &request_id)
        } else {
            body_to_send
        };

        // `cache_control` TTL ordering, last of all: it has to read the
        // markers every stage above left behind, including the ones the
        // restore just put back.
        let body_to_send = if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
            enforce_cache_control_ttl_order(
                body_to_send,
                &original_buffered,
                // B1 pins every marker to 1h on the operator's orders, so a 1h
                // marker on such a turn is theirs, not a leak from an earlier
                // one.
                state.config.force_1h_cache_ttl && auth_mode != AuthMode::Payg,
                &request_id,
            )
        } else {
            body_to_send
        };

        // Wire footprint. The last measurement before the body leaves, so it
        // covers every stage — compression, routing, prune, replay, TTL, hooks
        // — not just the compression dispatcher that `tok_saved` is measured
        // at. `tok_saved` counts tokens freed mid-pipeline and cannot answer
        // "did fewer bytes actually reach the provider"; only these two
        // numbers, read together, can. Logged once per request rather than
        // per attempt, because a retry re-sends the same bytes.
        {
            let sent = body_to_send.len() as i64;
            let received = original_buffered_len as i64;
            tracing::info!(
                target: "headroom.proxy",
                event = "outbound_body_bytes",
                request_id = %request_id,
                path = %path_for_log,
                bytes_in = received,
                bytes_out = sent,
                bytes_delta = sent - received,
                "outbound body size measured on the wire"
            );
            // Same site, same bytes: this is what actually left the proxy, so
            // the fingerprints describe the prefix the provider keyed on.
            log_prefix_composition(&request_id, &body_to_send);
            // Feed the ground-truth ledger. Sizes come off the wire, and the
            // arm label makes a compression-on vs compression-off comparison a
            // query instead of an argument.
            state.usage_observer.note_wire_bytes(
                &request_id,
                received.max(0) as u64,
                sent.max(0) as u64,
                state.config.compression_mode.as_str(),
            );
            // Hand the pair to the outcome, which books it against the
            // provider's usage once the response reports one.
            if let Some(ctx) = outcome_ctx.as_mut() {
                ctx.wire_bytes = Some((received, sent));
                ctx.forwarded_tokens_estimate = headroom_core::tokenizer::get_tokenizer(&ctx.model)
                    .count_text(&String::from_utf8_lossy(&body_to_send))
                    as i64;
            }
        }

        retry_body = Some(body_to_send.clone());

        // Forward the request with retry on transient errors (429, 529, 5xx).
        let max_attempts = if state.config.retry_enabled {
            state.config.retry_max_attempts.max(1)
        } else {
            1
        };
        // An overload reported inside a 200 body gets its own, longer budget:
        // it is the one retry here that cannot duplicate output, and the
        // outages it rides out run tens of seconds rather than a blip. The
        // loop has to spin far enough for it; every other branch keeps
        // checking `max_attempts` and falls through when that runs out.
        let overload_max_attempts = if state.config.retry_enabled {
            state.config.retry_overload_max_attempts.max(max_attempts)
        } else {
            1
        };
        let loop_attempts = max_attempts.max(overload_max_attempts);
        let mut last_err: Option<ProxyError> = None;
        {
            let mut result = None;
            let mut attempts_made = 0i64;
            for attempt in 0..loop_attempts {
                attempts_made = i64::from(attempt + 1);
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
                            let retry_after_header = r.headers().contains_key("retry-after");
                            let retry_after_uncapped = r
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(headroom_core::retry::retry_after_ms_uncapped);
                            let retry_after_exceeds_cap =
                                retry_after_uncapped.is_some_and(|delay| delay > max_delay as f64);
                            if retry_after_exceeds_cap {
                                tracing::warn!(
                                    event = "upstream_retry_after_exceeds_cap",
                                    request_id = %request_id,
                                    status,
                                    attempt = attempt + 1,
                                    max_attempts,
                                    retry_after_ms = retry_after_uncapped.unwrap_or_default(),
                                    retry_max_delay_ms = max_delay,
                                    session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(&request_session_key),
                                    "upstream Retry-After exceeds the internal wait cap; returning the response without an early retry"
                                );
                            } else {
                                let retry_after = retry_after_uncapped
                                    .map(|delay| delay.ceil().min(u64::MAX as f64) as u64);
                                let delay_source = if retry_after.is_some() {
                                    "header"
                                } else {
                                    "backoff"
                                };
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
                                    retry_after_header,
                                    delay_source,
                                    retry_after_clamped = false,
                                    session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(&request_session_key),
                                    "upstream returned retryable status; retrying"
                                );
                                crate::observability::record_upstream_retry(
                                    "anthropic",
                                    crate::observability::retry_reason::from_status(status),
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                                    .await;
                                continue;
                            }
                        }
                        // Anthropic reports rate limits and overload *inside*
                        // a 200 body when the client asked for a stream: the
                        // headers say success, then the first SSE event is
                        // `{"type":"error","error":{"type":"overloaded_error"}}`.
                        // A retry loop that only reads `r.status()` is blind to
                        // it and hands the client a turn that never started.
                        //
                        // Peeking the first event is safe because nothing has
                        // been forwarded yet — the bytes are still ours. Once
                        // content has gone out we cannot retry without
                        // duplicating it, so only a *leading* error qualifies;
                        // one that arrives later ends the stream without
                        // `message_stop` and is caught by the gate in
                        // `run_sse_state_machine` instead.
                        let mut r = r;
                        let (prefix, leading_error) = peek_leading_sse_error(&mut r).await;
                        if let Some(kind) = leading_error {
                            if attempt + 1 < overload_max_attempts {
                                let base = state.config.retry_base_delay_ms;
                                let max = state.config.retry_max_delay_ms;
                                let delay_ms = base.saturating_mul(1u64 << attempt).min(max);
                                tracing::warn!(
                                    request_id = %request_id,
                                    error_type = %kind,
                                    attempt = attempt + 1,
                                    max_attempts = overload_max_attempts,
                                    delay_ms = delay_ms,
                                    "upstream reported an error inside a 200 stream; retrying"
                                );
                                crate::observability::record_upstream_retry(
                                    "anthropic",
                                    crate::observability::retry_reason::IN_BAND_SSE,
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                                    .await;
                                continue;
                            }
                            tracing::warn!(
                                request_id = %request_id,
                                error_type = %kind,
                                attempts = overload_max_attempts,
                                "upstream error inside a 200 stream survived every retry"
                            );
                            crate::observability::record_upstream_retry_exhausted(
                                "anthropic",
                                crate::observability::retry_reason::IN_BAND_SSE,
                            );
                        }
                        result = Some((r, prefix));
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
                            crate::observability::record_upstream_retry(
                                "anthropic",
                                crate::observability::retry_reason::TRANSPORT,
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            last_err = Some(ProxyError::Upstream(e));
                            continue;
                        }
                        if is_retryable {
                            crate::observability::record_upstream_retry_exhausted(
                                "anthropic",
                                crate::observability::retry_reason::TRANSPORT,
                            );
                        }
                        return Err(ProxyError::Upstream(e));
                    }
                }
            }
            let resolved = result.ok_or_else(|| {
                last_err.unwrap_or_else(|| {
                    ProxyError::InvalidUpstream("retry loop exhausted".to_string())
                })
            })?;
            if let Some(ctx) = outcome_ctx.as_mut() {
                ctx.upstream_attempts = attempts_made.max(1);
            }
            resolved
        }
    } else {
        // Pure streaming path — the original passthrough behaviour. No peek
        // here: passthrough is byte-faithful by contract, and this path has no
        // retry loop to feed anyway.
        let body_stream =
            TryStreamExt::map_err(req.into_body().into_data_stream(), std::io::Error::other);
        let reqwest_body = reqwest::Body::wrap_stream(body_stream);
        (
            state
                .client
                .request(reqwest_method.clone(), upstream_url.clone())
                .headers(outgoing_headers.clone())
                .body(reqwest_body)
                .send()
                .await?,
            bytes::Bytes::new(),
        )
    };
    // Bytes already read off the body while checking for a leading in-band
    // error. They lead the client's stream so nothing is lost.
    let (upstream_resp, sse_prefix) = upstream_resp;

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
    // CCR retrieval on the streamed path. The proxy offers the model a
    // `headroom_retrieve` tool on every intercepted request; on a streamed
    // response the call used to travel straight to a client that has no such
    // tool. `rewrite_anthropic_stream` answers it here — suppressing the
    // block, running the continuation, splicing the result back in — so the
    // streamed turn behaves like the buffered one.
    //
    // Eligibility mirrors the buffered branch below: same feature flags, same
    // store requirement, same path. When any of them is off the upstream body
    // is handed on untouched and this costs one boolean.
    let ccr_stream_eligible = is_sse
        && status.is_success()
        && matches!(sse_kind, SseStreamKind::Anthropic)
        && state.config.ccr_handle_responses
        && state.ctx_offload.is_some()
        && path_for_log.contains("/v1/messages");
    // Put the peeked bytes back at the head of the stream. `sse_prefix` is
    // empty on every path that did not peek, so this is a no-op there.
    let upstream_body = {
        let rest = upstream_resp.bytes_stream();
        let head =
            futures_util::stream::iter((!sse_prefix.is_empty()).then(|| Ok(sse_prefix.clone())));
        head.chain(rest)
    };
    // A stream that dies part-way leaves the turn dead, and the retry loop above
    // is long gone by then — it only ever saw the headers. This wrapper holds the
    // opening bytes back so an early drop can start a new request instead of
    // reaching the client. It sits below CCR and below the telemetry tee, so a
    // discarded attempt is invisible to both.
    let upstream_body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
    > = if let Some(body) = retry_body.filter(|_| {
        is_sse
            && status.is_success()
            && state.config.retry_enabled
            && state.config.retry_stream_hold_bytes > 0
            && state.config.retry_max_attempts > 1
    }) {
        Box::pin(crate::sse::stream_retry::retry_on_early_drop(
            upstream_body,
            crate::sse::stream_retry::RetryContext {
                client: state.client.clone(),
                method: reqwest_method.clone(),
                url: upstream_url.to_string(),
                headers: outgoing_headers.clone(),
                body,
                request_id: request_id.clone(),
                max_attempts: state.config.retry_max_attempts,
                base_delay_ms: state.config.retry_base_delay_ms,
                max_delay_ms: state.config.retry_max_delay_ms,
                hold_bytes: state.config.retry_stream_hold_bytes,
            },
        ))
    } else {
        Box::pin(upstream_body)
    };
    let (upstream_body, ccr_round_usage): (
        std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
        Option<Arc<Mutex<CcrRoundUsage>>>,
    ) = if ccr_stream_eligible {
        let ctx = crate::sse::ccr_stream::CcrStreamContext {
            client: state.client.clone(),
            upstream_url: upstream_url.clone(),
            outgoing_headers: outgoing_headers.clone(),
            original_request: original_buffered.clone(),
            ccr_store: state
                .ctx_offload
                .as_ref()
                .expect("ctx_offload checked above")
                .store
                .ccr(),
            config: state.config.clone(),
            request_id: request_id.clone(),
            shape: crate::sse::ccr_stream::CcrShape::Anthropic,
            memory: memory_tool_context(
                &state,
                &headers_snapshot,
                Some("anthropic"),
                &original_buffered,
            )
            .await,
        };
        let (stream, usage) = crate::sse::ccr_stream::rewrite_anthropic_stream(upstream_body, ctx);
        (Box::pin(stream), Some(usage))
    } else {
        (Box::pin(upstream_body), None)
    };

    let rid = request_id.clone();
    let parser_telemetry = std::sync::Arc::new(ParserTelemetry::default());
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
        let parser_task = tokio::spawn(run_sse_state_machine(
            sse_kind,
            rx,
            rid_for_parser.clone(),
            state.usage_observer.clone(),
            outcome_ctx.clone(),
            replay_store_for_parser,
            ccr_round_usage.clone(),
        ));
        // Keep the parser detached from response forwarding, but do not drop
        // its JoinHandle: a panic would otherwise erase the only completion
        // record for this request. The waiter preserves the streaming path and
        // makes task panics/cancellation operator-visible.
        let waiter_telemetry = parser_telemetry.clone();
        tokio::spawn(async move {
            let result = parser_task.await;
            let sent_chunks = waiter_telemetry
                .sent_chunks
                .load(std::sync::atomic::Ordering::Relaxed);
            let dropped_chunks = waiter_telemetry
                .dropped_chunks
                .load(std::sync::atomic::Ordering::Relaxed);
            match result {
                // A clean finish is already announced once per stream by
                // `sse stream closed`, so this stays quiet unless the chunk
                // counts say something that line cannot: a parser that missed
                // input because its queue was full or already closed. Logging
                // every clean finish at info would double the per-stream volume
                // of a log that is never rotated.
                Ok(()) if dropped_chunks > 0 => tracing::warn!(
                    request_id = %rid_for_parser,
                    sent_chunks,
                    dropped_chunks,
                    "sse state-machine task completed having missed chunks; \
                     its usage totals are short by whatever those carried"
                ),
                Ok(()) => tracing::debug!(
                    request_id = %rid_for_parser,
                    sent_chunks,
                    dropped_chunks,
                    "sse state-machine task completed"
                ),
                Err(error) => tracing::error!(
                    request_id = %rid_for_parser,
                    sent_chunks,
                    dropped_chunks,
                    task_panic = error.is_panic(),
                    task_cancelled = error.is_cancelled(),
                    error = %error,
                    "sse state-machine task failed"
                ),
            }
        });
        Some(tx)
    } else {
        None
    };
    let resp_stream = upstream_body.map(move |r| match r {
        Ok(b) => {
            if let Some(tx) = &parser_tx {
                if let Err(e) = tx.try_send(b.clone()) {
                    parser_telemetry
                        .dropped_chunks
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::debug!(
                        request_id = %rid,
                        error = %e,
                        "sse parser queue full or closed; skipping telemetry chunk"
                    );
                } else {
                    parser_telemetry
                        .sent_chunks
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    // An upstream rejection arrives as a small JSON body that streams straight
    // through to the client, so the proxy never learns why its own request was
    // refused. That is the most expensive blind spot here: a rejected turn is a
    // whole turn lost, worse than any cache miss, and until now the log showed
    // only `upstream_status=400`. Buffer the body, log the provider's reason,
    // and hand the same bytes on unchanged.
    let should_buffer_error = !is_sse && (status.is_client_error() || status.is_server_error());
    let body = if should_buffer_error {
        let body_stream = Body::from_stream(resp_stream);
        match http_body_util::BodyExt::collect(body_stream).await {
            Ok(collected) => {
                let body_bytes = collected.to_bytes();
                let (kind, detail) = describe_upstream_error(&body_bytes);
                tracing::warn!(
                    request_id = %request_id,
                    event = "upstream_rejected",
                    path = %path_for_log,
                    upstream_status = status.as_u16(),
                    error_type = %kind,
                    error_message = %detail,
                    body_bytes = body_bytes.len(),
                    "upstream refused the forwarded request"
                );
                // The per-request warn above is one line among thousands. This
                // keeps the ratio and escalates on its own when refusals stop
                // being occasional — the signal that was missing while a
                // splice defect refused a fifth of subagent turns for a day.
                crate::observability::upstream_health::observe_rejection_reason(
                    status.as_u16(),
                    &kind,
                    &detail,
                );
                if let Some(ctx) = outcome_ctx.as_ref() {
                    emit_failed_http_outcome(ctx, &request_id, status, Some(&body_bytes));
                }
                Body::from(body_bytes)
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    event = "upstream_rejected",
                    upstream_status = status.as_u16(),
                    error = %e,
                    "upstream refused the forwarded request and the body could not be read"
                );
                crate::observability::upstream_health::observe_rejection_reason(
                    status.as_u16(),
                    "unreadable_body",
                    &e.to_string(),
                );
                if let Some(ctx) = outcome_ctx.as_ref() {
                    emit_failed_http_outcome(ctx, &request_id, status, None);
                }
                Body::empty()
            }
        }
    } else if should_buffer_for_cache {
        // Wrap the mapped stream into a hyper Body so BodyExt::collect can
        // buffer it. This is only for non-SSE success responses where we
        // want to cache the full body.
        let body_stream = Body::from_stream(resp_stream);
        match http_body_util::BodyExt::collect(body_stream).await {
            Ok(collected) => {
                let mut body_bytes = collected.to_bytes();
                // Usage from CCR continuation rounds, which are billed
                // upstream calls the client never sees. Stays zero unless
                // the model asked for a retrieval.
                let mut ccr_round_usage = CcrRoundUsage::default();
                // Same for a turn hook's own re-drives. Stays zero unless a
                // hook is registered and calls the model again.
                let mut turn_hook_usage = TurnHookUsage::default();

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
                        let (resolved, extra) = handle_ccr_response(
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
                        body_bytes = resolved;
                        ccr_round_usage = extra;
                    }
                }

                // Memory tools: same contract as CCR above. The proxy injects
                // `memory_search` and friends, so the proxy runs them — the
                // client has never heard of them.
                let memory_provider = if path_for_log.contains("/v1/messages") {
                    Some("anthropic")
                } else if path_for_log.contains("/v1/chat/completions") {
                    Some("openai")
                } else if path_for_log.contains("/v1/responses") {
                    Some("openai_responses")
                } else {
                    None
                };
                if let Some(memory) = memory_tool_context(
                    &state,
                    &headers_snapshot,
                    memory_provider,
                    &original_buffered,
                )
                .await
                {
                    if let Some(provider) = memory_provider {
                        let (resolved, extra) = handle_memory_response(
                            &body_bytes,
                            &original_buffered,
                            &upstream_url,
                            &state.client,
                            &memory,
                            &state.config,
                            &request_id,
                            &outgoing_headers,
                            provider,
                        )
                        .await;
                        body_bytes = resolved;
                        ccr_round_usage.absorb(extra);
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
                    // The outcome block parses `usage` by the finer label, and
                    // the hook's own calls have to be read the same way.
                    let usage_provider = outcome_ctx
                        .as_ref()
                        .map(|c| c.provider.clone())
                        .unwrap_or_else(|| provider.to_string());
                    let (hooked, hook_usage) = apply_response_hooks(
                        body_bytes,
                        &original_buffered,
                        provider,
                        &usage_provider,
                        &upstream_url,
                        &state.client,
                        &outgoing_headers,
                        &request_id,
                    )
                    .await;
                    body_bytes = hooked;
                    turn_hook_usage = hook_usage;
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
                            let response_headers: std::collections::HashMap<String, String> =
                                resp_headers
                                    .iter()
                                    .filter_map(|(k, v)| {
                                        v.to_str()
                                            .ok()
                                            .map(|val| (k.as_str().to_string(), val.to_string()))
                                    })
                                    .collect();
                            // Same key derivation as the lookup above; the two
                            // have to move together or the cache stops hitting.
                            if let Some((messages, extra)) =
                                crate::semantic_cache::cache_key_inputs(&parsed)
                            {
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
                        observe_proactive_expansion_cache_write(
                            ctx,
                            u64::try_from(cache_write).unwrap_or(0),
                        );
                        // Fold in the CCR continuation rounds. The client saw
                        // one turn; the upstream billed several, and only the
                        // last one's usage is in `parsed`. Without this the
                        // savings figures are computed against a fraction of
                        // what the turn actually cost.
                        if !ccr_round_usage.is_empty() {
                            tracing::info!(
                                request_id = %request_id,
                                event = "ccr_continuation_usage",
                                rounds = ccr_round_usage.rounds,
                                input_tokens = ccr_round_usage.input_tokens,
                                output_tokens = ccr_round_usage.output_tokens,
                                cache_write_tokens = ccr_round_usage.cache_write_tokens,
                                "billed CCR continuation rounds the client never saw"
                            );
                        }
                        let attempted_input = attempted_input + ccr_round_usage.input_tokens;
                        let output_tok = output_tok + ccr_round_usage.output_tokens;
                        let cache_read = cache_read + ccr_round_usage.cache_read_tokens;
                        let cache_write = cache_write + ccr_round_usage.cache_write_tokens;
                        let uncached_input = uncached_input + ccr_round_usage.input_tokens;
                        // Same for a turn hook's re-drives. The usage parsed
                        // above describes the one response the hook handed
                        // back; anything it called on the way there was just
                        // as billed, and leaving it out lets a token-saving
                        // hook hide its overhead behind the saving it claims.
                        if !turn_hook_usage.is_empty() {
                            tracing::info!(
                                request_id = %request_id,
                                event = "turn_hook_usage",
                                calls = turn_hook_usage.calls,
                                input_tokens = turn_hook_usage.input_tokens,
                                output_tokens = turn_hook_usage.output_tokens,
                                cache_write_tokens = turn_hook_usage.cache_write_tokens,
                                "billed turn-hook re-drives the client never saw"
                            );
                        }
                        // Anthropic's `input_tokens` is already the uncached
                        // count; both OpenAI shapes report a total the read
                        // has to come off, exactly as above.
                        let hook_uncached_input = if ctx.provider == "anthropic" {
                            turn_hook_usage.input_tokens
                        } else {
                            (turn_hook_usage.input_tokens - turn_hook_usage.cache_read_tokens)
                                .max(0)
                        };
                        let attempted_input = attempted_input + turn_hook_usage.input_tokens;
                        let output_tok = output_tok + turn_hook_usage.output_tokens;
                        let cache_read = cache_read + turn_hook_usage.cache_read_tokens;
                        let cache_write = cache_write + turn_hook_usage.cache_write_tokens;
                        let uncached_input = uncached_input + hook_uncached_input;
                        // Read off the pre-CCR `usage`: continuation rounds fold
                        // into the write total above but carry no TTL breakdown,
                        // so the split stays a subset of it and pricing charges
                        // the remainder at the cheaper 5m rate.
                        let (cache_write_5m, cache_write_1h) = anthropic_cache_ttl_split(usage);
                        let outcome = headroom_core::request_outcome::RequestOutcome {
                            request_id: request_id.clone(),
                            provider: ctx.provider.clone(),
                            model: ctx.model.clone(),
                            status_code: status.as_u16() as i64,
                            upstream_attempts: ctx.upstream_attempts,
                            provider_input_tokens: usage.map(|_| {
                                if ctx.provider == "anthropic" {
                                    attempted_input + cache_read + cache_write
                                } else {
                                    attempted_input
                                }
                            }),
                            provider_output_tokens: usage.map(|_| output_tok),
                            original_tokens: ctx.sizes(attempted_input).0,
                            optimized_tokens: ctx.sizes(attempted_input).1,
                            output_tokens: output_tok,
                            tokens_saved: ctx.tokens_saved,
                            attempted_input_tokens: ctx.attempted(attempted_input),
                            cache_read_tokens: cache_read,
                            cache_write_tokens: cache_write,
                            cache_write_5m_tokens: cache_write_5m,
                            cache_write_1h_tokens: cache_write_1h,
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
                        record_wire_footprint(ctx, uncached_input, cache_read, cache_write);
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
    } else if is_sse && status.is_success() && matches!(sse_kind, SseStreamKind::Anthropic) {
        // Last stop before the client. `retry_on_early_drop` above saves the
        // drops it can still take back; past that point the only thing left to
        // save is the shape of the reply. This closes an interrupted message
        // properly — marked as truncated — so a dead connection costs one
        // short answer instead of the session. It runs above the telemetry
        // tee, so accounting still books the turn as the incomplete one it was.
        Body::from_stream(crate::sse::stream_finisher::finish_on_drop(
            resp_stream,
            request_id.clone(),
        ))
    } else {
        Body::from_stream(resp_stream)
    };

    // One observation per upstream response, whatever its status. The refusal
    // count alone is the number that let a 22.5% rejection rate pass for
    // ordinary bad luck; the ratio is what makes it obvious.
    crate::observability::upstream_health::observe_upstream_response(status.as_u16());

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

/// Which messages the proxy rewrote this turn, and which of those the provider
/// is entitled to refuse.
struct RewrittenMessages {
    /// Indices whose content differs from what the client sent.
    indices: Vec<usize>,
    /// The subset carrying a `thinking` or `redacted_thinking` block. Anthropic
    /// rejects a turn whose signed thinking blocks changed, so any index here is
    /// a rejection this proxy is capable of causing.
    with_thinking: Vec<usize>,
    /// Indices where a signed reasoning block itself differs on the wire.
    ///
    /// Compared raw, not canonically: `cache_control` is the one key this proxy
    /// rewrites on every message by design, and adding or removing it on a
    /// signed block is still a modification of that block as far as the provider
    /// is concerned. The canonical compare above is blind to exactly that, which
    /// is why this list is kept separately rather than folded into it.
    thinking_touched: Vec<usize>,
}

/// Compare what the client sent against what is about to go on the wire.
///
/// Uses the prefix canonicaliser, so `cache_control` placement — which this
/// proxy owns and rewrites every turn by design — does not count as a change.
fn rewritten_message_report(
    original: &[serde_json::Value],
    forwarded: &[serde_json::Value],
) -> RewrittenMessages {
    use cache_stabilization::prefix_replay::canonicalize_for_prefix_compare;
    let mut indices = Vec::new();
    let mut with_thinking = Vec::new();
    let mut thinking_touched = Vec::new();
    for (i, (before, after)) in original.iter().zip(forwarded.iter()).enumerate() {
        if thinking_blocks_differ(before, after) {
            thinking_touched.push(i);
        }
        if canonicalize_for_prefix_compare(before) == canonicalize_for_prefix_compare(after) {
            continue;
        }
        indices.push(i);
        if carries_thinking_block(before) || carries_thinking_block(after) {
            with_thinking.push(i);
        }
    }
    RewrittenMessages {
        indices,
        with_thinking,
        thinking_touched,
    }
}

/// True if the signed reasoning blocks of a message are not byte-identical
/// between what the client sent and what goes on the wire.
fn thinking_blocks_differ(before: &serde_json::Value, after: &serde_json::Value) -> bool {
    fn reasoning_blocks(message: &serde_json::Value) -> Vec<&serde_json::Value> {
        message
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| {
                        matches!(
                            b.get("type").and_then(|t| t.as_str()),
                            Some("thinking") | Some("redacted_thinking")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    reasoning_blocks(before) != reasoning_blocks(after)
}

/// True if a message's content holds a signed reasoning block.
fn carries_thinking_block(message: &serde_json::Value) -> bool {
    message
        .get("content")
        .and_then(|c| c.as_array())
        .is_some_and(|blocks| {
            blocks.iter().any(|b| {
                matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
        })
}

/// Every signed reasoning block in a message array, in order.
fn signed_reasoning_blocks(messages: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    messages
        .iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_array()))
        .flatten()
        .filter(|b| {
            let is_reasoning = matches!(
                b.get("type").and_then(|t| t.as_str()),
                Some("thinking") | Some("redacted_thinking")
            );
            // Genuinely signed, as the name says. What this guards is
            // Anthropic's refusal of a *signed* block that came back altered,
            // and a block with no signature has nothing to violate. Counting
            // those too would make `drop_unsigned_reasoning_blocks` — the one
            // stage that is meant to remove them — look like tampering, and
            // the restore below would put back the block that upstream is
            // about to refuse.
            is_reasoning && !is_unsigned_reasoning(b)
        })
        .collect()
}

/// Drop `thinking` blocks that carry no signature.
///
/// The counterpart to `sse::stream_finisher`. When an upstream stream dies
/// with a thinking block open, the finisher closes that block so the turn ends
/// cleanly — but the `signature_delta` never arrived, so the block the client
/// stores is unsigned. Anthropic refuses a thinking block without a valid
/// signature, which would turn one truncated answer into a conversation that
/// can no longer be sent at all.
///
/// So the blocks the proxy had to cut short are dropped on their way back up.
/// This runs first among the stages that care, ahead of prefix replay and the
/// tail breakpoint, so every one of them sees the message array that actually
/// reaches the provider. Stripping later would leave the replay store holding
/// a block that never went on the wire and overlaying it back in on every
/// later turn, which costs a re-cache rather than a refused turn.
///
/// `signed_reasoning_blocks` excludes exactly what this removes, so the
/// tampering guard downstream never mistakes this for a rewrite.
///
/// Signed blocks are never touched, and neither is a body without an unsigned
/// one — which is every body that never met a dropped stream.
fn drop_unsigned_reasoning_blocks(body_to_send: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    // Cheap gate: the overwhelming majority of bodies have no reasoning block
    // at all, and this spares them a parse.
    const MARKER: &[u8] = b"\"thinking\"";
    if !body_to_send.windows(MARKER.len()).any(|w| w == MARKER) {
        return body_to_send;
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body_to_send) else {
        return body_to_send;
    };
    let Some(messages) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return body_to_send;
    };
    let mut dropped = 0usize;
    let mut markers_moved = 0usize;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        // Removing every block would leave a message with empty content, which
        // upstream refuses just as firmly as the unsigned block does. Nothing
        // this proxy writes looks like that — `stream_finisher` always leaves a
        // text block behind — but history the proxy did not write reaches here
        // too, and trading one bad turn for a different bad turn is no trade.
        if content.iter().all(is_unsigned_reasoning) {
            continue;
        }
        // A `cache_control` marker on a doomed block is a cache breakpoint, and
        // dropping it silently would move the cached prefix boundary and cost a
        // re-cache on every later turn of the conversation. It rides to the
        // next surviving block instead — the same carry `thinking_compactor`
        // makes when it rewrites a block out from under a marker.
        let mut carried: Option<serde_json::Value> = None;
        let mut kept: Vec<serde_json::Value> = Vec::with_capacity(content.len());
        for mut block in content.drain(..) {
            if is_unsigned_reasoning(&block) {
                dropped += 1;
                if let Some(cc) = block.get("cache_control") {
                    carried = Some(cc.clone());
                }
                continue;
            }
            if let Some(cc) = carried.take() {
                // An existing marker wins: a second one here would spend a
                // breakpoint on the same boundary, and there are only four.
                if block.get("cache_control").is_none() {
                    block["cache_control"] = cc;
                    markers_moved += 1;
                }
            }
            kept.push(block);
        }
        // Still carrying means the dropped block was last, so the marker goes
        // to whatever ends the message now.
        if let Some(cc) = carried {
            if let Some(last) = kept.last_mut() {
                if last.get("cache_control").is_none() {
                    last["cache_control"] = cc;
                    markers_moved += 1;
                }
            }
        }
        *content = kept;
    }
    if dropped == 0 {
        return body_to_send;
    }
    let Ok(out) = serde_json::to_vec(&v) else {
        return body_to_send;
    };
    tracing::info!(
        request_id = %request_id,
        event = "unsigned_reasoning_blocks_dropped",
        dropped,
        markers_moved,
        "removed thinking blocks with no signature; they are the tail of a \
         stream that died mid-block and upstream would refuse them"
    );
    bytes::Bytes::from(out)
}

/// A `thinking` block the model never got to sign.
///
/// `redacted_thinking` carries opaque `data` rather than a signature and is
/// always delivered whole, so a block with `data` is complete whatever its
/// signature says.
fn is_unsigned_reasoning(block: &serde_json::Value) -> bool {
    let is_reasoning = matches!(
        block.get("type").and_then(|t| t.as_str()),
        Some("thinking") | Some("redacted_thinking")
    );
    let unsigned = block
        .get("signature")
        .and_then(|s| s.as_str())
        .is_none_or(|s| s.is_empty());
    is_reasoning && unsigned && block.get("data").is_none()
}

/// Put the client's message array back when the outbound body no longer
/// carries their signed reasoning blocks unchanged.
///
/// Anthropic refuses a turn whose signed `thinking` or `redacted_thinking`
/// blocks came back altered — "blocks cannot be modified", naming a message
/// index but not who modified it. The live-zone compressor excludes those
/// block types and every stage of the outbound chain returns its input
/// untouched when it has nothing to do, so today the invariant holds by
/// convention: prefix replay rewrites the message array wholesale, the hook
/// seam re-serializes whatever a hook hands back, and neither checks. This is
/// the check, taken once on the bytes that are about to leave.
///
/// Restoring only `messages` keeps every change made outside it — model
/// routing, tool pruning, the TTL pin — so a body that trips this costs one
/// turn's compression rather than the turn.
fn restore_client_reasoning_blocks(
    body_to_send: bytes::Bytes,
    original: &bytes::Bytes,
    request_id: &str,
) -> bytes::Bytes {
    if body_to_send == original {
        return body_to_send;
    }
    // Cheap gate: nothing downstream matters for a body with no signed block,
    // and that is the overwhelming majority of them.
    const MARKER: &[u8] = b"thinking";
    if !original.windows(MARKER.len()).any(|w| w == MARKER) {
        return body_to_send;
    }

    let (Ok(before), Ok(mut after)) = (
        serde_json::from_slice::<serde_json::Value>(original),
        serde_json::from_slice::<serde_json::Value>(&body_to_send),
    ) else {
        return body_to_send;
    };

    let empty = Vec::new();
    let before_messages = before
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);
    let after_messages = after
        .get("messages")
        .and_then(|m| m.as_array())
        .unwrap_or(&empty);
    if signed_reasoning_blocks(before_messages) == signed_reasoning_blocks(after_messages) {
        return body_to_send;
    }

    let restored = serde_json::Value::Array(before_messages.clone());
    let block_count = signed_reasoning_blocks(before_messages).len();
    let Some(map) = after.as_object_mut() else {
        return body_to_send;
    };
    map.insert("messages".to_string(), restored);
    match serde_json::to_vec(&after) {
        Ok(bytes) => {
            tracing::warn!(
                target: "headroom.proxy",
                event = "signed_reasoning_blocks_restored",
                request_id = %request_id,
                signed_blocks = block_count,
                messages_before = before_messages.len(),
                "outbound body altered the client's signed reasoning blocks; \
                 forwarding the client's message array instead"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body_to_send,
    }
}

/// Make the outbound body satisfy Anthropic's `cache_control` TTL ordering.
///
/// A `ttl: "1h"` marker behind a 5-minute one kills the whole turn with a 400,
/// and the sections are read as one sequence — `tools`, `system`, `messages` —
/// so a violation can straddle two of them and be invisible to any stage that
/// looks at one list. See [`cache_stabilization::ttl_order`] for the two
/// repairs and which one applies when.
fn enforce_cache_control_ttl_order(
    body_to_send: bytes::Bytes,
    original: &bytes::Bytes,
    forced_1h: bool,
    request_id: &str,
) -> bytes::Bytes {
    // Cheap gate: only a 1h marker can break the rule, and only a 1h marker
    // can have leaked in.
    const LONG_TTL: &[u8] = b"\"1h\"";
    if !body_to_send.windows(LONG_TTL.len()).any(|w| w == LONG_TTL) {
        return body_to_send;
    }
    // This repairs violations the proxy introduced. On a body no stage rewrote
    // there is nothing of ours to repair, and editing it would break the
    // passthrough guarantee, change the client's cache key, and hide a bug in
    // their request — Anthropic's own 400 is the honest answer. Ordered after
    // the marker scan so the comparison only runs on bodies that could break
    // the rule.
    if body_to_send == original {
        return body_to_send;
    }
    let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(&body_to_send) else {
        return body_to_send;
    };

    // Which lane the turn belongs to is the client's call — except when B1 is
    // pinning every marker to 1h, which is the operator asking for that lane
    // on their behalf. Reading an unparseable client body as "asked for 1h"
    // keeps a marker of theirs from being stripped on a guess.
    let client_asked_for_1h = forced_1h
        || serde_json::from_slice::<serde_json::Value>(original)
            .map(|client| cache_stabilization::ttl_order::asks_for_1h(&client))
            .unwrap_or(true);

    let repair =
        cache_stabilization::ttl_order::enforce_ttl_order(&mut parsed, client_asked_for_1h);
    if repair.is_noop() {
        return body_to_send;
    }
    match serde_json::to_vec(&parsed) {
        Ok(bytes) => {
            tracing::warn!(
                target: "headroom.proxy",
                event = "cache_control_ttl_order",
                request_id = %request_id,
                demoted = repair.demoted,
                promoted = repair.promoted,
                client_asked_for_1h,
                "repaired cache_control TTL ordering before forwarding"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body_to_send,
    }
}

/// Render indices for a log field, capped so one pathological turn cannot
/// write a thousand-entry line.
fn join_indices(indices: &[usize]) -> String {
    const MAX: usize = 20;
    let head = indices
        .iter()
        .take(MAX)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if indices.len() > MAX {
        format!("{head},…+{}", indices.len() - MAX)
    } else {
        head
    }
}

/// The provider's own words for why it refused a request, as
/// `(error type, message)`.
///
/// Reads the two error envelopes the proxy forwards to — Anthropic's
/// `{"error": {"type", "message"}}` and OpenAI's `{"error": {"code", "message"}}`
/// — and returns those fields only. The raw body never reaches the log: an
/// unrecognised shape yields empty strings rather than whatever bytes the
/// upstream happened to send, because this runs on every failed request and the
/// log is not a place to spill unknown payloads.
fn describe_upstream_error(body: &[u8]) -> (String, String) {
    const MAX_MESSAGE_CHARS: usize = 400;
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (String::from("unparsed"), String::new());
    };
    let Some(error) = value.get("error") else {
        return (String::from("no_error_field"), String::new());
    };
    let kind = error
        .get("type")
        .or_else(|| error.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let message: String = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .collect();
    (kind, message)
}

/// True if the upstream response is an SSE stream. Compares
/// `content-type` against `text/event-stream` (with optional
/// parameters). RFC 7231 §3.1.1.1: media types compare
/// case-insensitive on the type/subtype tokens.
/// First 12 hex chars of the SHA-256 of `value`. Enough to tell two prefixes
/// apart in a log without carrying their bytes.
fn short_hash(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)[..12].to_string()
}

/// Log which parts of the cacheable prefix this request carries.
///
/// A fan-out of subagents shares a provider cache entry only where their
/// leading bytes are identical. Measured 2026-08-13: of 14 subagent
/// conversations, 5 shared a 43,603-token prefix and the other 9 each read a
/// slightly different floor, so eight cache entries were built where one would
/// have done. Sizes alone cannot say which component differs, so hash `system`
/// and `tools` separately — two requests whose `tools_fingerprint` matches but
/// whose `system_fingerprint` does not are diverging in the preamble, and vice
/// versa. `tool_names_fingerprint` isolates the common case further: the same
/// tools in a different ORDER hash differently there but identically by name
/// set, which names ordering as the culprit without a capture.
fn log_prefix_composition(request_id: &str, body: &[u8]) {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    // Read the model off the body rather than the caller: the cache is keyed
    // per model, so an opus and a sonnet request with identical prefixes still
    // build separate entries, and the fingerprints only mean anything when
    // compared within one model.
    let model = parsed.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let part = |value: Option<&serde_json::Value>| -> (String, usize) {
        match value {
            Some(value) => {
                let text = value.to_string();
                (short_hash(&text), text.len())
            }
            None => ("absent".to_string(), 0),
        }
    };
    let (system_fingerprint, system_bytes) = part(parsed.get("system"));
    let (tools_fingerprint, tools_bytes) = part(parsed.get("tools"));
    let names: Vec<&str> = parsed
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    tracing::info!(
        target: "headroom.proxy",
        event = "prefix_composition",
        request_id = %request_id,
        model = %model,
        system_fingerprint = %system_fingerprint,
        system_bytes = system_bytes,
        tools_fingerprint = %tools_fingerprint,
        tools_bytes = tools_bytes,
        tool_names_fingerprint = %short_hash(&names.join(",")),
        tool_names_sorted_fingerprint = %short_hash(&sorted.join(",")),
        tool_count = names.len(),
        "cacheable prefix composition"
    );
}

/// Anthropic's cache-write TTL split, as `(5m, 1h)`.
///
/// The flat `usage.cache_creation_input_tokens` the buffered path reads is a
/// total that says nothing about which TTL was billed, and the two differ:
/// a 5-minute write costs 1.25x input, a 1-hour write 2.0x. The breakdown sits
/// in a nested object, so pricing has to read it rather than assume the TTL the
/// proxy asked for. Mirrors the streaming parser in `sse::anthropic`.
///
/// Returns `(0, 0)` for any other provider — no one else publishes the field.
fn anthropic_cache_ttl_split(usage: Option<&serde_json::Value>) -> (i64, i64) {
    let Some(cc) = usage.and_then(|u| u.get("cache_creation")) else {
        return (0, 0);
    };
    let get = |key: &str| cc.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    (
        get("ephemeral_5m_input_tokens"),
        get("ephemeral_1h_input_tokens"),
    )
}

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
    // Told when the replay is declined, so a re-cache event a turn later can
    // name the cause instead of falling through to "unattributable".
    observer: Option<&cache_stabilization::usage_observer::UsageObserver>,
    // Seconds since this process started, so an empty store right after a
    // restart is not read as an unstable session key.
    uptime_seconds: u64,
    // How many tail breakpoints to place, and whether the client's `system`
    // markers may go. Both come from flags so the pair can be measured against
    // the single-marker placement it replaces.
    tail_breakpoints: usize,
    strip_system_breakpoints: bool,
) -> bytes::Bytes {
    use cache_stabilization::prefix_replay::{
        early_message_fingerprints, overlay_cached_prefix_reported, place_tail_cache_breakpoints,
        strip_system_cache_control, tail_slots_within_budget, ANTHROPIC_CACHE_CONTROL_LIMIT,
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

    // Ask for the prefix THIS turn continues, not merely the session's last
    // one: several streams share a session key, and handing back another
    // stream's prefix guarantees the append-only guard rejects it and the turn
    // forwards fresh bytes over content the provider had cached.
    let (prev_orig, prev_fwd, prefix_miss, chain_id) =
        match store.previous_turn_for(session_key, &original_messages) {
            Ok((o, f, chain_id)) => (Some(o), Some(f), None, chain_id),
            Err(miss) => (None, None, Some(miss), 0),
        };
    let (overlaid, skip_reason) = overlay_cached_prefix_reported(
        optimized.clone(),
        &original_messages,
        prev_orig.as_deref(),
        prev_fwd.as_deref(),
        // Only splice a diverged prefix when this turn genuinely continues the
        // stored chain. A zero id means the store fell back to the session's
        // most recent prefix, which belongs to some other stream.
        chain_id != 0,
    );
    let replayed_prefix = overlaid != optimized;
    // A turn that does not replay is where the money goes: measured over the
    // 2026-08-08/09 logs, non-replaying turns were 19% of traffic and carried
    // 97% of booked re-cache waste. `replayed_prefix` alone cannot say which of
    // five reasons applied, and they need opposite responses — a diverged
    // client prefix is not our doing, while a turn shorter than the stored
    // prefix means two streams are sharing one session slot.
    if let Some(reason) = skip_reason {
        // The two heads below share one canonical comparison, so `first_diff_path`
        // and the text it names cannot disagree. Computed here rather than inline
        // in the event for that reason, and only on a decline — a replaying turn
        // never pays for it.
        let (diff_stored_text_head, diff_current_text_head) =
            match (skip_reason, prev_orig.as_deref()) {
                (
                    Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                        first_diff_index,
                        ..
                    }),
                    Some(prev),
                ) => cache_stabilization::prefix_replay::divergence_text_heads(
                    prev,
                    &original_messages,
                    first_diff_index,
                )
                .unwrap_or_default(),
                _ => (String::new(), String::new()),
            };
        if let Some(observer) = observer {
            observer.note_replay_skip(
                request_id,
                cache_stabilization::usage_observer::ReplaySkipEvidence::from_inbound_original_histories(
                    reason,
                    prev_orig.as_deref(),
                    &original_messages,
                ),
            );
        }
        tracing::info!(
            event = "prefix_replay_not_replayed",
            request_id = %request_id,
            // Correlate replay declines with the drift and volatile-content
            // events emitted for the same logical session.  The raw session
            // key can contain an authorization credential or caller-supplied
            // identifier, so this is deliberately the existing 16-hex
            // SHA-256 log prefix rather than the key itself.
            session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
            reason = reason.as_str(),
            // Only set when `reason` is `no_previous_turn`, and the part that
            // makes it actionable: a first turn is free, an idle gap is already
            // lost, a missing tracker on a live session is a defect.
            miss_detail = prefix_miss.map(|m| m.as_str()).unwrap_or(""),
            proxy_uptime_seconds = uptime_seconds,
            // Which leading message first disagreed. A conversation that
            // declines every turn while growing normally is not being edited by
            // its client — something inside it churns per request and the
            // canonicalizer is not neutralising it. This names where.
            first_diff_index = match skip_reason {
                Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                        ..
                }) => first_diff_index as i64,
                _ => -1,
            },
            // The field that churned, named by structure alone — keys and
            // indices, never a value. One sample of `content[0].text` on the
            // opener says "an injected block changes per request, and we can
            // neutralise it"; `content[3].input` says a real edit. Without it
            // the index alone needs a distribution to say anything.
            first_diff_path = match (skip_reason, prev_orig.as_deref()) {
                (
                    Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                        first_diff_index,
                        ..
                    }),
                    Some(prev),
                ) => cache_stabilization::prefix_replay::describe_divergence(
                    prev,
                    &original_messages,
                    first_diff_index,
                )
                .unwrap_or_default(),
                _ => String::new(),
            },
            // How much of the stored prefix was replayed anyway. A decline no
            // longer forwards this turn's own bytes for the whole prefix: the
            // run that still agrees comes from the stored copy, so the provider
            // keeps reading it instead of missing at message 0. Zero here means
            // the divergence was at the very first message and nothing could be
            // salvaged.
            replayed_prefix_msgs = match skip_reason {
                Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                    replayed_prefix_msgs,
                    ..
                }) => replayed_prefix_msgs as i64,
                _ => -1,
            },
            // Which block kinds sat on each side of that difference. A
            // `tool_result` that vanished points at something collapsing tool
            // output in front of the client; an ordinary text change points at
            // a real edit. Type names only, never block contents.
            diff_shape_stored = match (skip_reason, prev_orig.as_deref()) {
                (
                    Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                        first_diff_index,
                        ..
                    }),
                    Some(prev),
                ) => prev
                    .get(first_diff_index)
                    .map(cache_stabilization::prefix_replay::block_type_shape)
                    .unwrap_or_default(),
                _ => String::new(),
            },
            diff_shape_current = match skip_reason {
                Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                        ..
                }) => original_messages
                    .get(first_diff_index)
                    .map(cache_stabilization::prefix_replay::block_type_shape)
                    .unwrap_or_default(),
                _ => String::new(),
            },
            // What the text blocks on each side WERE. The shapes above say a
            // `text` block came or went; these say whether it was the client's
            // own ephemeral scaffolding or real content, which is the
            // difference between churn we could neutralise and an edit we must
            // respect. Closed vocabulary, never the text.
            diff_text_kinds_stored = match (skip_reason, prev_orig.as_deref()) {
                (
                    Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                        first_diff_index,
                        ..
                    }),
                    Some(prev),
                ) => prev
                    .get(first_diff_index)
                    .map(cache_stabilization::prefix_replay::text_block_kinds)
                    .unwrap_or_default(),
                _ => String::new(),
            },
            diff_text_kinds_current = match skip_reason {
                Some(cache_stabilization::prefix_replay::ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                        ..
                }) => original_messages
                    .get(first_diff_index)
                    .map(cache_stabilization::prefix_replay::text_block_kinds)
                    .unwrap_or_default(),
                _ => String::new(),
            },
            // What the differing text actually SAYS, on each side. The path
            // above names where a mismatch is and the kinds name what sort of
            // block held it, but neither shows the characters, so the
            // 2026-08-13 investigation had to infer trailing whitespace from
            // message shapes when a hundred characters of each string would
            // have shown it outright. The one deliberate exception to logging
            // values: head only, escaped, canonical form, and only for the
            // message the path already names.
            diff_stored_text_head = %diff_stored_text_head,
            diff_current_text_head = %diff_current_text_head,
            stored_prefix_msgs = prev_orig.as_deref().map(|o| o.len()).unwrap_or(0),
            current_original_msgs = original_messages.len(),
            optimized_msgs = optimized.len(),
            // Which run of turns this one continues, `0` for none. Grouping by
            // conversation key or by message count cannot separate a branch, a
            // compaction and a second stream — three wrong conclusions came
            // from that on 2026-08-09. This can.
            chain_id = chain_id,
            "prefix replay declined: forwarding this turn's own bytes"
        );
    }
    // Anthropic counts `cache_control` across `system`, `tools` and `messages`
    // together and refuses the whole request past 4. The message slots are the
    // only ones this proxy can give up, so they yield to whatever the client set
    // on `system` and PR-E3 set on `tools`. Counted before any system stripping
    // below, which errs low — a slot freed there goes unused rather than risking
    // the sum.
    let (allowed_slots, reserved_slots) = tail_slots_within_budget(&parsed, tail_breakpoints);
    if allowed_slots < tail_breakpoints {
        tracing::warn!(
            event = "cache_marker_budget_clamped",
            request_id = %request_id,
            requested = tail_breakpoints,
            allowed = allowed_slots,
            reserved_by_system_and_tools = reserved_slots,
            limit = ANTHROPIC_CACHE_CONTROL_LIMIT,
            "cache_control budget: placing fewer message breakpoints than asked \
             to keep the request under the provider's limit"
        );
    }
    let (normalized, breakpoints_placed) = place_tail_cache_breakpoints(overlaid, allowed_slots);
    // Anthropic refuses a turn whose signed `thinking` blocks changed, naming a
    // message index but not who changed it. Both the client and this proxy
    // rewrite history, so a rejection is unattributable without knowing which
    // messages WE altered. Report that, and single out the altered ones that
    // carry a thinking block — if a rejection's index appears here, the proxy
    // caused it. Indices and counts only.
    let rewritten = rewritten_message_report(&original_messages, &normalized);
    if !rewritten.indices.is_empty() || !rewritten.thinking_touched.is_empty() {
        tracing::info!(
            event = "messages_rewritten",
            request_id = %request_id,
            rewritten_count = rewritten.indices.len(),
            rewritten_indices = %join_indices(&rewritten.indices),
            // The ones that can be refused. Empty here means a thinking-block
            // rejection came from the client's own edits, not ours.
            rewritten_with_thinking_count = rewritten.with_thinking.len(),
            rewritten_with_thinking_indices = %join_indices(&rewritten.with_thinking),
            // Signed blocks altered on the wire, `cache_control` included. The
            // provider refuses these outright, so a non-empty list is a defect
            // regardless of how much it saves.
            thinking_touched_count = rewritten.thinking_touched.len(),
            thinking_touched_indices = %join_indices(&rewritten.thinking_touched),
            total_messages = original_messages.len(),
            // The bytes of the earliest messages exactly as forwarded, so two
            // consecutive turns can be diffed to name the first one that moved.
            // The drift detector cannot answer this: it filters ephemeral blocks
            // before comparing and the provider does not, so it calls a prefix
            // stable while the provider re-creates it.
            early_fingerprints = %early_message_fingerprints(&normalized, 5),
            "messages this proxy altered before forwarding"
        );
    }
    // Only once a message breakpoint is in place. With none placed the client's
    // system markers are the only ones on the request, and dropping them would
    // turn caching off rather than move it.
    let system_markers_dropped = if strip_system_breakpoints && breakpoints_placed > 0 {
        strip_system_cache_control(&mut parsed)
    } else {
        0
    };
    let changed = normalized != optimized || system_markers_dropped > 0;

    let (final_body, forwarded_messages) = if changed {
        parsed["messages"] = serde_json::Value::Array(normalized.clone());
        match serde_json::to_vec(&parsed) {
            Ok(b) => {
                if replayed_prefix {
                    if let Some(observer) = observer {
                        observer.note_replay_applied(
                            request_id,
                            cache_stabilization::usage_observer::ReplayAppliedEvidence::new(
                                chain_id,
                                breakpoints_placed,
                                system_markers_dropped,
                            ),
                        );
                    }
                }
                tracing::info!(
                    event = "prefix_replay_applied",
                    request_id = %request_id,
                    replayed_prefix = replayed_prefix,
                    chain_id = chain_id,
                    // What went out on the wire, so a run can be attributed to
                    // its placement rather than to the flag it was started with.
                    breakpoints_placed = breakpoints_placed,
                    system_markers_dropped = system_markers_dropped,
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

    // A side errand shares this conversation's session key but is not a step in
    // it. Parking it would make it the session's "previous turn", and the next
    // real turn would diverge at the final message and recache from there.
    if cache_stabilization::prefix_replay::is_side_errand(&original_messages) {
        tracing::info!(
            event = "prefix_replay_side_errand_not_parked",
            request_id = %request_id,
            messages = original_messages.len(),
            "prefix replay: side errand left out of the store"
        );
        return final_body;
    }

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
pub(crate) fn maybe_inject_openai_prompt_cache_key(
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
    /// True only on the request that inserted the one-time expansion tail.
    /// Its provider cache creation usage is a separate cost signal from the
    /// raw bytes injected on the request path.
    proactive_expansion_applied: bool,
    /// Whole-body bytes received from the client and put on the wire. Carried
    /// here because the sizes are only knowable at the send point while the
    /// provider's usage only arrives at stream close, and the pair is worth
    /// nothing apart: bytes alone cannot say what the provider billed.
    wire_bytes: Option<(i64, i64)>,
    /// Request-side estimate used only when an error response omits provider
    /// usage. It remains separate from `provider_*_tokens` in the failed-work
    /// bucket so it cannot be mistaken for actual billing.
    forwarded_tokens_estimate: i64,
    /// Number of upstream transmissions made for this client turn.
    upstream_attempts: i64,
}

/// Book this request's wire bytes against the usage the provider reported for
/// it.
///
/// Both halves have to come from the same request or the ratio is meaningless,
/// which is why this takes the byte pair off the context rather than from any
/// running total. A request whose bytes were never measured (passthrough, or a
/// stream that ended before usage arrived) is skipped entirely: booking bytes
/// with no tokens, or tokens with no bytes, would quietly bias the ratio in
/// whichever direction the missing half went.
fn record_wire_footprint(
    ctx: &OutcomeContext,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
) {
    let Some((bytes_in, bytes_out)) = ctx.wire_bytes else {
        return;
    };
    if input_tokens + cache_read_tokens + cache_write_tokens <= 0 {
        return; // no usage reported; nothing to reconcile against
    }
    ctx.sink.savings_tracker.record_wire_footprint(
        bytes_in,
        bytes_out,
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
    );
}

fn observe_proactive_expansion_cache_write(ctx: &OutcomeContext, write_tokens: u64) {
    if ctx.proactive_expansion_applied {
        crate::observability::ctx_metrics::observe_proactive_expansion_cache_write_tokens(
            write_tokens,
        );
    }
}

/// Book a terminal non-SSE upstream rejection into the failure-only bucket. The ordinary
/// success body path builds the same fields later, but upstream rejections take
/// the small buffered-error branch and used to bypass `RequestOutcome`
/// entirely. A usage block is accepted when present; absent usage stays
/// `None`, distinct from the request-side forwarded-token estimate.
fn emit_failed_http_outcome(
    ctx: &OutcomeContext,
    request_id: &str,
    status: StatusCode,
    body: Option<&bytes::Bytes>,
) {
    let parsed = body.and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok());
    let usage = parsed.as_ref().and_then(|value| value.get("usage"));
    let get = |key: &str| -> i64 {
        usage
            .and_then(|value| value.get(key))
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
    };
    let (provider_input, output_tokens, cache_read, cache_write) = match ctx.provider.as_str() {
        "anthropic" => (
            get("input_tokens")
                .saturating_add(get("cache_read_input_tokens"))
                .saturating_add(get("cache_creation_input_tokens")),
            get("output_tokens"),
            get("cache_read_input_tokens"),
            get("cache_creation_input_tokens"),
        ),
        "openai_responses" => (
            get("input_tokens"),
            get("output_tokens"),
            usage
                .and_then(|value| value.get("input_tokens_details"))
                .and_then(|value| value.get("cached_tokens"))
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            0,
        ),
        _ => (
            get("prompt_tokens"),
            get("completion_tokens"),
            usage
                .and_then(|value| value.get("prompt_tokens_details"))
                .and_then(|value| value.get("cached_tokens"))
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            0,
        ),
    };
    let provider_input_for_sizes = if ctx.provider == "anthropic" {
        get("input_tokens")
    } else {
        provider_input
    };
    let (original_tokens, optimized_tokens) = ctx.sizes(provider_input_for_sizes);
    let outcome = headroom_core::request_outcome::RequestOutcome {
        request_id: request_id.to_string(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        status_code: i64::from(status.as_u16()),
        upstream_attempts: ctx.upstream_attempts,
        provider_input_tokens: usage.map(|_| provider_input),
        provider_output_tokens: usage.map(|_| output_tokens),
        original_tokens,
        optimized_tokens,
        output_tokens,
        tokens_saved: ctx.tokens_saved,
        attempted_input_tokens: ctx.attempted(provider_input_for_sizes),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        uncached_input_tokens: if ctx.provider == "anthropic" {
            get("input_tokens")
        } else {
            provider_input.saturating_sub(cache_read)
        },
        total_latency_ms: ctx.started_at.elapsed().as_secs_f64() * 1000.0,
        overhead_ms: ctx.overhead_ms,
        transforms_applied: ctx.transforms_applied.clone(),
        waste_signals: ctx.waste_signals.clone(),
        num_messages: ctx.num_messages,
        tags: ctx.tags.clone(),
        client: ctx.client.clone(),
        project: ctx.project.clone(),
        ..Default::default()
    };
    headroom_core::request_outcome::emit_failed_request_outcome(ctx.sink.as_ref(), &outcome);
}

impl OutcomeContext {
    /// Resolve `(original_tokens, optimized_tokens)` for this request.
    ///
    /// `original_tokens` is only populated when the compression pipeline ran —
    /// it comes from `Outcome::Compressed { tokens_before }`. A transform that
    /// shrinks the body outside that pipeline (ctx_offload) therefore produced
    /// a real `tokens_saved` against a zero baseline, which reads as a 0%
    /// saving and contributes nothing to the savings tracker.
    ///
    /// Fall back to the provider's own input count: that is, by definition, the
    /// size we forwarded, so the pre-transform size is it plus what we removed.
    fn sizes(&self, attempted_input_tokens: i64) -> (i64, i64) {
        if self.original_tokens > 0 {
            return (
                self.original_tokens,
                self.original_tokens.saturating_sub(self.tokens_saved),
            );
        }
        let forwarded = if attempted_input_tokens > 0 {
            attempted_input_tokens
        } else {
            self.forwarded_tokens_estimate.max(0)
        };
        (forwarded + self.tokens_saved.max(0), forwarded)
    }

    /// The denominator `RequestOutcome::attempted_input_tokens` is documented
    /// to carry: the size of the material compression was asked to work on.
    ///
    /// Every outcome site used to fill that field from the provider's
    /// `usage.input_tokens` instead. On Anthropic that number excludes cache
    /// reads and writes, so on a warm session it collapses to the uncached
    /// remainder — a live session reported 8,059 against 3.66M of actual
    /// compressible input, and the two fields `attempted_input_tokens` and
    /// `uncached_input_tokens` held byte-identical values, which is the tell.
    ///
    /// The compressible portion is exactly what [`Self::sizes`] already
    /// resolves as `original_tokens`, so read it from there. Note this is NOT
    /// the whole prompt: the frozen cached prefix is not compression's to
    /// touch, and folding it in would make the denominator a sum of the same
    /// prefix re-read every turn.
    fn attempted(&self, provider_input_tokens: i64) -> i64 {
        self.sizes(provider_input_tokens).0
    }
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

#[derive(Default)]
struct ParserTelemetry {
    sent_chunks: std::sync::atomic::AtomicU64,
    dropped_chunks: std::sync::atomic::AtomicU64,
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
    // Usage of CCR continuation rounds the client never saw, filled in by
    // `sse::ccr_stream` before this task's channel closes. `None` when the
    // rewriter did not run.
    ccr_round_usage: Option<Arc<Mutex<CcrRoundUsage>>>,
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
            // A stream can end with its last event unterminated: the framer
            // only yields a block once it sees the blank line, so a
            // `message_stop` that straddles the final two chunks sits in the
            // buffer and the turn reads as unfinished. Supplying the
            // terminator here costs nothing when the provider sent one.
            if framer.buffered_len() > 0 {
                let stranded = framer.buffered_len();
                framer.push(b"\n\n");
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
                tracing::warn!(
                    request_id = %request_id,
                    event = "sse_tail_flushed",
                    stranded_bytes = stranded,
                    "flushed an unterminated trailing event at end of stream"
                );
            }
            // Snapshot hidden continuation usage before any cache observer
            // runs. The final streamed usage belongs to the proxy's private
            // continuation request; the first discarded response below is the
            // cache footprint of the request the client actually sent.
            let ccr_rounds = ccr_round_usage
                .as_ref()
                .and_then(|u| u.lock().ok().map(|g| *g))
                .unwrap_or_default();
            let (cache_baseline_input, cache_baseline_read, cache_baseline_write) = ccr_rounds
                .client_cache_baseline(
                    state.usage.input_tokens,
                    state.usage.cache_read_input_tokens,
                    state.usage.cache_creation_input_tokens,
                );
            // The baseline above is the right thing to classify against and the
            // wrong thing to bill: it drops the continuation rounds, which the
            // provider charged for and which `savings_pricing_counterfactual`
            // already counts off the outcome. Hand the observer the full total
            // so the two events agree on one request's billed usage.
            if ccr_rounds.rounds > 0 {
                usage_observer.note_billed_totals(
                    &request_id,
                    state
                        .usage
                        .input_tokens
                        .saturating_add(ccr_rounds.input_tokens.max(0) as u64),
                    state
                        .usage
                        .cache_read_input_tokens
                        .saturating_add(ccr_rounds.cache_read_tokens.max(0) as u64),
                    state
                        .usage
                        .cache_creation_input_tokens
                        .saturating_add(ccr_rounds.cache_write_tokens.max(0) as u64),
                );
            }

            // Phase G PR-G3 + H2: emit per-session cache-hit-rate
            // ONLY when the stream completed cleanly with
            // `message_stop`. The gate is encapsulated by the
            // pure function `compute_anthropic_session_hit_rate`
            // so the H2 contract has a unit-testable surface.
            let cache_hit_rate = if state.status == crate::sse::anthropic::StreamStatus::MessageStop
            {
                crate::observability::cache_hit_rate::compute_hit_rate(
                    cache_baseline_input,
                    cache_baseline_read,
                    cache_baseline_write,
                )
            } else {
                None
            };
            match cache_hit_rate {
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
                        input_tokens = cache_baseline_input,
                        cache_read_input_tokens = cache_baseline_read,
                        cache_creation_input_tokens = cache_baseline_write,
                        "skipping proxy_cache_hit_rate_per_session: H2 gate or zero denominator"
                    );
                }
            }
            // CTX-7: feed the re-cache watchdog with this turn's
            // billed usage. Same H2 gate as the hit-rate metric: only
            // a cleanly completed stream (`message_stop`) carries
            // trustworthy final usage.
            if state.status == crate::sse::anthropic::StreamStatus::MessageStop {
                if let Some(ctx) = outcome_ctx.as_ref() {
                    observe_proactive_expansion_cache_write(ctx, cache_baseline_write);
                }
                let class = usage_observer.complete(
                    &request_id,
                    cache_baseline_input,
                    cache_baseline_read,
                    cache_baseline_write,
                    // Read off the streamed usage rather than the CCR baseline
                    // beside it: continuation rounds carry no TTL breakdown, so
                    // the split stays a subset of the write total, exactly as
                    // the buffered path treats it.
                    Some((
                        state.usage.cache_creation_5m_input_tokens,
                        state.usage.cache_creation_1h_input_tokens,
                    )),
                );
                // The observer's counters reset on restart, so persist the
                // classification here where the savings tracker is reachable.
                // Without this there is no way to answer "is the proxy paying
                // for itself" across sessions.
                if let (Some(class), Some(ctx)) = (class, outcome_ctx.as_ref()) {
                    use headroom_core::request_outcome::OutcomeSink as _;
                    let (reason, wasted) = class.as_record();
                    ctx.sink.record_cache_outcome("anthropic", reason, wasted);
                }
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
                    store.complete(&request_id, cache_baseline_read, cache_baseline_write);
                }
            }
            tracing::info!(
                request_id = %request_id,
                provider = "anthropic",
                input_tokens = state.usage.input_tokens,
                output_tokens = state.usage.output_tokens,
                cache_creation_input_tokens = state.usage.cache_creation_input_tokens,
                cache_read_input_tokens = state.usage.cache_read_input_tokens,
                cleared_input_tokens = state.cleared_input_tokens,
                stop_reason = state.stop_reason.as_deref().unwrap_or(""),
                blocks = state.blocks.len(),
                "sse stream closed"
            );
            // A turn the client will refuse whole: it was told a tool call
            // was coming and got nothing it can run. Warn here or the only
            // symptom is "tool call could not be parsed" on the far side,
            // with a clean `sse stream closed` on this one.
            if let Some(defect) = state.tool_call_defect() {
                tracing::warn!(
                    event = "tool_call_defect",
                    request_id = %request_id,
                    kind = defect.kind(),
                    stop_reason = state.stop_reason.as_deref().unwrap_or(""),
                    output_tokens = state.usage.output_tokens,
                    detail = %defect,
                    "upstream declared a tool call the client cannot execute"
                );
            }
            // Same H2 gate the three consumers above use, and for the same
            // reason. Anthropic reports the turn's final `output_tokens` in
            // the `message_delta` that precedes `message_stop`; a stream cut
            // short by a client disconnect carries whatever partial count had
            // arrived by then. Booking that as final under-reported output —
            // silently, because a truncated turn is indistinguishable from a
            // cheap one once it is in the ledger. Dropping the turn also
            // under-reports, but visibly: the counter says how many turns the
            // books are missing, and the log below keeps the partial numbers.
            // Anthropic reports the turn's final `output_tokens` in the
            // `message_delta` that precedes `message_stop`, and only that
            // delta carries a `stop_reason`. So a stop_reason in hand means
            // the usage is the final figure and booking it is exact, not the
            // partial count the comment above warns about — the terminator
            // that follows carries no usage of its own. Without a stop_reason
            // the totals really are mid-flight, and the turn is still dropped.
            let usage_is_final = state.stop_reason.is_some();
            let stream_completed =
                state.status == crate::sse::anthropic::StreamStatus::MessageStop || usage_is_final;
            if !stream_completed {
                crate::observability::record_stream_incomplete("anthropic");
                // Also booked into the persisted savings state, so the lifetime
                // verdict can report how many turns it is missing. The
                // Prometheus counter above resets with the process; the books
                // do not.
                if let Some(ref ctx) = outcome_ctx {
                    ctx.sink.savings_tracker.record_unbooked_turn();
                }
                tracing::warn!(
                    request_id = %request_id,
                    event = "stream_incomplete",
                    provider = "anthropic",
                    status = ?state.status,
                    partial_input_tokens = state.usage.input_tokens,
                    partial_output_tokens = state.usage.output_tokens,
                    "stream ended without message_stop; usage is partial, \
                     so this turn is not booked into cost or savings"
                );
            } else if state.status != crate::sse::anthropic::StreamStatus::MessageStop {
                // Booked on a final stop_reason, but the stream still ended
                // without its terminator. Say so: the turn's numbers are
                // right, and the missing tail is a fault worth seeing.
                tracing::warn!(
                    request_id = %request_id,
                    event = "stream_booked_without_message_stop",
                    provider = "anthropic",
                    status = ?state.status,
                    stop_reason = state.stop_reason.as_deref().unwrap_or(""),
                    output_tokens = state.usage.output_tokens,
                    "stream ended without message_stop but carried a final \
                     stop_reason; usage is final, so the turn is booked"
                );
            }
            // Fold in the CCR continuation rounds, exactly as the buffered
            // path does. The client saw one turn; the upstream billed several,
            // and only the last one's usage reached the stream. Without this
            // the savings figures are computed against a fraction of what the
            // turn cost. Reading it here is safe: the rewriter fills it in
            // before it sends the final events, and this runs after the
            // channel those events travelled on has closed.
            if !ccr_rounds.is_empty() {
                tracing::info!(
                    request_id = %request_id,
                    event = "ccr_continuation_usage",
                    rounds = ccr_rounds.rounds,
                    input_tokens = ccr_rounds.input_tokens,
                    output_tokens = ccr_rounds.output_tokens,
                    cache_write_tokens = ccr_rounds.cache_write_tokens,
                    client_cache_read_tokens = cache_baseline_read,
                    client_cache_write_tokens = cache_baseline_write,
                    "billed CCR continuation rounds the client never saw"
                );
            }
            let attempted_input = state.usage.input_tokens as i64 + ccr_rounds.input_tokens;
            if let (Some(ref ctx), true) = (&outcome_ctx, stream_completed) {
                let outcome = headroom_core::request_outcome::RequestOutcome {
                    request_id: request_id.clone(),
                    provider: ctx.provider.clone(),
                    model: ctx.model.clone(),
                    original_tokens: ctx.sizes(attempted_input).0,
                    optimized_tokens: ctx.sizes(attempted_input).1,
                    output_tokens: state.usage.output_tokens as i64 + ccr_rounds.output_tokens,
                    tokens_saved: ctx.tokens_saved,
                    attempted_input_tokens: ctx.attempted(attempted_input),
                    cache_read_tokens: state.usage.cache_read_input_tokens as i64
                        + ccr_rounds.cache_read_tokens,
                    cache_write_tokens: state.usage.cache_creation_input_tokens as i64
                        + ccr_rounds.cache_write_tokens,
                    cache_write_5m_tokens: state.usage.cache_creation_5m_input_tokens as i64,
                    cache_write_1h_tokens: state.usage.cache_creation_1h_input_tokens as i64,
                    // Anthropic's `input_tokens` already excludes cache reads
                    // and writes, so it *is* the uncached count. Python's
                    // Bedrock path has to subtract instead, because there
                    // `input_tokens` is the total — do not copy that formula
                    // here.
                    uncached_input_tokens: attempted_input,
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
                record_wire_footprint(
                    ctx,
                    outcome.uncached_input_tokens,
                    outcome.cache_read_tokens,
                    outcome.cache_write_tokens,
                );
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
                    original_tokens: ctx.sizes(input_tok).0,
                    optimized_tokens: ctx.sizes(input_tok).1,
                    output_tokens: output_tok,
                    tokens_saved: ctx.tokens_saved,
                    attempted_input_tokens: ctx.attempted(input_tok),
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
                    original_tokens: ctx.sizes(input_tok).0,
                    optimized_tokens: ctx.sizes(input_tok).1,
                    output_tokens: output_tok,
                    tokens_saved: ctx.tokens_saved,
                    attempted_input_tokens: ctx.attempted(input_tok),
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

/// Read `(input, output, cache_read, cache_write)` out of one upstream
/// response's `usage` block.
///
/// `provider` carries the same labels as `OutcomeContext::provider`
/// (`"anthropic"` / `"openai_responses"` / anything else = OpenAI chat), and
/// the three arms below mirror the outcome block's parsing exactly. They have
/// to: the total this feeds is added to a number that block read, and settled
/// against one it will read, so a different reading here would stop cancelling.
fn response_usage(response: &serde_json::Value, provider: &str) -> (i64, i64, i64, i64) {
    let usage = response.get("usage");
    let get = |key: &str| -> i64 {
        usage
            .and_then(|u| u.get(key))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    let cached = |details: &str| -> i64 {
        usage
            .and_then(|u| u.get(details))
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    match provider {
        "anthropic" => (
            get("input_tokens"),
            get("output_tokens"),
            get("cache_read_input_tokens"),
            get("cache_creation_input_tokens"),
        ),
        "openai_responses" => (
            get("input_tokens"),
            get("output_tokens"),
            cached("input_tokens_details"),
            0,
        ),
        _ => (
            get("prompt_tokens"),
            get("completion_tokens"),
            cached("prompt_tokens_details"),
            0,
        ),
    }
}

/// Upstream calls a turn hook made that nothing else accounts for.
///
/// A hook that re-drives the model through `call_model` makes real, billed
/// requests. The outcome block reads exactly one response — whichever the hook
/// handed back — so every other upstream call on that turn is spend no surface
/// records. A tool-search reload is a whole extra model call; count only the
/// last one and the feature hides its own overhead behind the saving it claims.
///
/// The Python original matched the response the usage block would read by
/// object identity and dropped that one entry. Here [`record`](Self::record)
/// takes the running total of every real upstream response and
/// [`settle`](Self::settle) subtracts whatever the outcome block is about to
/// read, so the two always sum back to what the upstream actually billed —
/// including when a hook returns a response it synthesised rather than one it
/// was given, which upstream over-counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TurnHookUsage {
    /// Upstream calls beyond the one the outcome block reads. Zero on the
    /// common path, and zero unless a hook re-drove the model.
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
}

impl TurnHookUsage {
    /// Note one real upstream response, the original included.
    fn record(&mut self, response: &serde_json::Value, provider: &str) {
        let (input, output, cache_read, cache_write) = response_usage(response, provider);
        self.calls += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.cache_read_tokens += cache_read;
        self.cache_write_tokens += cache_write;
    }

    /// Drop the contribution of the response the outcome block will read. What
    /// is left is the delta that block has to add.
    ///
    /// A hook is free to return a response carrying larger figures than
    /// anything upstream sent, so each component floors at zero: over-counting
    /// a bill beats under-counting it.
    fn settle(&mut self, read: &serde_json::Value, provider: &str) {
        let (input, output, cache_read, cache_write) = response_usage(read, provider);
        self.calls = (self.calls - 1).max(0);
        self.input_tokens = (self.input_tokens - input).max(0);
        self.output_tokens = (self.output_tokens - output).max(0);
        self.cache_read_tokens = (self.cache_read_tokens - cache_read).max(0);
        self.cache_write_tokens = (self.cache_write_tokens - cache_write).max(0);
    }

    /// True when there is nothing extra to account for.
    fn is_empty(&self) -> bool {
        *self == Self::default()
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
    /// Usage of every re-drive made through this handle. Shared with
    /// `apply_response_hooks`, and behind a lock because `CallModel::call`
    /// only has `&self`.
    usage: Arc<std::sync::Mutex<TurnHookUsage>>,
    /// Provider label for reading those responses' `usage` blocks.
    usage_provider: String,
}

impl ProxyCallModel {
    /// Note one re-drive's usage. Only the calls that came back with a body
    /// count: a request that never left, or died on the wire, was not billed.
    fn record(&self, response: &serde_json::Value) {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(response, &self.usage_provider);
    }
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
                Ok(bytes) => {
                    let parsed: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                    self.record(&parsed);
                    parsed
                }
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
/// Returns the (possibly replaced) body bytes, unchanged on parse failure,
/// along with the usage of any upstream call the caller's outcome block will
/// not see. Callers MUST gate on a non-empty registry (byte-identical no-op).
///
/// `provider` is the hook-facing label (`"anthropic"` / `"openai"`);
/// `usage_provider` is the finer one the outcome block parses `usage` by.
async fn apply_response_hooks(
    body_bytes: bytes::Bytes,
    original_request: &bytes::Bytes,
    provider: &str,
    usage_provider: &str,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    headers: &http::HeaderMap,
    request_id: &str,
) -> (bytes::Bytes, TurnHookUsage) {
    let response: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => return (body_bytes, TurnHookUsage::default()),
    };
    let template: serde_json::Value = match serde_json::from_slice(original_request) {
        Ok(v) => v,
        Err(_) => return (body_bytes, TurnHookUsage::default()),
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
    // The call we already made counts too: if a hook replaces the response,
    // this original is the one nobody else will read.
    let usage = Arc::new(std::sync::Mutex::new(TurnHookUsage::default()));
    usage
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(&response, usage_provider);

    let call_model = ProxyCallModel {
        template,
        upstream_url: upstream_url.clone(),
        client: client.clone(),
        headers: headers.clone(),
        request_id: request_id.to_string(),
        usage: Arc::clone(&usage),
        usage_provider: usage_provider.to_string(),
    };
    let out = crate::turn_hooks::run_response_hooks(&ctx, response, &call_model).await;
    let mut hook_usage = *usage.lock().unwrap_or_else(|e| e.into_inner());

    // Settle against the body the caller will actually go on to read, which on
    // a serialize failure is still the original response.
    let (final_bytes, final_response) = match serde_json::to_vec(&out) {
        Ok(v) => (bytes::Bytes::from(v), out),
        Err(_) => {
            let original = serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
            (body_bytes, original)
        }
    };
    hook_usage.settle(&final_response, usage_provider);
    (final_bytes, hook_usage)
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

pub(crate) async fn handle_ccr_response(
    body_bytes: &bytes::Bytes,
    original_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    ccr_store: &dyn headroom_core::ccr::CcrStore,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
) -> (bytes::Bytes, CcrRoundUsage) {
    // Usage from every response this function replaces. The caller parses the
    // usage of the body we return, so accounting for that one here too would
    // double-count it.
    let mut round_usage = CcrRoundUsage::default();
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
            return (body_bytes.clone(), round_usage);
        }
    };

    if !handler.has_ccr_tool_calls(&response, provider) {
        return (body_bytes.clone(), round_usage);
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
            return (body_bytes.clone(), round_usage);
        }
    };

    // Which offloaded Reads the conversation has already invalidated. Computed
    // once from the request as it arrived: continuation rounds only append tool
    // results, so no later round can make a Read stale that was not stale here.
    let stale_reads = current_request
        .get(items_field)
        .and_then(|m| m.as_array())
        .map(|messages| crate::compression::ctx_offload::stale_offloaded_reads(messages))
        .unwrap_or_default();

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

        // Fetch original content for each CCR call.
        let mut results: Vec<CcrToolResult> = Vec::new();
        for call in &ccr_calls {
            let fetched = ccr_store.get(&call.hash_key);
            // Count the tool-driven retrieval here, at the only place both
            // outcomes are known. `/ctx/get` counts the HTTP surface, which
            // nothing on the model path uses, so counting only there left
            // `retrieval_hits` at zero however much the model retrieved.
            crate::observability::ctx_metrics::observe_retrieval(fetched.is_some());
            match fetched {
                Some(content) => {
                    // What comes back is the file as it was when it was read. If
                    // it has been edited since, say so — otherwise the model
                    // takes pre-edit content for the current file and acts on it.
                    let content = match stale_reads.get(&call.hash_key) {
                        Some(path) => {
                            tracing::info!(
                                request_id = %request_id,
                                hash = %call.hash_key,
                                "ccr: retrieved a Read that has since gone stale"
                            );
                            format!(
                                "{}\n\n{content}",
                                crate::compression::ctx_offload::stale_read_warning(path)
                            )
                        }
                        None => content,
                    };
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

        // Mixed CCR + real tool calls. The continuation cannot run: appending
        // the assistant message would leave the client's tool_use unanswered
        // upstream. Skipping used to drop the retrieval with it, so the model
        // asked for content and got nothing — silently on the streamed path,
        // and as a tool_use for a tool it never declared on the buffered one.
        // Answer it in place and let the real tool call go back to the client.
        if !other_calls.is_empty() {
            let spliced =
                handler.splice_ccr_results_as_text(&mut current_response, &results, provider);
            tracing::info!(
                request_id = %request_id,
                ccr_count = ccr_calls.len(),
                other_count = other_calls.len(),
                spliced = spliced,
                "ccr: mixed CCR and real tool calls; answered the retrieval in place"
            );
            crate::observability::ccr_retrieval::observe_outcome(
                crate::observability::ccr_retrieval::OUTCOME_SPLICED_MIXED,
                spliced as u64,
            );
            break;
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

        // A continuation that fails is not a soft outcome: the retrieval is
        // already parsed and the content already fetched, and giving up here
        // leaves the model with an unanswered `headroom_retrieve`. Overload
        // and transport blips are exactly what the retry is for; a 4xx is the
        // request itself being wrong, so retrying it would only burn quota.
        let mut attempt = 0;
        let resp = loop {
            let body = continuation_body.clone();
            let outcome = client
                .post(upstream_url.clone())
                .headers(outgoing_headers.clone())
                .body(body)
                .send()
                .await;
            let retryable = match &outcome {
                Err(_) => true,
                Ok(r) => {
                    let s = r.status();
                    s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS
                }
            };
            if !retryable || attempt >= CCR_CONTINUATION_RETRIES {
                break outcome;
            }
            attempt += 1;
            let backoff = std::time::Duration::from_millis(250 << (attempt - 1));
            match &outcome {
                Err(e) => tracing::warn!(
                    request_id = %request_id,
                    error = %e,
                    attempt = attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "ccr: continuation transport error; retrying"
                ),
                Ok(r) => tracing::warn!(
                    request_id = %request_id,
                    status = %r.status(),
                    attempt = attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "ccr: continuation rejected upstream; retrying"
                ),
            }
            crate::observability::ccr_retrieval::observe_continuation_retry();
            tokio::time::sleep(backoff).await;
        };

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    attempts = attempt + 1,
                    error = %e,
                    "ccr: upstream request failed during continuation"
                );
                break;
            }
        };

        if !resp.status().is_success() {
            tracing::warn!(
                request_id = %request_id,
                attempts = attempt + 1,
                status = %resp.status(),
                "ccr: upstream returned error during continuation"
            );
            break;
        }

        match resp.bytes().await {
            Ok(bytes) => {
                // The response about to be dropped was still billed.
                round_usage.add_response(&current_response);
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
    // `headroom_retrieve` remains; a retrieve left standing is a real failure.
    match handler.residual_ccr_status(&current_response, provider) {
        headroom_core::ccr::response_handler::RESIDUAL_CCR_RESOLVED => {
            tracing::info!(request_id = %request_id, "ccr: retrieval handled successfully");
            crate::observability::ccr_retrieval::observe_outcome(
                crate::observability::ccr_retrieval::OUTCOME_CONTINUATION,
                1,
            );
        }
        status => {
            // Whatever is left cannot be answered: out of rounds, upstream
            // refused every attempt, or a provider shape this path cannot
            // splice. Do not hand it back as a `tool_use` — the client never
            // declared `headroom_retrieve` and cannot resolve it, so the turn
            // ends on a call nothing will ever answer. Say so in the turn
            // instead, which is what the streamed path already does.
            let (residual, _) = handler.parse_ccr_tool_calls(&current_response, provider);
            let notes: Vec<_> = residual
                .iter()
                .map(|call| headroom_core::ccr::response_handler::CcrToolResult {
                    tool_call_id: call.tool_call_id.clone(),
                    content: "The proxy could not complete a context retrieval for this turn."
                        .to_string(),
                    success: false,
                    items_retrieved: 0,
                })
                .collect();
            let spliced =
                handler.splice_ccr_results_as_text(&mut current_response, &notes, provider);
            tracing::warn!(
                request_id = %request_id,
                status = %status,
                residual = residual.len(),
                spliced = spliced,
                "ccr: headroom_retrieve remains unresolved with no client tool call"
            );
            crate::observability::ccr_retrieval::observe_outcome(
                crate::observability::ccr_retrieval::OUTCOME_UNRESOLVED,
                residual.len().max(1) as u64,
            );
        }
    }

    match serde_json::to_vec(&current_response) {
        Ok(bytes) => (bytes::Bytes::from(bytes), round_usage),
        Err(_) => (body_bytes.clone(), round_usage),
    }
}

/// Error types Anthropic reports in-band that a retry can plausibly clear.
/// `invalid_request_error` and friends are excluded: resending an identical
/// body gets an identical refusal.
const RETRYABLE_IN_BAND_ERRORS: &[&str] = &["overloaded_error", "rate_limit_error", "api_error"];

/// Read just far enough into a streamed body to see whether it opens with an
/// error event, and hand back every byte consumed so the caller can put them
/// in front of the rest of the stream.
///
/// Returns `(prefix, Some(error_type))` when the first complete SSE event is a
/// retryable error, `(prefix, None)` otherwise. The prefix is always the exact
/// bytes read — on the ordinary path that is one `message_start` chunk, which
/// then leads the client's stream unchanged.
///
/// Bounded twice over: it stops at the first event terminator and gives up
/// after `MAX_PEEK_BYTES`. A body that never produces a blank line is a body
/// this proxy should not be buffering.
async fn peek_leading_sse_error(
    resp: &mut reqwest::Response,
) -> (bytes::Bytes, Option<&'static str>) {
    /// One SSE event is a few hundred bytes; 16 KiB is slack, not a budget.
    const MAX_PEEK_BYTES: usize = 16 * 1024;

    let is_sse = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if !is_sse {
        return (bytes::Bytes::new(), None);
    }

    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        // A transport error here is not ours to classify — hand back what we
        // have and let the normal stream path surface it.
        let Ok(chunk) = resp.chunk().await else {
            return (bytes::Bytes::from(buf), None);
        };
        let Some(chunk) = chunk else {
            // Body ended before a complete event. Nothing to retry on.
            return (bytes::Bytes::from(buf), None);
        };
        buf.extend_from_slice(&chunk);

        if let Some(end) = find_event_end(&buf) {
            let kind = leading_event_error_type(&buf[..end]);
            return (bytes::Bytes::from(buf), kind);
        }
        if buf.len() >= MAX_PEEK_BYTES {
            return (bytes::Bytes::from(buf), None);
        }
    }
}

/// Offset just past the first event terminator, tolerating CRLF.
fn find_event_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| i + 2)
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4))
}

/// The retryable error type carried by one framed SSE event, if any.
fn leading_event_error_type(event: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(event).ok()?;
    let data = text
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .map(str::trim)?;
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    if parsed.get("type").and_then(serde_json::Value::as_str) != Some("error") {
        return None;
    }
    let kind = parsed
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(serde_json::Value::as_str)?;
    RETRYABLE_IN_BAND_ERRORS
        .iter()
        .find(|known| **known == kind)
        .copied()
}

/// Build the memory context for a request, or `None` when memory is off or
/// uninitialised. Mirrors the gate on the injection site, so the proxy resolves
/// exactly the turns it injected into.
async fn memory_tool_context(
    state: &AppState,
    headers_snapshot: &Option<HeaderMap>,
    provider: Option<&str>,
    request_body: &bytes::Bytes,
) -> Option<MemoryToolContext> {
    let handler = state.memory_handler.as_ref()?;
    if !handler.lock().await.is_initialized() {
        return None;
    }
    let provider = match provider? {
        "anthropic" => crate::memory::tool_adapter::Provider::Anthropic,
        _ => crate::memory::tool_adapter::Provider::Openai,
    };
    let base_user_id = headers_snapshot
        .as_ref()
        .and_then(|h| h.get("x-headroom-user-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default");
    // The project comes from the system prompt's working directory, since
    // Claude Code sends no project header. It is resolved here rather than read
    // from `project_context`, which is a thread-local and so cannot be trusted
    // across an await on a multi-threaded runtime.
    let parsed: serde_json::Value = serde_json::from_slice(request_body).unwrap_or_default();
    let user_id = crate::memory::router::scoped_user_id(
        base_user_id,
        &crate::memory::router::RequestContext {
            headers: header_map_to_lowercase_strings(headers_snapshot.as_ref()),
            system_prompt: crate::memory::router::extract_system_prompt(&parsed),
            base_user_id: base_user_id.to_string(),
            project_root_override: None,
        },
    );
    Some(MemoryToolContext {
        handler: handler.clone(),
        provider,
        user_id,
    })
}

/// What a memory continuation needs. Assembled at the seam that has the
/// request in scope, the same way [`crate::handlers::local_model::RoutedCcr`]
/// is.
pub(crate) struct MemoryToolContext {
    pub handler: Arc<tokio::sync::Mutex<crate::memory::handler::MemoryHandler>>,
    pub provider: crate::memory::tool_adapter::Provider,
    pub user_id: String,
}

/// Execute `memory_*` tool calls the model made, and continue the turn.
///
/// The proxy injects these tools (see the injection site in `forward_http`),
/// so the proxy has to run them: the client has never heard of `memory_search`
/// and answers a call to it with `No such tool available`. `MemoryHandler`
/// could already execute them — until this function existed nothing ever asked
/// it to, on any path, streaming or buffered.
///
/// Deliberately shaped like [`handle_ccr_response`], down to the round cap and
/// the mixed-tool rule: a turn that calls a memory tool *and* a client tool is
/// left alone, because we cannot fabricate the client's half.
pub(crate) async fn handle_memory_response(
    body_bytes: &bytes::Bytes,
    original_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    memory: &MemoryToolContext,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
) -> (bytes::Bytes, CcrRoundUsage) {
    use headroom_core::ccr::response_handler::CCRResponseHandler;

    let mut round_usage = CcrRoundUsage::default();
    let items_field = if provider == "openai_responses" {
        "input"
    } else {
        "messages"
    };

    let Ok(response) = serde_json::from_slice::<serde_json::Value>(body_bytes) else {
        return (body_bytes.clone(), round_usage);
    };
    {
        let handler = memory.handler.lock().await;
        if !handler.is_initialized() || !handler.has_memory_tool_calls(&response, memory.provider) {
            return (body_bytes.clone(), round_usage);
        }
    }

    let Ok(mut current_request) = serde_json::from_slice::<serde_json::Value>(original_request)
    else {
        tracing::warn!(
            request_id = %request_id,
            "memory: failed to parse original request; skipping tool handling"
        );
        return (body_bytes.clone(), round_usage);
    };

    // A client tool call sharing the turn makes a continuation impossible: it
    // would send upstream an assistant turn whose client `tool_use` has no
    // `tool_result` — the client has not run it yet — and upstream rejects the
    // whole request, losing the memory answer with it. Answer the memory call
    // now and hold the answer for the next request, which carries that result.
    // Anthropic only: the holding area works in Anthropic block shapes.
    if provider == "anthropic" {
        let (ours, client_ids) = crate::memory::deferred::split_tool_calls(&response);
        if !ours.is_empty() && !client_ids.is_empty() {
            let results = {
                let handler = memory.handler.lock().await;
                handler
                    .handle_memory_tool_calls(&response, &memory.user_id, memory.provider, None)
                    .await
            };
            let held = pair_results_with_calls(&ours, &results, &client_ids);
            let count = held.len();
            if let Ok(mut store) = crate::memory::deferred::store().lock() {
                for pending in held {
                    store.hold(pending);
                }
            }
            tracing::info!(
                request_id = %request_id,
                event = "memory_answer_deferred",
                held = count,
                client_tool_calls = client_ids.len(),
                "memory: turn also calls a client tool; holding the answer for \
                 the next request"
            );
            // The calls have run. Leave the turn alone so the client's own tool
            // call reaches it untouched.
            return (body_bytes.clone(), round_usage);
        }
    }

    // Reused purely for its provider-aware message shaping — the CCR handler
    // knows how each provider wants an assistant turn and a tool result
    // expressed, and memory results go back the same way.
    let shaper = CCRResponseHandler::new(None);
    let mut current_response = response;
    let mut rounds = 0;

    while rounds < config.ccr_max_retrieval_rounds {
        let results: Vec<serde_json::Value> = {
            let handler = memory.handler.lock().await;
            if !handler.has_memory_tool_calls(&current_response, memory.provider) {
                break;
            }
            handler
                .handle_memory_tool_calls(&current_response, &memory.user_id, memory.provider, None)
                .await
        };
        if results.is_empty() {
            break;
        }

        // `handle_memory_tool_calls` returns provider-shaped tool results
        // already; wrap them the way the continuation array expects.
        let assistant_msg = shaper.extract_assistant_message(&current_response, provider);
        let tool_result_msg = memory_results_message(&results, provider);

        let Some(items) = current_request
            .get_mut(items_field)
            .and_then(|v| v.as_array_mut())
        else {
            tracing::warn!(
                request_id = %request_id,
                field = items_field,
                "memory: no continuation array in request; cannot continue"
            );
            break;
        };
        extend_or_push(items, assistant_msg, &["_openai_responses_output_items"]);
        extend_or_push(
            items,
            tool_result_msg,
            &["_memory_tool_results", "_openai_responses_tool_results"],
        );

        let Ok(continuation_body) = serde_json::to_vec(&current_request) else {
            break;
        };
        tracing::info!(
            request_id = %request_id,
            round = rounds + 1,
            results_count = results.len(),
            "memory: sending continuation request"
        );
        // A failed continuation takes the memory call down with it: the block
        // is already suppressed, so the tool the model asked for never runs and
        // the turn reaches the client short one tool call. Transport blips and
        // 429/5xx get another attempt; anything else is a body we built wrong,
        // so keep what upstream objected to instead of dropping it.
        let mut attempt: u32 = 0;
        let resp = loop {
            match client
                .post(upstream_url.clone())
                .headers(outgoing_headers.clone())
                .body(continuation_body.clone())
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => break Some(r),
                Ok(r) => {
                    let status = r.status();
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < MEMORY_CONTINUATION_RETRIES {
                        attempt += 1;
                        tokio::time::sleep(memory_continuation_backoff(attempt)).await;
                        continue;
                    }
                    let detail = r.text().await.unwrap_or_default();
                    tracing::warn!(
                        request_id = %request_id,
                        status = %status,
                        attempt,
                        round = rounds + 1,
                        detail = %first_bytes(&detail, 600),
                        "memory: upstream returned error during continuation"
                    );
                    break None;
                }
                Err(e) => {
                    if attempt < MEMORY_CONTINUATION_RETRIES && is_retryable_transport_error(&e) {
                        attempt += 1;
                        tokio::time::sleep(memory_continuation_backoff(attempt)).await;
                        continue;
                    }
                    tracing::warn!(
                        request_id = %request_id,
                        attempt,
                        round = rounds + 1,
                        error = %e,
                        "memory: upstream request failed during continuation"
                    );
                    break None;
                }
            }
        };
        let Some(resp) = resp else { break };
        let Ok(bytes) = resp.bytes().await else { break };
        round_usage.add_response(&current_response);
        let Ok(next) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            break;
        };
        current_response = next;
        rounds += 1;
    }

    // The cap is the other way a memory call gets stranded: the block is
    // suppressed, the round budget runs out, and the tool never runs. Say so —
    // the alternative is a turn quietly missing work the model asked for.
    if rounds >= config.ccr_max_retrieval_rounds {
        let still_pending = {
            let handler = memory.handler.lock().await;
            handler.has_memory_tool_calls(&current_response, memory.provider)
        };
        if still_pending {
            tracing::warn!(
                request_id = %request_id,
                rounds,
                "memory: retrieval round cap reached with calls outstanding; \
                 raise HEADROOM_CCR_MAX_RETRIEVAL_ROUNDS"
            );
        }
    }

    match serde_json::to_vec(&current_response) {
        Ok(bytes) => (bytes::Bytes::from(bytes), round_usage),
        Err(_) => (body_bytes.clone(), round_usage),
    }
}

/// Retries for a memory continuation that fails for a reason that may pass.
const MEMORY_CONTINUATION_RETRIES: u32 = 2;

/// Backoff before continuation attempt `attempt` (1-based): 250ms, then 500ms.
fn memory_continuation_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(250u64 << (attempt.saturating_sub(1)).min(4))
}

/// Leading bytes of an upstream error body, for a log line that has to stay
/// one line. Upstream puts the useful part first.
fn first_bytes(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.replace('\n', " ");
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…[{} more bytes]",
        &s[..end].replace('\n', " "),
        s.len() - end
    )
}

/// Match each memory `tool_use` with the `tool_result` answering it.
///
/// A call whose answer is missing is skipped: restoring it would put an
/// unanswered `tool_use` back into the history, which is the failure this
/// whole path avoids.
fn pair_results_with_calls(
    calls: &[serde_json::Value],
    results: &[serde_json::Value],
    client_ids: &[String],
) -> Vec<crate::memory::deferred::PendingMemoryResult> {
    calls
        .iter()
        .filter_map(|call| {
            let id = call.get("id").and_then(serde_json::Value::as_str)?;
            let answer = results
                .iter()
                .find(|r| r.get("tool_use_id").and_then(serde_json::Value::as_str) == Some(id))?;
            Some(crate::memory::deferred::PendingMemoryResult::new(
                call.clone(),
                answer.clone(),
                client_ids.to_vec(),
            ))
        })
        .collect()
}

/// Wrap provider-shaped memory tool results for the continuation array.
///
/// Anthropic wants one user turn holding every `tool_result` block; the OpenAI
/// shapes want one entry per result, so those go behind a sentinel key that
/// [`extend_or_push`] expands.
fn memory_results_message(results: &[serde_json::Value], provider: &str) -> serde_json::Value {
    match provider {
        "anthropic" => serde_json::json!({"role": "user", "content": results}),
        "openai_responses" => {
            serde_json::json!({"_openai_responses_tool_results": results})
        }
        _ => serde_json::json!({"_memory_tool_results": results}),
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
        // Carries usage on purpose: the caller parses the returned body for
        // its own accounting, so counting it here too would double-bill it.
        let final_body = serde_json::json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]}
            ],
            "usage": {"input_tokens": 7_777, "output_tokens": 99}
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

        // Upstream's first reply: a headroom_retrieve function_call. It
        // carries a usage block, because that first call was billed and the
        // client will never see it.
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "output": [
                    {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                     "arguments": format!("{{\"hash\":\"{hash}\"}}")}
                ],
                "usage": {
                    "input_tokens": 4_000,
                    "output_tokens": 60,
                    "cache_read_input_tokens": 30_000,
                    "cache_creation_input_tokens": 500
                }
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
        let (body, round_usage) = out;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["output"][0]["content"][0]["text"], "done");

        // The intercepted round was a real billed call. The caller only ever
        // parses the body returned above, so unless these come back with it
        // they are never accounted for anywhere.
        assert_eq!(round_usage.rounds, 1);
        assert_eq!(round_usage.input_tokens, 4_000);
        assert_eq!(round_usage.output_tokens, 60);
        assert_eq!(round_usage.cache_read_tokens, 30_000);
        assert_eq!(round_usage.cache_write_tokens, 500);
        // The returned body's own usage stays out of it — the caller adds that.
        assert_eq!(parsed["usage"]["input_tokens"], 7_777);
        assert_ne!(round_usage.input_tokens, 4_000 + 7_777);
    }

    /// No retrieval, no extra rounds — the common path must report nothing so
    /// the accounting is untouched.
    #[tokio::test]
    async fn ccr_reports_no_rounds_when_nothing_was_retrieved() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use wiremock::MockServer;

        let store = InMemoryCcrStore::new();
        let server = MockServer::start().await;
        let plain = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
                "usage": {"input_tokens": 10, "output_tokens": 2}
            }))
            .unwrap(),
        );
        let config = Config::for_test(server.uri().parse().unwrap());
        let (_body, round_usage) = handle_ccr_response(
            &plain,
            &plain,
            &format!("{}/v1/responses", server.uri()).parse().unwrap(),
            &reqwest::Client::new(),
            &store as &dyn headroom_core::ccr::CcrStore,
            &config,
            "req-test",
            &http::HeaderMap::new(),
            "openai_responses",
        )
        .await;
        assert!(round_usage.is_empty());
        assert_eq!(round_usage.input_tokens, 0);
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

    fn outcome_ctx_for_sizes(original_tokens: i64, tokens_saved: i64) -> OutcomeContext {
        OutcomeContext {
            sink: Arc::new(ProxyOutcomeSink {
                cost_tracker: Arc::new(headroom_core::cost_tracker::CostTracker::new(
                    None, "monthly",
                )),
                savings_tracker: Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
                    None, false,
                )),
                request_logger: Arc::new(crate::request_logger::RequestLogger::new(None)),
            }),
            model: "m".into(),
            provider: "anthropic".into(),
            tags: Default::default(),
            client: None,
            project: None,
            original_tokens,
            tokens_saved,
            transforms_applied: vec![],
            num_messages: 0,
            total_latency_ms: 0.0,
            overhead_ms: 0.0,
            started_at: Instant::now(),
            waste_signals: None,
            proactive_expansion_applied: false,
            wire_bytes: None,
            forwarded_tokens_estimate: 0,
            upstream_attempts: 1,
        }
    }

    #[test]
    fn proactive_expansion_cache_write_is_attributed_only_to_injected_requests() {
        let registry = crate::observability::prometheus::registry();
        let before =
            crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry);

        let untouched = outcome_ctx_for_sizes(0, 0);
        observe_proactive_expansion_cache_write(&untouched, 100);
        assert_eq!(
            crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry),
            before
        );

        let mut injected = outcome_ctx_for_sizes(0, 0);
        injected.proactive_expansion_applied = true;
        observe_proactive_expansion_cache_write(&injected, 100);
        assert_eq!(
            crate::observability::ctx_metrics::proactive_expansion_cache_write_tokens_get(registry),
            before + 100
        );
    }

    #[test]
    fn forwarded_rejections_persist_each_status_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = Arc::new(headroom_core::savings_tracker::SavingsTracker::new(
            Some(dir.path().join("proxy_savings.json")),
            false,
        ));
        let mut ctx = outcome_ctx_for_sizes(1_000, 100);
        ctx.sink = Arc::new(ProxyOutcomeSink {
            cost_tracker: Arc::new(headroom_core::cost_tracker::CostTracker::new(
                None, "monthly",
            )),
            savings_tracker: tracker.clone(),
            request_logger: Arc::new(crate::request_logger::RequestLogger::new(None)),
        });

        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            emit_failed_http_outcome(&ctx, "rejected", status, None);
        }

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot["lifetime"]["requests"], 0);
        assert_eq!(snapshot["failed_work"]["requests"], 3);
        assert_eq!(snapshot["failed_work"]["by_status"]["401"], 1);
        assert_eq!(snapshot["failed_work"]["by_status"]["429"], 1);
        assert_eq!(snapshot["failed_work"]["by_status"]["503"], 1);
        let metrics = tracker.metrics_snapshot(&serde_json::json!({}));
        assert_eq!(metrics["requests"]["total"], 0);
        assert_eq!(metrics["requests"]["failed"], 3);
    }

    #[test]
    fn billed_input_tokens_use_the_upstream_cache_usage_not_savings_baseline() {
        let outcome = headroom_core::request_outcome::RequestOutcome {
            // These are a compression comparison, not the provider bill.
            original_tokens: 100_000,
            optimized_tokens: 10_000,
            // Anthropic usage from the request that actually crossed the
            // proxy boundary: 2k uncached plus 7k cache read plus 1k write.
            uncached_input_tokens: 2_000,
            cache_read_tokens: 7_000,
            cache_write_tokens: 1_000,
            ..Default::default()
        };
        assert_eq!(provider_billed_input_tokens(&outcome), 10_000);
    }

    #[test]
    fn billed_input_tokens_fall_back_to_the_post_transform_estimate() {
        let outcome = headroom_core::request_outcome::RequestOutcome {
            original_tokens: 100_000,
            optimized_tokens: 10_000,
            ..Default::default()
        };
        assert_eq!(provider_billed_input_tokens(&outcome), 10_000);
    }

    /// When compression ran, its own pre-compression size is the baseline.
    #[test]
    fn sizes_uses_the_compression_baseline_when_there_is_one() {
        let ctx = outcome_ctx_for_sizes(10_000, 2_000);
        // The provider's count is deliberately inconsistent here: compression
        // measured the body itself, so its numbers win.
        assert_eq!(ctx.sizes(7_500), (10_000, 8_000));
    }

    /// The gap this closes: ctx_offload shrinks the body outside the
    /// compression pipeline, so `original_tokens` is 0 while `tokens_saved` is
    /// real. Booking that against a zero baseline reported a 0% saving and
    /// contributed nothing to the savings tracker.
    #[test]
    fn sizes_derives_a_baseline_when_compression_did_not_run() {
        let ctx = outcome_ctx_for_sizes(0, 1_500);
        // Forwarded 20k, removed 1.5k, so the body arrived at 21.5k.
        assert_eq!(ctx.sizes(20_000), (21_500, 20_000));

        let outcome = headroom_core::request_outcome::RequestOutcome {
            original_tokens: 21_500,
            tokens_saved: 1_500,
            ..Default::default()
        };
        assert!(
            (outcome.savings_pct() - 6.976_744_186_046_512).abs() < 1e-9,
            "a real saving must report a real percentage, got {}",
            outcome.savings_pct()
        );
    }

    /// Regression guard for items 1d/1e: the booked saving is the compression
    /// dispatcher's own per-turn figure, so `tok_after` can never go negative
    /// by absorbing a CTX-offload total measured against a different baseline.
    ///
    /// The numbers are the live turn from item 1e (2026-08-08 22:40:36Z):
    /// compression saw a 358-token live zone and freed 243, while the CTX
    /// transforms had already removed 12,197 tokens earlier in the pipeline.
    /// Folding that 12,197 into this subtraction was the original defect — it
    /// reported `tok_after = 358 - 12,440 = -12,082`.
    #[test]
    fn sizes_books_only_the_compression_turn_so_tok_after_stays_non_negative() {
        const COMPRESSION_TOKENS_BEFORE: i64 = 358;
        const COMPRESSION_TOKENS_FREED: i64 = 243;
        const CTX_TRANSFORM_TOKENS_SAVED: i64 = 12_197;

        let ctx = outcome_ctx_for_sizes(COMPRESSION_TOKENS_BEFORE, COMPRESSION_TOKENS_FREED);
        let (original, optimized) = ctx.sizes(0);

        // The published subtraction matches the `compression applied` line's
        // own arithmetic, which is the only per-turn measurement available.
        assert_eq!(original, COMPRESSION_TOKENS_BEFORE);
        assert_eq!(
            optimized,
            COMPRESSION_TOKENS_BEFORE - COMPRESSION_TOKENS_FREED
        );
        assert!(
            optimized >= 0,
            "tok_after must not go negative, got {optimized}"
        );

        // `saturating_sub` on i64 saturates at i64::MIN, not at zero, so it is
        // not the guard it looks like. Pin the shape the defect produced so a
        // future change that folds the CTX total back in fails here loudly.
        let folded_in = outcome_ctx_for_sizes(
            COMPRESSION_TOKENS_BEFORE,
            COMPRESSION_TOKENS_FREED + CTX_TRANSFORM_TOKENS_SAVED,
        );
        assert_eq!(folded_in.sizes(0).1, -12_082);
    }

    /// A passthrough turn stays at zero rather than inventing a saving.
    #[test]
    fn sizes_reports_no_saving_for_an_untouched_body() {
        let ctx = outcome_ctx_for_sizes(0, 0);
        assert_eq!(ctx.sizes(20_000), (20_000, 20_000));
        assert_eq!(ctx.sizes(0), (0, 0));
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

    /// The head is `tools` + `system` and nothing else — the parts the
    /// injection stages write to and the compressors never touch. Including
    /// `messages` would mix compression's savings into the overhead figure and
    /// make it meaningless.
    #[test]
    fn prefix_head_bytes_covers_tools_and_system_only() {
        let body = serde_json::json!({
            "model": "m",
            "system": "abc",
            "tools": [{"name": "a"}],
            "messages": [{"role": "user", "content": "a very long message body"}],
        });
        let head = prefix_head_bytes(&body);
        let without_messages = serde_json::json!({
            "model": "m",
            "system": "abc",
            "tools": [{"name": "a"}],
        });
        assert_eq!(head, prefix_head_bytes(&without_messages));
        assert!(head > 0);
    }

    /// A body with neither is zero, not a panic.
    #[test]
    fn prefix_head_bytes_handles_an_empty_body() {
        assert_eq!(prefix_head_bytes(&serde_json::json!({})), 0);
    }

    /// The inventory has to pair definitions with the calls the model made, in
    /// both provider shapes, or the never-called list is wrong.
    #[test]
    fn tool_inventory_pairs_definitions_with_calls() {
        let body = serde_json::json!({
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"name": "Workflow", "input_schema": {"type": "object"}},
                {"type": "function", "function": {"name": "Legacy"}}
            ],
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "1", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "2", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1"}]}
            ]
        });
        let (defs, calls) = tool_inventory_of(&body);
        let names: Vec<&str> = defs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Read", "Workflow", "Legacy"]);
        assert!(defs.iter().all(|(_, b)| *b > 0));
        assert_eq!(calls, vec![("Read".to_string(), 2)]);
    }

    /// A `tool_result` is not a call. Counting it would make every tool look
    /// used and the never-called list would always be empty.
    #[test]
    fn tool_results_do_not_count_as_calls() {
        let body = serde_json::json!({
            "tools": [{"name": "Read"}],
            "messages": [{"role": "user", "content": [
                // Carries a `name` on purpose: the block *type* has to be what
                // excludes it, not the field happening to be absent.
                {"type": "tool_result", "tool_use_id": "1", "name": "Read", "content": "x"},
                {"type": "text", "text": "and some prose", "name": "Read"}
            ]}]
        });
        let (_, calls) = tool_inventory_of(&body);
        assert!(
            calls.is_empty(),
            "only tool_use blocks are calls, got {calls:?}"
        );
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

    // ── rewritten_message_report ─────────────────────────────────────

    #[test]
    fn cache_control_placement_is_not_a_rewrite() {
        // The proxy re-places the breakpoint every turn by design; counting
        // that as a rewrite would mark every message and say nothing.
        let before =
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "hi"}]});
        let after = serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]});
        assert!(rewritten_message_report(&[before], &[after])
            .indices
            .is_empty());
    }

    #[test]
    fn compressed_text_beside_a_thinking_block_is_flagged() {
        let before = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"},
            {"type": "text", "text": "a long log line"}]});
        let after = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"},
            {"type": "text", "text": "[compressed]"}]});
        let report = rewritten_message_report(&[before], &[after]);
        assert_eq!(report.indices, vec![0]);
        assert_eq!(report.with_thinking, vec![0]);
    }

    #[test]
    fn a_rewrite_without_thinking_is_not_flagged() {
        let before = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "content": "a long log line"}]});
        let after = serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "content": "[compressed]"}]});
        let report = rewritten_message_report(&[before], &[after]);
        assert_eq!(report.indices, vec![0]);
        assert!(report.with_thinking.is_empty());
    }

    #[test]
    fn stripping_cache_control_off_a_signed_block_counts_as_touching_it() {
        // The canonical compare is blind here on purpose, so this is the only
        // list that can catch it. The provider judges the block as sent.
        let before = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig",
             "cache_control": {"type": "ephemeral"}}]});
        let after = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"}]});
        let report = rewritten_message_report(&[before], &[after]);
        assert!(report.indices.is_empty());
        assert_eq!(report.thinking_touched, vec![0]);
    }

    #[test]
    fn an_untouched_signed_block_is_not_reported() {
        let msg = serde_json::json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "…", "signature": "sig"},
            {"type": "text", "text": "hello"}]});
        let report = rewritten_message_report(&[msg.clone()], &[msg]);
        assert!(report.thinking_touched.is_empty());
    }

    #[test]
    fn index_lists_are_capped() {
        let many: Vec<usize> = (0..25).collect();
        assert_eq!(join_indices(&many[..3]), "0,1,2");
        assert!(join_indices(&many).ends_with("…+5"));
    }

    // ── describe_upstream_error ──────────────────────────────────────

    #[test]
    fn describes_an_anthropic_rejection() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error",
            "message":"messages.11: unexpected block"}}"#;
        let (kind, message) = describe_upstream_error(body);
        assert_eq!(kind, "invalid_request_error");
        assert_eq!(message, "messages.11: unexpected block");
    }

    #[test]
    fn describes_an_openai_rejection() {
        let body = br#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#;
        let (kind, message) = describe_upstream_error(body);
        assert_eq!(kind, "context_length_exceeded");
        assert_eq!(message, "too long");
    }

    #[test]
    fn unknown_error_shapes_reach_the_log_as_nothing() {
        // The point of the helper: a body the proxy does not recognise must not
        // be forwarded into the log verbatim.
        let (kind, message) = describe_upstream_error(b"<html>secret</html>");
        assert_eq!(kind, "unparsed");
        assert!(message.is_empty());
        let (kind, message) = describe_upstream_error(br#"{"detail":"secret"}"#);
        assert_eq!(kind, "no_error_field");
        assert!(message.is_empty());
    }

    #[test]
    fn long_error_messages_are_truncated() {
        let long = "x".repeat(2_000);
        let body = format!(r#"{{"error":{{"type":"e","message":"{long}"}}}}"#);
        let (_, message) = describe_upstream_error(body.as_bytes());
        assert_eq!(message.chars().count(), 400);
    }

    // ── anthropic_cache_ttl_split ────────────────────────────────────
    #[test]
    fn cache_ttl_split_reads_the_nested_cache_creation_object() {
        let usage = serde_json::json!({
            "input_tokens": 12,
            "cache_creation_input_tokens": 4_000,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 1_000,
                "ephemeral_1h_input_tokens": 3_000
            }
        });
        assert_eq!(anthropic_cache_ttl_split(Some(&usage)), (1_000, 3_000));
    }

    #[test]
    fn cache_ttl_split_is_zero_when_the_provider_omits_it() {
        // OpenAI shapes, and older Anthropic bodies, carry no nested object.
        // Pricing treats (0, 0) as "unreported" and falls back to the 5m rate
        // rather than inventing a 1h premium.
        let usage = serde_json::json!({"prompt_tokens": 10, "completion_tokens": 2});
        assert_eq!(anthropic_cache_ttl_split(Some(&usage)), (0, 0));
        assert_eq!(anthropic_cache_ttl_split(None), (0, 0));
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

    #[test]
    fn hidden_ccr_continuation_does_not_become_next_client_cache_baseline() {
        use crate::cache_stabilization::usage_observer::UsageObserver;

        let mut usage = CcrRoundUsage::default();
        usage.add_response(&serde_json::json!({
            "usage": {
                "input_tokens": 1_025,
                "cache_read_input_tokens": 92_100,
                "cache_creation_input_tokens": 1_025,
                "output_tokens": 100
            }
        }));
        let baseline = usage.client_cache_baseline(0, 92_100, 129_915);
        assert_eq!(baseline, (1_025, 92_100, 1_025));

        let observer = UsageObserver::new();
        observer.begin_request("ccr-1", "ccr-conv".into(), None, None, None);
        observer.complete("ccr-1", baseline.0, baseline.1, baseline.2, None);
        observer.begin_request("ccr-2", "ccr-conv".into(), None, None, None);
        let class = observer.complete("ccr-2", 2, 93_125, 525, None);

        assert_eq!(
            class, None,
            "93,125 exactly reuses the client-visible baseline"
        );
        assert!(observer.snapshot().last_event.is_none());
    }

    #[test]
    fn replay_decline_logs_hashed_session_and_chain_identity() {
        use crate::cache_stabilization::drift_detector::session_key_log_prefix;
        use crate::cache_stabilization::prefix_replay::SessionReplayStore;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::Layer;

        struct Capture(Arc<Mutex<Vec<HashMap<String, String>>>>);

        impl<S: tracing::Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                struct Visitor(HashMap<String, String>);

                impl tracing::field::Visit for Visitor {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0
                            .insert(field.name().to_string(), format!("{value:?}"));
                    }

                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        self.0.insert(field.name().to_string(), value.to_string());
                    }

                    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                        self.0.insert(field.name().to_string(), value.to_string());
                    }
                }

                let mut visitor = Visitor(HashMap::new());
                event.record(&mut visitor);
                if visitor
                    .0
                    .get("event")
                    .is_some_and(|name| name == "prefix_replay_not_replayed")
                {
                    self.0.lock().unwrap().push(visitor.0);
                }
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(captured.clone()));
        let session_key = "Bearer never-log-this-session-key";
        let expected_hash = session_key_log_prefix(session_key);
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let body = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"messages": messages.clone()})).unwrap(),
        );

        tracing::subscriber::with_default(subscriber, || {
            apply_prefix_replay(
                &SessionReplayStore::new(2),
                session_key,
                "replay-log-test",
                messages,
                body,
                None,
                7,
                2,
                false,
            );
        });

        let captured = captured.lock().unwrap();
        let event = captured
            .first()
            .expect("first turn must emit a prefix_replay_not_replayed event");
        assert_eq!(event.get("session_key_hash"), Some(&expected_hash));
        assert_eq!(event.get("chain_id"), Some(&"0".to_string()));
        assert!(
            event.values().all(|value| !value.contains(session_key)),
            "the raw session key must never be written to the event: {event:?}"
        );
    }

    // ── drop_unsigned_reasoning_blocks ───────────────────────────
    //
    // The counterpart to `sse::stream_finisher`. These pin the two things that
    // make it safe to run on every Anthropic turn: it is inert unless a stream
    // actually died mid-thinking, and when it does fire it does not move the
    // prompt-cache boundary.

    fn body_with(messages: serde_json::Value) -> bytes::Bytes {
        bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model": "claude", "messages": messages}))
                .unwrap(),
        )
    }

    fn messages_of(body: &bytes::Bytes) -> serde_json::Value {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["messages"].clone()
    }

    #[test]
    fn unsigned_reasoning_drop_is_inert_without_an_unsigned_block() {
        // No reasoning at all: not even parsed, and byte-identical out.
        let plain = body_with(serde_json::json!([{"role": "user", "content": "hi"}]));
        assert_eq!(drop_unsigned_reasoning_blocks(plain.clone(), "r"), plain);

        // A signed block is a real one and must survive untouched.
        let signed = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "t", "signature": "sig"},
                {"type": "text", "text": "answer"},
            ],
        }]));
        assert_eq!(drop_unsigned_reasoning_blocks(signed.clone(), "r"), signed);

        // `redacted_thinking` carries `data`, never a signature.
        let redacted = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "answer"},
            ],
        }]));
        assert_eq!(
            drop_unsigned_reasoning_blocks(redacted.clone(), "r"),
            redacted
        );
    }

    #[test]
    fn unsigned_reasoning_is_dropped_and_the_turn_stays_sendable() {
        // The shape `stream_finisher` leaves behind: thinking cut off before
        // its signature, then the truncation marker as text.
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "half a thought"},
                {"type": "text", "text": "[truncated: ...]"},
            ],
        }]));
        let out = drop_unsigned_reasoning_blocks(body, "r");
        let content = &messages_of(&out)[0]["content"];
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn a_cache_breakpoint_on_a_dropped_block_moves_rather_than_vanishes() {
        // The marker sits on the doomed block. Losing it would shift the cached
        // prefix boundary and cost a re-cache on every later turn.
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "half",
                    "cache_control": {"type": "ephemeral"},
                },
                {"type": "text", "text": "tail"},
            ],
        }]));
        let out = drop_unsigned_reasoning_blocks(body, "r");
        let content = messages_of(&out)[0]["content"].clone();
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(
            content[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"}),
            "the breakpoint should have carried to the surviving block"
        );
    }

    #[test]
    fn a_breakpoint_carries_backwards_when_the_dropped_block_was_last() {
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "text", "text": "lead"},
                {
                    "type": "thinking",
                    "thinking": "half",
                    "cache_control": {"type": "ephemeral"},
                },
            ],
        }]));
        let out = drop_unsigned_reasoning_blocks(body, "r");
        let content = messages_of(&out)[0]["content"].clone();
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(
            content[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn a_breakpoint_is_never_doubled_onto_a_block_that_has_one() {
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "half",
                    "cache_control": {"type": "ephemeral"},
                },
                {
                    "type": "text",
                    "text": "tail",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                },
            ],
        }]));
        let out = drop_unsigned_reasoning_blocks(body, "r");
        let content = messages_of(&out)[0]["content"].clone();
        assert_eq!(
            content[0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "the block's own marker wins; breakpoints are a budget of four"
        );
    }

    #[test]
    fn a_message_is_left_alone_when_dropping_would_empty_it() {
        // Upstream refuses empty content as firmly as it refuses the unsigned
        // block, so there is nothing to gain by trading one for the other.
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "half"}],
        }]));
        assert_eq!(drop_unsigned_reasoning_blocks(body.clone(), "r"), body);
    }

    #[test]
    fn dropping_is_idempotent_so_the_prefix_holds_across_turns() {
        // The property the cache depends on: once a truncated turn is in the
        // history, every later turn carries it, and each must produce the same
        // bytes upstream or the prefix moves under the cache every turn.
        let body = body_with(serde_json::json!([
            {"role": "user", "content": "q"},
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "half"},
                    {"type": "text", "text": "[truncated: ...]"},
                ],
            },
            {"role": "user", "content": "carry on"},
        ]));
        let once = drop_unsigned_reasoning_blocks(body, "r");
        let twice = drop_unsigned_reasoning_blocks(once.clone(), "r");
        assert_eq!(once, twice, "a second pass must change nothing");
    }

    #[test]
    fn the_tampering_guard_does_not_see_an_unsigned_drop_as_a_rewrite() {
        // `restore_client_reasoning_blocks` reverts the whole message array
        // when the outbound signed blocks stop matching the client's. If it
        // counted unsigned ones it would revert this drop every turn — and
        // with it every byte of compression on that turn.
        let client: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "real", "signature": "sig"},
                {"type": "thinking", "thinking": "half"},
                {"type": "text", "text": "tail"},
            ],
        }]))
        .unwrap();
        let dropped = drop_unsigned_reasoning_blocks(
            body_with(serde_json::Value::Array(client.clone())),
            "r",
        );
        let forwarded: Vec<serde_json::Value> =
            serde_json::from_value(messages_of(&dropped)).unwrap();
        assert_eq!(
            signed_reasoning_blocks(&client),
            signed_reasoning_blocks(&forwarded),
            "dropping an unsigned block must leave the signed set identical"
        );
    }

    #[test]
    fn a_signed_block_beside_an_unsigned_one_survives_verbatim() {
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "real", "signature": "sig"},
                {"type": "thinking", "thinking": "half"},
                {"type": "text", "text": "tail"},
            ],
        }]));
        let out = drop_unsigned_reasoning_blocks(body, "r");
        let content = messages_of(&out)[0]["content"].clone();
        assert_eq!(content.as_array().unwrap().len(), 2);
        assert_eq!(content[0]["signature"], "sig");
        assert_eq!(content[0]["thinking"], "real");
    }

    #[test]
    fn apply_prefix_replay_pipes_inbound_tail_evidence_to_usage_observer() {
        use crate::cache_stabilization::prefix_replay::SessionReplayStore;
        use crate::cache_stabilization::usage_observer::{RecacheEventKind, UsageObserver};

        let store = SessionReplayStore::new(2);
        let observer = UsageObserver::new();
        let session_key = "tail-evidence-session";
        let prior = vec![
            serde_json::json!({"role":"user","content":"open"}),
            serde_json::json!({"role":"assistant","content":"answer"}),
            serde_json::json!({"role":"user","content":"old tail"}),
        ];

        observer.begin_request("tail-1", "tail-conversation".into(), None, None, None);
        let prior_body = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"messages": prior.clone()})).unwrap(),
        );
        apply_prefix_replay(
            &store,
            session_key,
            "tail-1",
            prior.clone(),
            prior_body,
            Some(&observer),
            1,
            2,
            false,
        );
        store.complete("tail-1", 0, 50_000);
        observer.complete("tail-1", 200, 0, 50_000, None);

        let mut current = prior;
        current[2] = serde_json::json!({"role":"user","content":"replacement tail"});
        observer.begin_request("tail-2", "tail-conversation".into(), None, None, None);
        let current_body = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({"messages": current.clone()})).unwrap(),
        );
        apply_prefix_replay(
            &store,
            session_key,
            "tail-2",
            current,
            current_body,
            Some(&observer),
            2,
            2,
            false,
        );
        let class = observer.complete("tail-2", 200, 0, 50_000, None);

        assert_eq!(class, None, "a branch cache build is not a cache miss");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.recache_wasted_tokens_total, 0);
        let event = snapshot.last_event.expect("branch cache build recorded");
        assert_eq!(event.event_kind, RecacheEventKind::Branch);
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("inbound_tail_replaced")
        );
        assert_eq!(event.origin.as_deref(), Some("inbound"));
        assert_eq!(event.scope.as_deref(), Some("final_message"));
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

    // ── signed reasoning blocks ──────────────────────────────────

    fn client_body_with_thinking() -> serde_json::Value {
        serde_json::json!({
            "model": "claude-sonnet-4-5[1m]",
            "tools": [{"name": "a"}, {"name": "b"}],
            "messages": [
                {"role": "user", "content": "solve this"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "private reasoning", "signature": "sig123"},
                    {"type": "text", "text": "42"}
                ]},
                {"role": "user", "content": "continue"}
            ]
        })
    }

    fn as_bytes(v: &serde_json::Value) -> bytes::Bytes {
        bytes::Bytes::from(serde_json::to_vec(v).unwrap())
    }

    /// The common case: the pipeline changed something outside the message
    /// array, so the signed blocks still match and the body goes as built.
    #[test]
    fn a_body_whose_reasoning_blocks_survive_is_forwarded_as_built() {
        let original = as_bytes(&client_body_with_thinking());
        let mut sent = client_body_with_thinking();
        sent["model"] = serde_json::json!("claude-sonnet-4-5");
        sent["tools"] = serde_json::json!([{"name": "a"}]);
        let sent = as_bytes(&sent);

        let out = restore_client_reasoning_blocks(sent.clone(), &original, "r1");
        assert_eq!(out, sent);
    }

    /// A body with no signed block never takes the restore path, however much
    /// the pipeline rewrote it.
    #[test]
    fn a_body_without_reasoning_blocks_is_forwarded_as_built() {
        let original = as_bytes(&serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}]
        }));
        let sent = as_bytes(&serde_json::json!({
            "messages": [{"role": "user", "content": "compressed"}]
        }));

        let out = restore_client_reasoning_blocks(sent.clone(), &original, "r1");
        assert_eq!(out, sent);
    }

    /// Editing a signed block is what Anthropic refuses. The client's message
    /// array goes back; the model rewrite outside it stays.
    #[test]
    fn an_edited_reasoning_block_restores_the_client_messages() {
        let original = as_bytes(&client_body_with_thinking());
        let mut sent = client_body_with_thinking();
        sent["model"] = serde_json::json!("claude-sonnet-4-5");
        sent["messages"][1]["content"][0]["thinking"] = serde_json::json!("edited");
        let sent = as_bytes(&sent);

        let out = restore_client_reasoning_blocks(sent, &original, "r1");
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            parsed["messages"][1]["content"][0]["thinking"],
            "private reasoning"
        );
        assert_eq!(parsed["model"], "claude-sonnet-4-5");
    }

    /// Dropping the message that held the block counts as altering it: the
    /// signed blocks on the wire no longer match what the client sent.
    #[test]
    fn a_dropped_reasoning_block_restores_the_client_messages() {
        let original = as_bytes(&client_body_with_thinking());
        let mut sent = client_body_with_thinking();
        sent["messages"].as_array_mut().unwrap().remove(1);
        let sent = as_bytes(&sent);

        let out = restore_client_reasoning_blocks(sent, &original, "r1");
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["messages"][1]["content"][0]["signature"], "sig123");
    }

    // ── cache_control TTL ordering ───────────────────────────────

    /// A body with no 1h marker cannot break the rule, and pays no parse.
    #[test]
    fn a_body_with_no_1h_marker_skips_the_ttl_repair() {
        let body = as_bytes(&serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}
            ]}]
        }));
        let out = enforce_cache_control_ttl_order(body.clone(), &body, false, "r1");
        assert_eq!(out, body);
    }

    /// The `/btw` case end to end: the client's turn is in the 5m lane and a
    /// replayed 1h marker sits behind its breakpoints.
    #[test]
    fn a_replayed_1h_marker_is_contained_before_forwarding() {
        let client = as_bytes(&serde_json::json!({
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "u"}]}]
        }));
        let sent = as_bytes(&serde_json::json!({
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]}]
        }));

        let out = enforce_cache_control_ttl_order(sent, &client, false, "r1");
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(parsed["messages"][0]["content"][0]["cache_control"]
            .get("ttl")
            .is_none());
    }

    /// B1 authors those 1h markers on purpose, so they are not a leak and the
    /// pin must survive the guard.
    #[test]
    fn the_forced_1h_pin_survives_the_ttl_repair() {
        let client = as_bytes(&serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}
            ]}]
        }));
        let sent = as_bytes(&serde_json::json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "u",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]}]
        }));

        let out = enforce_cache_control_ttl_order(sent.clone(), &client, true, "r1");
        assert_eq!(out, sent);
    }

    // ── turn-hook re-drive accounting ──────────────────────────────────

    /// Each surface spells the same quantities differently, and reading a
    /// response by the wrong name silently bills it as zero.
    #[test]
    fn usage_is_read_by_the_provider_shape() {
        let anthropic = serde_json::json!({"usage": {
            "input_tokens": 10, "output_tokens": 2,
            "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4
        }});
        assert_eq!(response_usage(&anthropic, "anthropic"), (10, 2, 3, 4));

        let responses = serde_json::json!({"usage": {
            "input_tokens": 10, "output_tokens": 2,
            "input_tokens_details": {"cached_tokens": 3}
        }});
        assert_eq!(
            response_usage(&responses, "openai_responses"),
            (10, 2, 3, 0)
        );

        let chat = serde_json::json!({"usage": {
            "prompt_tokens": 10, "completion_tokens": 2,
            "prompt_tokens_details": {"cached_tokens": 3}
        }});
        assert_eq!(response_usage(&chat, "openai_chat"), (10, 2, 3, 0));

        // A response with no usage block reads as zeros rather than failing.
        assert_eq!(
            response_usage(&serde_json::json!({}), "anthropic"),
            (0, 0, 0, 0)
        );
    }

    /// No re-drive: the one response recorded is the one the outcome block
    /// reads, so the accounting has to come out untouched.
    #[test]
    fn a_hook_that_never_calls_the_model_reports_nothing() {
        let response = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
        let mut usage = TurnHookUsage::default();
        usage.record(&response, "anthropic");
        usage.settle(&response, "anthropic");
        assert!(usage.is_empty());
    }

    /// Whichever response the hook hands back, what is left is the spend the
    /// outcome block would otherwise miss.
    #[test]
    fn settling_leaves_only_the_calls_the_outcome_block_misses() {
        let original = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
        let redrive = serde_json::json!({"usage": {"input_tokens": 300, "output_tokens": 20}});

        // The hook looked and left the response alone: its own call is the delta.
        let mut kept = TurnHookUsage::default();
        kept.record(&original, "anthropic");
        kept.record(&redrive, "anthropic");
        kept.settle(&original, "anthropic");
        assert_eq!(kept.calls, 1);
        assert_eq!(kept.input_tokens, 300);
        assert_eq!(kept.output_tokens, 20);

        // The hook returned the re-drive: now the original is the unread one.
        let mut replaced = TurnHookUsage::default();
        replaced.record(&original, "anthropic");
        replaced.record(&redrive, "anthropic");
        replaced.settle(&redrive, "anthropic");
        assert_eq!(replaced.calls, 1);
        assert_eq!(replaced.input_tokens, 100);
        assert_eq!(replaced.output_tokens, 10);
    }

    /// A hook may hand back a response it built itself, matching no upstream
    /// call. The delta plus what the outcome block reads still has to come to
    /// what was really billed.
    #[test]
    fn a_synthesised_response_still_totals_the_real_spend() {
        let original = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
        let redrive = serde_json::json!({"usage": {"input_tokens": 300, "output_tokens": 20}});

        let mut usage = TurnHookUsage::default();
        usage.record(&original, "anthropic");
        usage.record(&redrive, "anthropic");
        usage.settle(&serde_json::json!({"id": "made-up"}), "anthropic");
        // The outcome block reads nothing off the synthetic body, so the delta
        // carries both real calls.
        assert_eq!(usage.input_tokens, 400);
        assert_eq!(usage.output_tokens, 30);
    }

    /// Inflated figures in a synthesised response must not drive the delta
    /// negative and bill less than the turn cost.
    #[test]
    fn settling_never_goes_negative() {
        let original = serde_json::json!({"usage": {"input_tokens": 100, "output_tokens": 10}});
        let inflated = serde_json::json!({"usage": {"input_tokens": 9000, "output_tokens": 900}});
        let mut usage = TurnHookUsage::default();
        usage.record(&original, "anthropic");
        usage.settle(&inflated, "anthropic");
        assert!(usage.is_empty());
    }

    /// A hook that calls the model twice made two billed requests, and the
    /// outcome block reads neither.
    #[tokio::test]
    async fn call_model_records_every_redrive() {
        use crate::turn_hooks::CallModel;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "redrive",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 7,
                    "cache_read_input_tokens": 40,
                    "cache_creation_input_tokens": 5
                }
            })))
            .mount(&server)
            .await;

        let usage = Arc::new(std::sync::Mutex::new(TurnHookUsage::default()));
        let call_model = ProxyCallModel {
            template: serde_json::json!({"model": "claude-x", "messages": []}),
            upstream_url: format!("{}/v1/messages", server.uri()).parse().unwrap(),
            client: reqwest::Client::new(),
            headers: http::HeaderMap::new(),
            request_id: "req-hook".to_string(),
            usage: Arc::clone(&usage),
            usage_provider: "anthropic".to_string(),
        };

        assert_eq!(call_model.call(vec![]).await["id"], "redrive");
        call_model.call(vec![]).await;

        let recorded = *usage.lock().unwrap();
        assert_eq!(recorded.calls, 2);
        assert_eq!(recorded.input_tokens, 200);
        assert_eq!(recorded.output_tokens, 14);
        assert_eq!(recorded.cache_read_tokens, 80);
        assert_eq!(recorded.cache_write_tokens, 10);
    }

    /// A call that never reached the upstream was not billed, so it must not
    /// show up as spend.
    #[tokio::test]
    async fn a_failed_redrive_records_nothing() {
        use crate::turn_hooks::CallModel;

        let usage = Arc::new(std::sync::Mutex::new(TurnHookUsage::default()));
        let call_model = ProxyCallModel {
            template: serde_json::json!({"model": "claude-x", "messages": []}),
            // Reserved as invalid by RFC 6890; nothing is listening.
            upstream_url: "http://192.0.2.1:1/v1/messages".parse().unwrap(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(200))
                .build()
                .unwrap(),
            headers: http::HeaderMap::new(),
            request_id: "req-hook".to_string(),
            usage: Arc::clone(&usage),
            usage_provider: "anthropic".to_string(),
        };

        assert!(call_model.call(vec![]).await.is_null());
        assert!(usage.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod identity_trust_tests {
    use super::*;

    #[test]
    fn loopback_callers_may_choose_their_partition() {
        for ip in ["127.0.0.1", "::1", "127.0.0.53", "localhost"] {
            assert!(
                identity_header_is_trusted(Some(ip)),
                "{ip} should be trusted"
            );
        }
    }

    #[test]
    fn remote_callers_may_not() {
        for ip in ["10.0.0.5", "192.168.1.20", "8.8.8.8", "2606:4700::1111"] {
            assert!(
                !identity_header_is_trusted(Some(ip)),
                "{ip} must not be trusted"
            );
        }
    }

    #[test]
    fn unknown_peer_fails_closed() {
        // The loopback guard treats None as local; partition selection must not.
        assert!(crate::loopback_guard::is_loopback_host(None));
        assert!(!identity_header_is_trusted(None));
    }
}
