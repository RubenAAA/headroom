//! Shared proxy state: `AppState`, its constructors and accessors, and
//! the CTX offload runtime it owns.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

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
    /// Optional per-stream Zen transports. These are never used for the main
    /// Claude route or routed Codex/OpenAI calls.
    pub(crate) zen_egresses: Option<Arc<ProviderEgressPool>>,
    pub(crate) default_egress_id: String,
    /// Bounded pool of transports for caller-selected upstreams. A cache key
    /// contains the hostname and the complete approved address set, so a DNS
    /// change receives a new client and cannot reuse a connection pinned to a
    /// different resolution.
    pub(crate) caller_clients: Arc<Mutex<lru::LruCache<CallerClientKey, reqwest::Client>>>,
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
    /// The same detector, run over the body the proxy actually forwards.
    ///
    /// `drift_state` is fed the inbound body, before any stage has touched
    /// it, so proxy-caused movement in the hot zone cannot appear there —
    /// which left every recache we caused ourselves in the classifier's
    /// `unexplained_after_replay` bucket. Comparing the two answers "did we
    /// do this?" directly. Separate LRU, because the two hash sequences are
    /// different and sharing one would make each overwrite the other.
    pub outbound_drift_state: DriftState,
    /// B2: per-session record of the tool order last forwarded upstream,
    /// bounded to 1000 sessions. Read and written once per Anthropic
    /// request, after tools are final.
    pub tool_order_state: cache_stabilization::tool_order::ToolOrderStore,
    pub roster_pin_state: cache_stabilization::tool_roster_pin::RosterPinStore,
    /// Reversible redaction memory (placeholder <-> original per session).
    /// Read and written on routed translate paths when `--redact-sensitive`
    /// is on; in-memory only, never serialized or logged.
    pub redact_store: crate::redact::RedactStore,
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
    /// Per-conversation opening-sentence pins. Same lifetime rules as
    /// `working_dir_pins`; see [`cache_stabilization::role_sentence`].
    pub role_sentence_pins: cache_stabilization::role_sentence::RoleSentencePins,
    /// Same-head stampede gate; see [`cache_stabilization::prefix_stampede`].
    pub stampede_gate: cache_stabilization::prefix_stampede::PrefixStampedeGate,
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
    /// Live Cursor-agent conversations, and the tools each has on offer.
    ///
    /// Held on the state rather than per-request because a conversation
    /// outlives the request that opened it: when the model reaches for a tool
    /// the response ends and the agent process stays blocked until a later
    /// request brings the result back. See `crate::cursor`.
    pub cursor_bridge: Arc<crate::cursor::bridge::Bridge>,
    /// Memory handler: orchestrates memory tool injection, context search,
    /// and tool call execution. `Some` only when `config.memory_enabled`.
    pub memory_handler: Option<Arc<crate::memory::handler::MemoryHandler>>,
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
    /// Dashboard client CIDRs authorized for sensitive stats metadata.
    /// Separate from the gateway list by design (see
    /// `forwarded_headers::TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV`); empty by
    /// default, which authorizes loopback callers only.
    pub trusted_dashboard_client_cidrs: Vec<crate::forwarded_headers::IpCidr>,
    /// Background compressor for deferred off-path compression jobs.
    pub background_compressor: Option<Arc<crate::background_compression::BackgroundCompressor>>,
    /// Fail-closed action for compression failures on WebSocket frames.
    pub compression_failure_action: crate::compression_failure::CompressionFailureAction,
    /// CCR batch context store — keyed by upstream batch id, holds the
    /// original (pre-compression) request messages/tools/model so batch
    /// results can be CCR-post-processed. Constructed unconditionally.
    pub batch_context_store: Arc<headroom_core::ccr::BatchContextStore>,
    /// Cost-aware routing targets that recently failed a turn, with the time
    /// each becomes eligible again. Written when a routed turn falls back to
    /// the client's own model, read by the router on every request it sees, so
    /// one outage costs one failed turn rather than one per tool-less turn.
    pub model_route_cooldowns: crate::model_router::ModelCooldowns,
}

impl AppState {
    /// Select the provider transport and opaque egress identity pinned to an
    /// inbound stream lane. Empty/missing lanes use the first configured
    /// egress; real routed requests carry the lane derived from the client
    /// session and original system prompt.
    ///
    /// A selected pool egress comes with its in-flight guard, taken under the
    /// same check that turns requests away from a rotating egress.
    pub(crate) fn zen_client_for_lane(
        &self,
        lane_key: Option<&str>,
    ) -> Result<ZenEgressSelection<'_>, String> {
        match self.zen_egresses.as_ref() {
            Some(pool) => {
                let slot = pool.slot_for_lane(lane_key.unwrap_or_default());
                let guard = pool.acquire(slot)?;
                Ok((
                    &pool.clients[slot],
                    slot,
                    pool.egress_ids[slot].as_str(),
                    Some(guard),
                ))
            }
            None => Ok((&self.client, 0, self.default_egress_id.as_str(), None)),
        }
    }

    /// Re-take the in-flight guard for a Zen egress already selected, as a
    /// held turn does before each probe. `Err` carries the egress ID while it
    /// is rotating; without a pool there is nothing to count.
    pub(crate) fn acquire_zen_egress(
        &self,
        slot: usize,
    ) -> Result<Option<EgressInflightGuard>, String> {
        match self.zen_egresses.as_ref() {
            Some(pool) => pool.acquire(slot).map(Some),
            None => Ok(None),
        }
    }

    /// Per-egress in-flight counts for `/debug/inflight`, keyed by the same
    /// opaque IDs `/debug/zen-egresses` lists. Empty without a pool.
    pub(crate) fn zen_egress_in_flight(&self) -> serde_json::Map<String, serde_json::Value> {
        self.zen_egresses
            .as_ref()
            .map(|pool| pool.in_flight_by_egress())
            .unwrap_or_default()
    }

    /// Toggle admission for one configured Zen egress while its upstream
    /// SOCKS endpoint is changing. Existing requests remain visible to the
    /// normal in-flight drain and can finish before old tunnels are closed.
    pub(crate) fn set_zen_egress_maintenance(&self, egress_id: &str, rotating: bool) -> bool {
        self.zen_egresses
            .as_ref()
            .is_some_and(|pool| pool.set_maintenance(egress_id, rotating))
    }

    /// Safe inventory for the local rotation watcher. Egress IDs are opaque
    /// hashes; proxy URLs and credentials are never returned.
    pub(crate) fn zen_egress_inventory(&self) -> Vec<serde_json::Value> {
        self.zen_egresses
            .as_ref()
            .map(|pool| {
                pool.egress_ids
                    .iter()
                    .enumerate()
                    .map(|(slot, egress_id)| {
                        serde_json::json!({
                            "slot": slot,
                            "egress_id": egress_id,
                            "rotating": pool.is_in_maintenance(egress_id),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
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
pub(super) const DRIFT_DETECTOR_CAPACITY: usize = 1000;

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
    /// Provider upstream client. A provider-only proxy is scoped to
    /// this client so routing never leaks into the process environment
    /// (which tool executions inherit). HTTP/2 is disabled when a proxy
    /// is set so HTTPS provider APIs tunnel through a CONNECT proxy
    /// instead of failing ALPN negotiation through it. Both HTTP/1.1
    /// and HTTP/2 negotiated via ALPN otherwise.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn build_upstream_client(config: &Config) -> Result<reqwest::Client, ProxyError> {
        Self::build_client_for_proxy(config, config.http_proxy.as_deref())
    }

    pub(super) fn build_client_for_proxy(
        config: &Config,
        proxy_url: Option<&str>,
    ) -> Result<reqwest::Client, ProxyError> {
        let mut client_builder = upstream_client_builder(config);
        // Provider-only proxy: scoped to this upstream client so
        // routing never leaks into the process environment (which tool
        // executions inherit). HTTP/2 is disabled when a proxy is set so
        // HTTPS provider APIs tunnel through a CONNECT proxy instead of
        // failing ALPN negotiation through it.
        if let Some(proxy_url) = proxy_url {
            let proxy = reqwest::Proxy::all(proxy_url).map_err(|_| {
                ProxyError::Config(
                    "invalid provider proxy URL (the configured URL is omitted because it may contain credentials)".to_string(),
                )
            })?;
            client_builder = client_builder.proxy(proxy).http1_only();
            tracing::info!(
                event = "provider_http_proxy_configured",
                "provider upstream calls routed through a proxy (HTTP/2 disabled)"
            );
        }
        // Both HTTP/1.1 and HTTP/2 negotiated via ALPN (unless a proxy
        // forced HTTP/1.1 above).
        client_builder.build().map_err(ProxyError::Upstream)
    }

    pub(super) fn build_zen_egresses(
        config: &Config,
    ) -> Result<Option<ProviderEgressPool>, ProxyError> {
        let mut clients = Vec::with_capacity(config.zen_http_proxy_pool.len());
        let mut egress_ids = Vec::with_capacity(config.zen_http_proxy_pool.len());
        let mut seen = std::collections::HashSet::new();
        for proxy_url in &config.zen_http_proxy_pool {
            if !seen.insert(proxy_url) {
                continue;
            }
            clients.push(Self::build_client_for_proxy(config, Some(proxy_url))?);
            egress_ids.push(provider_egress_id(Some(proxy_url)));
        }
        if clients.is_empty() {
            Ok(None)
        } else {
            tracing::info!(
                event = "zen_egress_pool_configured",
                egress_count = clients.len(),
                "routed provider requests will use sticky per-stream egress assignment"
            );
            Ok(Some(ProviderEgressPool::new(clients, egress_ids)))
        }
    }

    /// One registry dir for the per-project CTX stores, shared by capture,
    /// offload and recall so all three read and write the same file for a
    /// given project. Nothing is opened here — the project is a property of
    /// a request, not of the process, so handles are opened on first sight.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn resolve_ctx_base(config: &Config) -> Option<std::path::PathBuf> {
        if !(config.ctx_capture || config.ctx_offload || config.ctx_inject) {
            return None;
        }
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
    }

    /// CTX-2: construct the passive-capture observer only when enabled.
    /// A failure to spawn the worker is logged loudly and disables capture
    /// (a broken observer must never take down the proxy) — the request path
    /// is unaffected either way.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn start_ctx_observer(
        config: &Config,
        ctx_stores: &Option<Arc<crate::ctx::projects::ProjectStores>>,
    ) -> Option<Arc<crate::ctx::observer::CtxObserver>> {
        match (config.ctx_capture, ctx_stores.clone()) {
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
        }
    }

    /// CTX-3: construct the offload runtime only when enabled. Independent
    /// of `ctx_capture` — offload is its own flag. A failure to open the CCR
    /// store is logged loudly and disables offload (a broken sink must never
    /// take down the proxy).
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn start_ctx_offload(
        config: &Config,
        ctx_base: &Option<std::path::PathBuf>,
        ctx_stores: &Option<Arc<crate::ctx::projects::ProjectStores>>,
    ) -> Option<CtxOffloadRuntime> {
        if !config.ctx_offload {
            return None;
        }
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
                            cross_session_seed: config.ctx_offload_cross_session_seed,
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
    }

    /// CTX-4: recall/resume injection. Requires ctx_capture (the identity +
    /// sessions layer) — enforce loudly, no silent dependency. Shares the
    /// store registry with capture and offload, so it recalls from the same
    /// per-project files those two write.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn build_ctx_inject(
        config: &Config,
        ctx_observer: &Option<Arc<crate::ctx::observer::CtxObserver>>,
        ctx_stores: &Option<Arc<crate::ctx::projects::ProjectStores>>,
    ) -> Result<Option<Arc<crate::ctx::inject::InjectEngine>>, ProxyError> {
        if !config.ctx_inject {
            return Ok(None);
        }
        if !config.ctx_capture {
            return Err(ProxyError::Config(
                "--ctx-inject requires --ctx-capture (the sessions/identity layer); \
                 enable ctx_capture or disable ctx_inject"
                    .to_string(),
            ));
        }
        match (ctx_observer.as_ref(), ctx_stores.clone()) {
            (Some(_), Some(stores)) => Ok(Some(Arc::new(crate::ctx::inject::InjectEngine::new(
                stores,
            )))),
            _ => {
                // ctx_capture was on but capture failed to start; without
                // the sessions layer injection cannot run. Log and disable.
                tracing::warn!(
                    event = "ctx_inject_no_observer",
                    "CTX-4 injection disabled: sessions observer unavailable"
                );
                Ok(None)
            }
        }
    }

    /// CCR proactive-expansion tracker, sharing the offload flag.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn build_ccr_tracker(
        config: &Config,
    ) -> Option<Arc<Mutex<headroom_core::ccr::context_tracker::ContextTracker>>> {
        if !(config.ctx_offload && config.ccr_context_tracking) {
            return None;
        }
        Some(Arc::new(Mutex::new(
            headroom_core::ccr::context_tracker::ContextTracker::new(Some(
                headroom_core::ccr::context_tracker::ContextTrackerConfig {
                    proactive_expansion: config.ccr_proactive_expansion,
                    max_proactive_expansions: config.ccr_max_proactive_expansions,
                    ..Default::default()
                },
            )),
        )))
    }

    /// Attach the memory backend: FTS5-backed store preferred, in-memory
    /// fallback when no store dir resolves or opening fails.
    /// Extracted from `build_memory_handler` without behavior change.
    pub(super) fn attach_memory_backend(
        handler: &mut crate::memory::handler::MemoryHandler,
        memory_dir: &Option<std::path::PathBuf>,
    ) {
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
    }

    /// Memory handler with the FTS5-backed store preferred: BM25 with
    /// stemming, and memories that survive a restart. The in-memory
    /// backend it replaces scored by counting overlapping words and lost
    /// everything on exit; it stays as the fallback for the case where
    /// no store dir resolves, because a degraded memory beats none.
    /// Extracted from `AppState::new` without behavior change.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_memory_handler(
        memory_enabled: bool,
        memory_inject_tools: bool,
        memory_inject_context: bool,
        memory_top_k: usize,
        memory_min_similarity: f64,
        memory_mode: &str,
        memory_use_native_tool: bool,
        ctx_store_dir: &Option<std::path::PathBuf>,
    ) -> Option<Arc<crate::memory::handler::MemoryHandler>> {
        if !memory_enabled {
            return None;
        }
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
        let mut handler = crate::memory::handler::MemoryHandler::new(handler_config, "rust-proxy");
        // Prefer the FTS5-backed store: BM25 with stemming, and memories
        // that survive a restart. The in-memory backend it replaces scored
        // by counting overlapping words and lost everything on exit; it
        // stays as the fallback for the case where no store dir resolves,
        // because a degraded memory beats none.
        let memory_dir = ctx_store_dir
            .clone()
            .or_else(headroom_core::ctx::default_base_dir)
            .map(|base| base.join("memory"));
        Self::attach_memory_backend(&mut handler, &memory_dir);
        Some(Arc::new(handler))
    }

    /// Prefix-replay store, with the offload-gate adoption hook: a prefix
    /// adopted from another session carries that session's offload
    /// digests, so the gate must learn them under the adopter's key too.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn build_replay_store(
        config: &Config,
        ctx_offload: &Option<CtxOffloadRuntime>,
    ) -> SessionReplayStore {
        let mut replay_store = if config.replay_store_dir.is_empty() {
            SessionReplayStore::new(REPLAY_STORE_CAPACITY)
        } else {
            SessionReplayStore::with_persistence(
                REPLAY_STORE_CAPACITY,
                std::path::PathBuf::from(&config.replay_store_dir),
            )
        };
        // A prefix adopted from another session carries that session's offload
        // digests, so the gate must learn them under the adopter's key too.
        if let Some(runtime) = ctx_offload.as_ref() {
            let gate = runtime.gate.clone();
            replay_store.set_adoption_hook(Arc::new(move |donor, session_key| {
                gate.adopt_from(donor, session_key)
            }));
        }
        replay_store
    }

    /// Background compression worker flag chain.
    /// Extracted from `AppState::new` without behavior change.
    pub(super) fn build_background_compressor(
    ) -> Option<Arc<crate::background_compression::BackgroundCompressor>> {
        std::env::var("HEADROOM_BACKGROUND_COMPRESSION")
            .ok()
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false)
            .then(|| {
                let min_tokens: usize = std::env::var("HEADROOM_BACKGROUND_COMPRESSION_MIN_TOKENS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50_000);
                let _ = min_tokens; // used by callers, not here
                Arc::new(crate::background_compression::BackgroundCompressor::new(10))
            })
    }

    pub fn new(mut config: Config) -> Result<Self, ProxyError> {
        let client = Self::build_upstream_client(&config)?;
        let zen_egresses = Self::build_zen_egresses(&config)?;
        let default_egress_id = provider_egress_id(config.http_proxy.as_deref());
        // Runtime clients retain their proxy config internally. Do not keep
        // pool URLs (which may carry credentials) in Config/Debug output.
        config.zen_http_proxy_pool.clear();

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
        // given project.
        let ctx_base = Self::resolve_ctx_base(&config);
        let ctx_stores = ctx_base
            .clone()
            .map(|dir| Arc::new(crate::ctx::projects::ProjectStores::new(dir)));

        // CTX-2: construct the passive-capture observer only when enabled.
        let ctx_observer = Self::start_ctx_observer(&config, &ctx_stores);

        // CTX-3: construct the offload runtime only when enabled.
        let ctx_offload = Self::start_ctx_offload(&config, &ctx_base, &ctx_stores);

        // CTX-4: recall/resume injection. Requires ctx_capture — enforced
        // loudly, no silent dependency.
        let ctx_inject = Self::build_ctx_inject(&config, &ctx_observer, &ctx_stores)?;

        let ccr_context_tracker = Self::build_ccr_tracker(&config);

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

        let memory_handler = Self::build_memory_handler(
            memory_enabled,
            memory_inject_tools,
            memory_inject_context,
            memory_top_k,
            memory_min_similarity,
            &memory_mode,
            memory_use_native_tool,
            &config.ctx_store_dir,
        );

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

        let replay_store = Self::build_replay_store(&config, &ctx_offload);

        // Read before `config` moves into the Arc below.
        let observed_cache_ttl = if config.force_1h_cache_ttl || config.split_cache_ttl {
            cache_stabilization::usage_observer::ANTHROPIC_CACHE_TTL_1H
        } else {
            cache_stabilization::usage_observer::ANTHROPIC_CACHE_TTL
        };
        let stampede_gate = cache_stabilization::prefix_stampede::PrefixStampedeGate::new(
            REPLAY_STORE_CAPACITY,
            config.cache_stampede_wait_cap,
            observed_cache_ttl,
        );

        Ok(Self {
            config: Arc::new(config),
            client,
            zen_egresses: zen_egresses.map(Arc::new),
            default_egress_id,
            caller_clients: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(CALLER_CLIENT_CACHE_CAPACITY).expect("non-zero capacity"),
            ))),
            bedrock_credentials: None,
            drift_state: DriftState::new(DRIFT_DETECTOR_CAPACITY),
            outbound_drift_state: DriftState::new(DRIFT_DETECTOR_CAPACITY),
            tool_order_state: cache_stabilization::tool_order::ToolOrderStore::default(),
            roster_pin_state: cache_stabilization::tool_roster_pin::RosterPinStore::default(),
            redact_store: crate::redact::RedactStore::new(),
            replay_store,
            working_dir_pins: cache_stabilization::working_dir::WorkingDirPins::new(
                REPLAY_STORE_CAPACITY,
            ),
            role_sentence_pins: cache_stabilization::role_sentence::RoleSentencePins::new(
                REPLAY_STORE_CAPACITY,
            ),
            stampede_gate,
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
            cursor_bridge: Arc::new(crate::cursor::bridge::Bridge::new()),
            model_route_cooldowns: crate::model_router::ModelCooldowns::default(),
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
            // Canonical name matches upstream (`HEADROOM_PROXY_…`) and the
            // module const; the legacy `HEADROOM_TRUSTED_GATEWAY_CIDRS`
            // spelling is honored as a fallback so an existing export keeps
            // working. Was reading only the legacy name before, which
            // disagreed with both the const and upstream.
            trusted_gateway_cidrs: std::env::var(
                crate::forwarded_headers::TRUSTED_GATEWAY_CIDRS_ENV,
            )
            .or_else(|_| std::env::var("HEADROOM_TRUSTED_GATEWAY_CIDRS"))
            .ok()
            .and_then(|v| crate::forwarded_headers::load_trusted_gateway_cidrs(&v).ok())
            .unwrap_or_default(),
            // Fail loudly on a malformed dashboard allow-list: silently
            // yielding an empty list would turn a typo into "remote
            // dashboards stop working" with no signal (or worse, an
            // operator "fix" that widens access elsewhere).
            trusted_dashboard_client_cidrs: match std::env::var(
                crate::forwarded_headers::TRUSTED_DASHBOARD_CLIENT_CIDRS_ENV,
            ) {
                Err(_) => Vec::new(),
                Ok(raw) => crate::forwarded_headers::load_trusted_dashboard_client_cidrs(&raw)
                    .map_err(crate::error::ProxyError::Config)?,
            },
            background_compressor: Self::build_background_compressor(),
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
