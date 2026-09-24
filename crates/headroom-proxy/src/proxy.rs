//! Core reverse-proxy router and HTTP forwarding handler.

mod ccr_response;
mod forward;
mod sse_anthropic;
mod sse_openai;

use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use url::Url;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, Request, Response, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::Router;
#[cfg(test)]
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt};

use crate::cache_stabilization;
use crate::cache_stabilization::beta_sticky::BetaProvider;
use crate::cache_stabilization::drift_detector::{
    compute_structural_hash, derive_session_key, observe_drift_with_birth, stream_lane_key,
    ApiKind, DriftState,
};
use crate::cache_stabilization::prefix_replay::{SessionReplayStore, REPLAY_STORE_CAPACITY};
use crate::compression;
use crate::config::Config;
use crate::error::ProxyError;
use crate::headers::{build_forward_request_headers, filter_response_headers};
use crate::health::{health, healthz, healthz_upstream, livez, readyz, rollout_status};
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
const DRIFT_DETECTOR_CAPACITY: usize = 1000;

/// Maximum number of messages allowed in a request body.
/// Mirrors Python's `MAX_MESSAGE_ARRAY_LENGTH = 10_000`.
const MAX_MESSAGE_ARRAY_LENGTH: usize = 10_000;

/// Extra attempts for a CCR continuation before giving up on the retrieval.
/// The content is already fetched by this point, so the only thing a failure
/// costs is the model's answer; three attempts covers the overload bursts that
/// produced every observed continuation failure.
const CCR_CONTINUATION_RETRIES: u32 = 2;

/// Ceiling on waiting for a continuation round's response headers, per
/// attempt. The shared client timeout (600s, sized for streams) cannot see a
/// stalled headers wait: measured 2026-09-09, one round hung 43s/31s/26s
/// across its three attempts on flaky egress while the 600s bound sat
/// untouched, holding the client's turn 107s for a retrieval that died.
/// `.send()` resolves at response headers, so on a streamed continuation this
/// cannot cut a slow model short — the body arrives after, bounded by
/// [`CCR_CONTINUATION_IDLE_TIMEOUT`]. A headers wait past this is a stall, not
/// thinking; fail it fast so the retry can actually help instead of re-waiting
/// the same stall.
///
/// On a *buffered* continuation the two are the same wait, and this is a bound
/// on generation: a routed chat-completions backend can still lose a slow round
/// here. Anthropic continuations stream for exactly that reason (see
/// `streamed_continuation_request`); the routed shapes need their own SSE fold
/// before they can follow.
const CCR_CONTINUATION_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Ceiling on the silence between two chunks of a streamed continuation's
/// response body. The generation lives in that body, so the only bound that
/// does not cut a slow model short is one on silence: Anthropic pings while it
/// thinks, so a gap this long is a dead connection rather than a long one. A
/// buffered continuation arrives in one chunk and never waits on this.
const CCR_CONTINUATION_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Short classification of a reqwest transport failure for log lines.
/// reqwest's Display names the URL but not the phase; without this every
/// continuation stall reads identically and the next one is undebuggable
/// the same way.
fn ccr_transport_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_builder() {
        "builder"
    } else if e.is_body() {
        "body"
    } else if e.is_request() {
        "request"
    } else if e.is_decode() {
        "decode"
    } else {
        "unknown"
    }
}

/// The source chain behind a reqwest error, outermost first, length-capped.
/// This is where the actual cause lives (hyper: connection closed early,
/// TLS alert, DNS) — Display alone never shows it.
fn ccr_error_chain(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        parts.push(s.to_string());
        if parts.len() >= 4 {
            break;
        }
        source = s.source();
    }
    let joined = parts.join(" <- ");
    joined.chars().take(500).collect()
}
const CALLER_CLIENT_CACHE_CAPACITY: usize = 128;

/// Client, slot, egress ID and in-flight guard for one routed Zen send.
pub(crate) type ZenEgressSelection<'a> = (
    &'a reqwest::Client,
    usize,
    &'a str,
    Option<EgressInflightGuard>,
);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CallerClientKey {
    host: String,
    addresses: Vec<SocketAddr>,
}

/// Provider-only transports assigned once per stream lane. First-seen lanes
/// cycle across the configured egresses, which gives a fan-out of N streams
/// N distinct egresses when at least N are configured; later turns stay on
/// their assigned egress. The bounded map retains affinity for active work
/// without growing forever on a long-lived proxy.
pub(crate) struct ProviderEgressPool {
    clients: Vec<reqwest::Client>,
    egress_ids: Vec<String>,
    assignments: Mutex<ProviderEgressAssignments>,
    maintenance: Mutex<std::collections::HashSet<String>>,
    /// Requests holding each egress, indexed like `clients`. Exposed on
    /// `/debug/inflight` as `egress_in_flight` so a rotation drains only the
    /// lane it rotates instead of waiting for the whole proxy to go idle.
    in_flight: Vec<std::sync::atomic::AtomicUsize>,
}

struct ProviderEgressAssignments {
    lanes: lru::LruCache<String, usize>,
    next_slot: usize,
}

impl ProviderEgressPool {
    pub(crate) fn new(clients: Vec<reqwest::Client>, egress_ids: Vec<String>) -> Self {
        debug_assert!(!clients.is_empty());
        debug_assert_eq!(clients.len(), egress_ids.len());
        Self {
            clients,
            assignments: Mutex::new(ProviderEgressAssignments {
                lanes: lru::LruCache::new(NonZeroUsize::new(4096).expect("non-zero capacity")),
                next_slot: 0,
            }),
            maintenance: Mutex::new(std::collections::HashSet::new()),
            in_flight: (0..egress_ids.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            egress_ids,
        }
    }

    fn slot_for_lane(&self, lane_key: &str) -> usize {
        if lane_key.is_empty() {
            return 0;
        }
        let lane_fingerprint = hex::encode(Sha256::digest(lane_key.as_bytes()));
        let mut assignments = self.assignments.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&slot) = assignments.lanes.get(&lane_fingerprint) {
            return slot;
        }
        let slot = assignments.next_slot % self.clients.len();
        assignments.next_slot = assignments.next_slot.wrapping_add(1);
        assignments.lanes.put(lane_fingerprint, slot);
        slot
    }

    pub(crate) fn set_maintenance(&self, egress_id: &str, rotating: bool) -> bool {
        if !self.egress_ids.iter().any(|id| id == egress_id) {
            return false;
        }
        let mut maintenance = self.maintenance.lock().unwrap_or_else(|e| e.into_inner());
        if rotating {
            maintenance.insert(egress_id.to_owned());
        } else {
            maintenance.remove(egress_id);
        }
        true
    }

    fn is_in_maintenance(&self, egress_id: &str) -> bool {
        self.maintenance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(egress_id)
    }

    /// Count one request onto `slot`, or return its egress ID if the egress
    /// is rotating. The check and the increment happen under the lock
    /// `set_maintenance` takes, so once that call returns, every request it
    /// did not turn away is already counted and a drain that reads the count
    /// afterwards cannot miss it.
    fn acquire(self: &Arc<Self>, slot: usize) -> Result<EgressInflightGuard, String> {
        let egress_id = &self.egress_ids[slot];
        let maintenance = self.maintenance.lock().unwrap_or_else(|e| e.into_inner());
        if maintenance.contains(egress_id) {
            return Err(egress_id.clone());
        }
        self.in_flight[slot].fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(maintenance);
        Ok(EgressInflightGuard {
            pool: Arc::clone(self),
            slot,
        })
    }

    fn in_flight_by_egress(&self) -> serde_json::Map<String, serde_json::Value> {
        self.egress_ids
            .iter()
            .zip(&self.in_flight)
            .map(|(egress_id, count)| {
                (
                    egress_id.clone(),
                    count.load(std::sync::atomic::Ordering::SeqCst).into(),
                )
            })
            .collect()
    }
}

/// One request counted against one Zen egress. It goes with the response
/// body (see [`attach_egress_guard`]), so the count covers the whole turn up
/// to the last upstream byte, and drops early on error or client disconnect.
pub(crate) struct EgressInflightGuard {
    pool: Arc<ProviderEgressPool>,
    slot: usize,
}

impl Drop for EgressInflightGuard {
    fn drop(&mut self) {
        self.pool.in_flight[self.slot].fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pin_project_lite::pin_project! {
    /// Response body stream that releases its egress guard at the end of the
    /// body or on the first error, rather than whenever the consumer drops it.
    struct EgressGuardedStream<S> {
        #[pin]
        inner: S,
        guard: Option<EgressInflightGuard>,
    }
}

impl<S, T, E> futures_util::Stream for EgressGuardedStream<S>
where
    S: futures_util::Stream<Item = Result<T, E>>,
{
    type Item = Result<T, E>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.project();
        let item = futures_util::ready!(this.inner.poll_next(cx));
        if !matches!(item, Some(Ok(_))) {
            this.guard.take();
        }
        std::task::Poll::Ready(item)
    }
}

/// Move `guard` into `resp`'s body. Every consumer of a routed Zen response
/// (streamed, buffered, or dropped on an error arm) then holds the egress for
/// exactly as long as it holds the upstream body, with no plumbing per arm.
pub(crate) fn attach_egress_guard(
    resp: reqwest::Response,
    guard: Option<EgressInflightGuard>,
) -> reqwest::Response {
    let Some(guard) = guard else {
        return resp;
    };
    use http_body_util::BodyExt;
    use reqwest::ResponseBuilderExt;
    let url = resp.url().clone();
    let (parts, body) = http::Response::<reqwest::Body>::from(resp).into_parts();
    let body = reqwest::Body::wrap_stream(EgressGuardedStream {
        inner: body.into_data_stream(),
        guard: Some(guard),
    });
    let mut builder = http::Response::builder()
        .status(parts.status)
        .version(parts.version)
        .url(url);
    if let Some(headers) = builder.headers_mut() {
        *headers = parts.headers;
    }
    if let Some(extensions) = builder.extensions_mut() {
        extensions.extend(parts.extensions);
    }
    let rebuilt = builder
        .body(body)
        .expect("parts copied from a valid response");
    reqwest::Response::from(rebuilt)
}

fn provider_egress_id(proxy_url: Option<&str>) -> String {
    let Some(proxy_url) = proxy_url else {
        return "direct".to_string();
    };
    let digest = Sha256::digest(proxy_url.as_bytes());
    format!("proxy-{}", hex::encode(&digest[..6]))
}

/// Transport settings shared by trusted and caller-selected upstreams.
///
/// Caller-selected destinations add a pinned DNS override and disable proxies
/// below, but retain the same TLS roots, timeouts, keepalives, redirect policy,
/// and response behavior as the normal upstream client.
fn upstream_client_builder(config: &Config) -> reqwest::ClientBuilder {
    let builder = crate::ssl_context::client_builder()
        .connect_timeout(config.upstream_connect_timeout)
        // End-to-end bound on a single upstream request, streamed body
        // included. The send is bounded separately, below.
        .timeout(config.upstream_timeout)
        // Upstream redirects are forwarded to the client. In particular, a
        // caller-selected public endpoint cannot redirect this process into a
        // private network.
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(config.pool_idle_timeout)
        .http2_keep_alive_interval(std::time::Duration::from_secs(20))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .tcp_keepalive(std::time::Duration::from_secs(20));
    apply_upstream_write_timeout(builder, config)
}

/// Apply `config.upstream_write_timeout` (port of Python
/// `ProxyConfig.write_timeout_seconds`, upstream a507249b).
///
/// httpx bounds the send phase on its own; reqwest 0.12 has no per-phase
/// write knob, and both knobs it does have are the wrong phase — `timeout`
/// and `read_timeout` cover the wait for the answer, so setting either to
/// the write bound would kill a model that thinks longer than it. Linux's
/// `TCP_USER_TIMEOUT` bounds exactly what httpx's `write` bounds: outbound
/// bytes the peer never acknowledges, including a zero-window stall. Think
/// time is unaffected, because a thinking peer has already acked the
/// request. This replaces reqwest's own 30s default with the operator knob.
#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
fn apply_upstream_write_timeout(
    builder: reqwest::ClientBuilder,
    config: &Config,
) -> reqwest::ClientBuilder {
    builder.tcp_user_timeout(config.upstream_write_timeout)
}

/// No `TCP_USER_TIMEOUT` off Linux: the send stays under the total
/// `upstream_timeout`, as it did before the knob existed.
#[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
fn apply_upstream_write_timeout(
    builder: reqwest::ClientBuilder,
    _config: &Config,
) -> reqwest::ClientBuilder {
    builder
}

/// Build the request-scoped transport for a caller-selected upstream.
///
/// `resolve_to_addrs` preserves the URL hostname for Host/SNI while forcing
/// reqwest's connector to use the already-validated addresses. `no_proxy`
/// prevents an ambient or provider proxy from resolving the target a second
/// time beyond this process's policy boundary.
fn caller_upstream_client(
    state: &AppState,
    upstream: &crate::upstream_guard::ResolvedCallerUpstream,
) -> Result<reqwest::Client, ProxyError> {
    let mut key_addresses = upstream.addresses().to_vec();
    key_addresses.sort_unstable();
    key_addresses.dedup();
    let key = CallerClientKey {
        host: upstream.host().to_string(),
        addresses: key_addresses,
    };
    // Fast path under a short critical section: never hold the mutex
    // across `Client::build()` (TLS/pool setup, potentially ms). Two
    // concurrent misses may both build; `put` is idempotent so the
    // loser simply overwrites with an equivalent client.
    if let Some(client) = state
        .caller_clients
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
    {
        return Ok(client);
    }

    let client = upstream_client_builder(&state.config)
        .no_proxy()
        .resolve_to_addrs(upstream.host(), upstream.addresses())
        .build()
        .map_err(ProxyError::Upstream)?;
    state
        .caller_clients
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .put(key, client.clone());
    Ok(client)
}

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
    fn build_upstream_client(config: &Config) -> Result<reqwest::Client, ProxyError> {
        Self::build_client_for_proxy(config, config.http_proxy.as_deref())
    }

    fn build_client_for_proxy(
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

    fn build_zen_egresses(config: &Config) -> Result<Option<ProviderEgressPool>, ProxyError> {
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
    fn resolve_ctx_base(config: &Config) -> Option<std::path::PathBuf> {
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
    fn start_ctx_observer(
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
    fn start_ctx_offload(
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
    fn build_ctx_inject(
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
    fn build_ccr_tracker(
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
    fn attach_memory_backend(
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
    fn build_memory_handler(
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
    fn build_replay_store(
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
    fn build_background_compressor(
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
        // Headline leg: tool-schema tokens never entered context (deferral /
        // hook shrink), additive to `tokens_saved`. Zero on turns without
        // deferral, so every existing number reproduces exactly there.
        let tool_schema_saved =
            crate::tool_schema_savings::tool_schema_saved_from_tags(&outcome.tags).max(0);
        let rec = headroom_core::savings_tracker::RequestRecord {
            model: &outcome.model,
            // `original_tokens` is a savings baseline. Cost and usage must
            // instead follow the request Anthropic received after every proxy
            // transform, as reported in the response usage breakdown.
            input_tokens: billed_input_tokens,
            tokens_saved: outcome.tokens_saved,
            tool_schema_saved,
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
            // Router offload: the bill the requested model never saw because
            // the turn was served cheaper. Same value the ledger books as the
            // `model_offload` line; the tracker accumulates it into lifetime
            // totals and `savings_percent`.
            offload_savings_usd: offload_savings(outcome).map(|o| o.saved_usd).unwrap_or(0.0),
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
        // Split counter for the headline leg; `tokens_saved_total` above
        // keeps its meaning so existing PromQL is unaffected.
        crate::observability::proxy_counters::record_tool_schema_saved(
            tool_schema_saved.max(0) as u64
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
            tool_schema_saved: crate::tool_schema_savings::tool_schema_saved_from_tags(
                &outcome.tags,
            )
            .max(0),
            tokens_sent: provider_billed_input_tokens(outcome),
            cache_read_tokens: outcome.cache_read_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            cache_write_5m_tokens: outcome.cache_write_5m_tokens,
            cache_write_1h_tokens: outcome.cache_write_1h_tokens,
            uncached_tokens: outcome.uncached_input_tokens,
            output_tokens: outcome.output_tokens,
            cache_inferred: outcome.cache_inferred,
        };
        self.cost_tracker.record_tokens(&outcome.model, &rec);
    }

    fn log_request(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        let entry = crate::request_logger::RequestLogEntry::from_outcome(outcome);
        self.request_logger.log(entry);
    }

    fn record_output_savings(&self, transforms: &[String], output_tokens: i64) {
        // Every `flush_every`th call writes the ledger to disk, and this runs on
        // the task handling the request. `std::fs::write` + `rename` on a tokio
        // worker stalls every other request that thread is driving, so hand it
        // to the blocking pool the way `record_savings_ledger` already does.
        let transforms = transforms.to_vec();
        tokio::task::spawn_blocking(move || {
            headroom_core::output_savings::get_recorder()
                .record_from_labels(&transforms, output_tokens);
        });
    }

    fn record_failed(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        crate::observability::proxy_counters::record_failed(&outcome.provider);
        self.savings_tracker.record_failed_work(
            &headroom_core::savings_tracker::FailedWorkRecord {
                provider: Some(outcome.provider.clone()),
                status_code: outcome.status_code,
                upstream_attempts: outcome.upstream_attempts,
                forwarded_tokens: outcome.optimized_tokens,
                provider_input_tokens: outcome.provider_input_tokens,
                provider_output_tokens: outcome.provider_output_tokens,
                timestamp: None,
            },
        );
    }

    fn record_rate_limited(&self, outcome: &headroom_core::request_outcome::RequestOutcome) {
        // source="upstream": this funnel only ever sees a 429 the provider
        // returned. Our own limiter rejects before a request is ever sent
        // and records source="headroom" from its own call site.
        crate::observability::proxy_counters::record_rate_limited("upstream");
        self.savings_tracker
            .record_rate_limited(Some(outcome.provider.as_str()));
        // A 429 is failed work too: it reached the failure bucket, not the
        // success funnel, and the ledger must see the wasted tokens.
        self.record_failed(outcome);
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
        // Headline: deferral/hook-shrink tags are additive savings the
        // message count never saw. The ledger helper gates on `> 0` itself,
        // so this stays a no-op for turns without savings.
        let saved =
            crate::tool_schema_savings::headline_tokens_saved(outcome.tokens_saved, &outcome.tags);
        let model = outcome.model.clone();
        let client = outcome.client.clone();
        // Price the booked (headline) count, not the message-only count, so
        // the ledger's token and dollar columns share one basis. The
        // cache-aware rate selection is unchanged.
        let priced_cost = outcome.compression_savings_cost_usd_for(saved);
        // `"free"` (zero-rate tier) makes the basis self-describing: the
        // counterfactual dollars below are 0.0 because the model costs
        // nothing, not because nothing was saved — do not read them
        // against priced rows.
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
        let offload = offload_savings(outcome);
        // Widening the gate above brought turns here that never reached this
        // line before, including ones booked outside a runtime. `spawn_blocking`
        // panics off-runtime, and a ledger append must never be the thing that
        // takes a request down — write inline when there is no pool to hand it
        // to. The counterfactuals are already logged either way.
        if tokio::runtime::Handle::try_current().is_err() {
            write_savings_ledger(
                forwarded,
                saved,
                &model,
                client.as_deref(),
                priced_cost,
                &priced_basis,
                offload,
            );
            return;
        }
        tokio::task::spawn_blocking(move || {
            write_savings_ledger(
                forwarded,
                saved,
                &model,
                client.as_deref(),
                priced_cost,
                &priced_basis,
                offload,
            );
        });
    }
}

/// Append this turn's compression saving and, when the router moved it, the
/// bill its original model never saw. Takes a cross-process flock, so callers
/// hand it to the blocking pool when there is one.
#[allow(clippy::too_many_arguments)]
fn write_savings_ledger(
    forwarded: i64,
    saved: i64,
    model: &str,
    client: Option<&str>,
    priced_cost: f64,
    priced_basis: &str,
    offload: Option<OffloadSavings>,
) {
    headroom_core::savings_ledger::record_from_forwarded_with_cost(
        forwarded,
        saved,
        Some(model),
        client,
        Some(priced_cost),
        Some(priced_basis),
    );
    if let Some(offload) = offload {
        headroom_core::savings_ledger::record_savings_event(
            headroom_core::savings_ledger::SavingsEvent {
                tokens_before: offload.tokens,
                tokens_after: 0,
                model: Some(&offload.from_model),
                client: None,
                source: Some("proxy"),
                timestamp: None,
                cost_usd: Some(offload.saved_usd),
                cost_basis: Some("model_offload"),
                fallback_rate: None,
                path: None,
            },
        );
    }
}

/// What a rerouted turn saved by not running on the model the client asked
/// for: the tokens that model never billed, and the money they would have
/// cost there minus what they cost where the turn actually ran.
///
/// Both legs price through the same helper on the same token counts, so an
/// unknown model on either side cannot make them disagree — it moves both.
/// `uncached_input_tokens` rather than `optimized_tokens` because the
/// routed path folds the cached prefix into its input count, and adding
/// `cache_read_tokens` on top would bill that prefix twice.
///
/// `None` for a turn the client's own model served, and for a failed one:
/// a reroute that 502s saved nothing, it just cost the user a turn.
fn offload_savings(
    outcome: &headroom_core::request_outcome::RequestOutcome,
) -> Option<OffloadSavings> {
    let from_model = outcome.routed_from_model.clone()?;
    if !(200..300).contains(&outcome.status_code) {
        return None;
    }
    let fresh = outcome.uncached_input_tokens.max(0);
    let out = outcome.output_tokens.max(0);
    let read = outcome.cache_read_tokens.max(0);
    let write = outcome.cache_write_tokens.max(0);
    let tokens = fresh + out + read + write;
    if tokens <= 0 {
        return None;
    }
    // The counterfactual has to price the write at the TTL the provider
    // actually billed. The client's model would have been charged 2.0x input
    // on a 1-hour write against 1.25x on a 5-minute one, and the 1h tier is the
    // common case here, so pricing both legs flat at 5m understated what the
    // reroute avoided.
    let write_1h = outcome.cache_write_1h_tokens;
    let fallback = headroom_core::savings_ledger::DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN;
    let would_have_cost = headroom_core::pricing::estimate_cost_usd_split(
        &from_model,
        fresh,
        out,
        read,
        write,
        write_1h,
        fallback,
    );
    let did_cost = headroom_core::pricing::estimate_cost_usd_split(
        &outcome.model,
        fresh,
        out,
        read,
        write,
        write_1h,
        fallback,
    );
    let saved_usd = (would_have_cost - did_cost).max(0.0);
    tracing::info!(
        event = "model_offload_savings",
        request_id = %outcome.request_id,
        from_model = %from_model,
        to_model = %outcome.model,
        tokens,
        fresh_input_tokens = fresh,
        cache_read_tokens = read,
        cache_write_tokens = write,
        output_tokens = out,
        would_have_cost_usd = would_have_cost,
        did_cost_usd = did_cost,
        saved_usd,
        "turn served off the client's model: tokens and cost it never billed"
    );
    Some(OffloadSavings {
        from_model,
        tokens,
        saved_usd,
    })
}

/// One rerouted turn's contribution to the ledger, computed on the request
/// thread and moved to the blocking pool with the compression entry.
struct OffloadSavings {
    from_model: String,
    tokens: i64,
    saved_usd: f64,
}

/// Build the axum app. `/healthz`, `/healthz/upstream`, `/livez`, `/readyz`,
/// and `/health` are intercepted; everything else hits the catch-all
/// forwarder. WebSocket upgrades are handled inside the catch-all handler
/// when an `Upgrade: websocket` header is present.
/// PR-D1: native AWS Bedrock InvokeModel routes. Mounts only when
/// `enable_bedrock_native` is on (default). Merged as a sub-router with
/// ONLY the Bedrock routes carrying the auth-mode layer, so it fires
/// before the handler runs and is scoped to these routes alone.
/// Extracted from `build_app` without behavior change.
fn mount_bedrock_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    // PR-D1: native AWS Bedrock InvokeModel route. Mounts only when
    // `enable_bedrock_native` is on (default). The handler runs the
    // live-zone compressor over Anthropic-shape bodies, signs with
    // SigV4, and forwards to the configured Bedrock endpoint. The
    // `/converse` route mounts the same handler — the wire shape is
    // identical for `anthropic.claude-*` model IDs (Bedrock just
    // accepts both legacy `invoke` and modern `converse` paths).
    if !state.config.enable_bedrock_native {
        tracing::warn!(
            event = "bedrock_native_disabled",
            "Bedrock native InvokeModel route disabled by \
             --enable-bedrock-native=false; Bedrock requests will fall \
             through to the catch-all (no SigV4 re-signing — fails closed)"
        );
        return router;
    }
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
    let router = router.merge(bedrock_router);
    if !state.config.bedrock_validate_eventstream_crc {
        tracing::warn!(
            event = "bedrock_eventstream_crc_validation_disabled",
            "Bedrock EventStream CRC validation is DISABLED — \
             only safe for debugging; production must keep \
             --bedrock-validate-eventstream-crc=true"
        );
    }
    router
}

/// PR-C4: Conversations API (passthrough-with-instrumentation).
/// The flag is read once at app-build time so router shape
/// matches the configured policy. When disabled, requests still
/// reach upstream via `catch_all`'s streaming forwarder, but the
/// per-route handlers (and their structured-log breadcrumbs) are
/// NOT mounted — operators flip the toggle to silence logs, not
/// to break the surface. The catch-all preserves byte equivalence.
/// Extracted from `build_app` without behavior change.
fn mount_conversation_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if !state.config.enable_conversations_passthrough {
        // Mirror the WARN we use elsewhere when a default-on guard
        // is flipped off. Logged at app-build time, not per-request.
        tracing::warn!(
            event = "conversations_passthrough_disabled",
            "Conversations API per-route handlers disabled by \
             --enable-conversations-passthrough=false; requests will \
             still reach upstream via the catch-all (no per-route logs)"
        );
        return router;
    }
    router
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
        )
}

/// Batch API routes. Gated on `enable_batch_api` — when disabled,
/// batch requests fall through to the catch-all (byte-equal passthrough).
/// NOTE: Google batch (`:batchGenerateContent`) is NOT registered here —
/// the Gemini dispatcher below owns `/v1beta/models/*model_action` and
/// delegates batch actions itself; registering it twice would panic axum
/// at startup ("Overlapping method route").
/// Extracted from `build_app` without behavior change.
fn mount_batch_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if !state.config.enable_batch_api {
        return router;
    }
    router
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
        )
}

/// Model routing: intercept /v1/messages only when a local model
/// or extra model routes are configured. When disabled, /v1/messages
/// falls through to the catch-all and streams normally (zero overhead).
/// Extracted from `build_app` without behavior change.
fn mount_model_routes(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if state.config.local_model.is_none() && state.config.model_routes.is_empty() {
        return router;
    }
    let router = router.route(
        "/v1/messages",
        post(crate::handlers::local_model::handle_messages),
    );
    // Token counting for translated routes: a routed alias forwarded
    // verbatim is unknown to Anthropic (404 + a non-JSON page that also
    // pollutes the upstream-health refusal window), so the handler
    // answers those locally. Unrouted models forward byte-identical and
    // keep the exact upstream count.
    let router = router.route(
        "/v1/messages/count_tokens",
        post(crate::handlers::count_tokens::handle_count_tokens),
    );
    // Gateway model discovery (CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1):
    // Claude Code queries this at startup to populate the /model picker
    // with routed models. See handlers::local_model::handle_models.
    router.route(
        "/v1/models",
        get(crate::handlers::local_model::handle_models),
    )
}

/// /debug/* endpoints: loopback-only introspection. The loopback guard
/// layer rejects non-loopback callers with 404 (not 403 — invisible to
/// external scanners). Matches Python's `require_loopback` dependency.
/// Extracted from `build_app` without behavior change.
fn mount_debug_routes(router: Router<AppState>) -> Router<AppState> {
    use axum::extract::ConnectInfo;
    use axum::http::StatusCode;
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

    async fn debug_active_conversations(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let conversations = state.usage_observer.active_conversations();
        axum::response::Json(
            crate::debug_introspection::serialize_active_conversations_debug(conversations),
        )
    }

    async fn debug_zen_egresses(
        axum::extract::State(state): axum::extract::State<AppState>,
    ) -> axum::response::Json<serde_json::Value> {
        let egresses = state.zen_egress_inventory();
        axum::response::Json(serde_json::json!({
            "pool_enabled": !egresses.is_empty(),
            "egresses": egresses,
        }))
    }

    async fn debug_zen_egress_maintenance(
        axum::extract::State(state): axum::extract::State<AppState>,
        axum::extract::Path(egress_id): axum::extract::Path<String>,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> Response {
        let Some(rotating) = body.get("rotating").and_then(serde_json::Value::as_bool) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if !state.set_zen_egress_maintenance(&egress_id, rotating) {
            return StatusCode::NOT_FOUND.into_response();
        }
        axum::Json(serde_json::json!({"ok": true, "rotating": rotating})).into_response()
    }

    let debug_router = Router::new()
        .route("/debug/tasks", get(debug_tasks))
        .route("/debug/ws-sessions", get(debug_ws_sessions))
        .route("/debug/warmup", get(debug_warmup))
        .route("/debug/zen-egresses", get(debug_zen_egresses))
        .route(
            "/debug/zen-egresses/{egress_id}/maintenance",
            post(debug_zen_egress_maintenance),
        )
        .route(
            "/debug/active-conversations",
            get(debug_active_conversations),
        )
        .route(
            "/debug/inflight",
            get(
                |axum::extract::State(state): axum::extract::State<AppState>| async move {
                    // `zen_held` turns are parked in the Zen 429 hold: no
                    // generation is running for them, so the rotation
                    // watcher's drain must not wait on them (they are
                    // waiting on it). `egress_in_flight` already leaves
                    // them out: a parked turn gives its egress back.
                    axum::response::Json(serde_json::json!({
                        "in_flight": InflightGuard::count_global(),
                        "zen_held": crate::routed::zen_hold::held_count(),
                        "egress_in_flight": state.zen_egress_in_flight(),
                    }))
                },
            ),
        )
        .layer(middleware::from_fn(loopback_guard));

    let router = router.merge(debug_router);

    // The MCP endpoint Cursor's agent calls back into, sharing the guard
    // above. It hands out whatever tools a conversation advertised and then
    // blocks on them, so it belongs behind the same loopback gate.
    //
    // Mounted unconditionally: it answers nothing for a conversation that
    // was never opened, and only a `cursor:` model route opens one, so an
    // installation not using Cursor carries a dead path and no state.
    router.merge(
        Router::new()
            .route(
                "/mcp/{conversation}",
                post(crate::cursor::endpoint::handle_mcp),
            )
            .layer(middleware::from_fn(loopback_guard)),
    )
}

pub fn build_app(state: AppState) -> Router {
    // Point `headroom-core`'s Kompress size gate at the Prometheus counter.
    // The core crate has no dependency on this one, so it reports through a
    // hook instead. First call wins; a second `build_app` (as in tests) is a
    // harmless no-op.
    headroom_core::transforms::observability::set_kompress_size_gate_hook(Box::new(|outcome| {
        crate::observability::proxy_counters::record_kompress_size_gate(outcome);
    }));

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/healthz/upstream", get(healthz_upstream))
        // Orchestrator probes (port of upstream `/livez` + `/readyz`).
        // Auth-exempt like `/healthz`, and mounted (not forwarded): before
        // this they fell through to the catch-all and leaked upstream.
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/health", get(health))
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
        // Same split-out rationale as /codex-limits: the statusline polls
        // this on every prompt for routed Spark models, whose own
        // `context_window` never arrives from Claude Code.
        .route(
            "/spark-context",
            get(crate::handlers::stats::handle_spark_context),
        )
        .route(
            "/stats/reset",
            post(crate::handlers::stats::handle_stats_reset),
        )
        .route(
            "/stats-history",
            get(crate::handlers::stats::handle_stats_history),
        )
        .route(
            "/stats-lifetime",
            get(crate::handlers::stats::handle_stats_lifetime),
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

    // PR-D1: native AWS Bedrock InvokeModel routes.
    let router = mount_bedrock_routes(router, &state);

    // PR-C4: Conversations API (passthrough-with-instrumentation).
    let router = mount_conversation_routes(router, &state);

    // Batch API routes.
    let router = mount_batch_routes(router, &state);

    // Gemini native API routes. These handle the Gemini-specific format
    // (contents[] with parts[], systemInstruction) and apply compression
    // via the OpenAI pipeline after format conversion.
    let router = router.route(
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
    let router = router.route(
        "/anthropic/v1/messages",
        post(crate::foundry::handle_foundry_messages),
    );

    // Model routing: intercept /v1/messages only when configured.
    let router = mount_model_routes(router, &state);

    // /debug/* endpoints: loopback-only introspection.
    let router = mount_debug_routes(router);

    // CTX-5/6: mount /ctx/* endpoints when the offload store is available.
    // The endpoints share the proxy's listener — no separate bind.
    let router = if state.config.ctx_offload {
        router.nest("/ctx", crate::ctx::endpoints::router())
    } else {
        router
    };

    // Everything below is layered AFTER the fallback is set, and the order is
    // load-bearing: `Router::layer` wraps the routes registered up to the call,
    // so a layer added above `fallback` does not see the catch-all at all.
    // `/v1/messages` is not a registered route — it falls through — so layering
    // earlier leaves the proxy's busiest path unwrapped.
    //
    // Innermost to outermost: identity strip, request counter, auth gate.
    let router = router.fallback(any(catch_all));

    // WEB-02: drop a caller-supplied memory identity unless the caller is on
    // loopback. Done here rather than at each reader so a new reader cannot
    // miss it — and the readers that matter (`handle_memory_response`, the
    // routed transforms) sit behind the catch-all.
    let router = router.layer(axum::middleware::from_fn(strip_untrusted_identity));

    // Count every inbound request, including ones that fall through to the
    // catch-all.
    let router = router.layer(axum::middleware::from_fn(track_inbound_request));

    // Raise the body cap for every route, not just Bedrock. axum defaults to
    // 2 MiB, and a conversation of ~100k tokens serializes past that, so the
    // proxy answered 413 "Failed to buffer the request body: length limit
    // exceeded" — which Claude Code reports as "Request too large (max 32MB).
    // Accumulated images and attachments...", blaming images that do not
    // exist. Observed 2026-09-14: bodies topping out at 1.90 MB in the log
    // with nothing bigger ever recorded, and 9 such 413s across 6 sessions.
    let router = router.layer(DefaultBodyLimit::max(state.config.max_body_bytes as usize));

    // Require `HEADROOM_PROXY_TOKEN` from non-loopback callers. Outermost, so
    // one gate covers both transports: a WebSocket upgrade arrives as an
    // ordinary HTTP GET and only becomes a socket inside the handler. A
    // rejected caller is not counted as proxy traffic, because it never
    // reached the proxy.
    router
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::proxy_auth::proxy_auth_gate,
        ))
        .with_state(state)
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
    // Codex Live call creation (port of upstream `53982aef`): a multipart
    // POST carrying `sdp` + `session` that the generic forwarder would relay
    // to the default upstream unconverted. The handler gates on headers
    // alone first and hands an unauthenticated request back with its body
    // unread, so the fall-through below forwards exactly what arrived.
    // Non-POST methods keep existing behaviour (the upgrade branch above
    // owns GET-with-upgrade; plain GET forwards as HTTP).
    if parts.method == http::Method::POST
        && crate::codex_live_http::is_live_call_path(parts.uri.path())
    {
        let request_id = ensure_request_id(&parts.headers);
        let inbound_path = parts.uri.path().to_string();
        match crate::codex_live_http::handle_live_call(
            &state.client,
            state.config.strip_internal_headers.is_enabled(),
            state.config.max_body_bytes as usize,
            &request_id,
            &inbound_path,
            parts,
            body,
        )
        .await
        {
            crate::codex_live_http::LiveHttpDecision::Handled(resp) => return resp,
            crate::codex_live_http::LiveHttpDecision::Fallthrough(p, b) => {
                let req = Request::from_parts(p, b);
                return forward_http(state, client_addr, req)
                    .await
                    .unwrap_or_else(|e| e.into_response());
            }
        }
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
    project_root_override: Option<&str>,
) -> Option<(String, Option<String>)> {
    let system_prompt = crate::memory::router::extract_system_prompt(body);
    let ctx = crate::memory::router::RequestContext {
        headers: header_map_to_lowercase_strings(headers),
        system_prompt,
        base_user_id: String::new(),
        project_root_override: project_root_override.map(str::to_string),
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
pub(crate) fn resolve_ctx_project(
    headers: Option<&HeaderMap>,
    body: &serde_json::Value,
    project_root_override: Option<&str>,
) -> String {
    let ctx = crate::memory::router::RequestContext {
        headers: header_map_to_lowercase_strings(headers),
        system_prompt: crate::memory::router::extract_system_prompt(body),
        base_user_id: String::new(),
        project_root_override: project_root_override.map(str::to_string),
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
/// CCR expansion pre-gates: non-empty query, flag on, not cache mode, and
/// not a compact-continuation summary (already context; expanding it
/// re-adds stale session state to later turns). Each gate logs its
/// distinct `ccr_expansion_evaluation` outcome.
/// Extracted from `check_ccr_expansion_prereqs` without behavior change.
fn expansion_gate_passes(state: &AppState, user_query: &str, request_id: &str) -> bool {
    if user_query.trim().is_empty()
        || !state.config.ccr_proactive_expansion
        || crate::modes::is_cache_mode(Some(&state.config.mode))
    {
        // Shadow signal for the expansion re-enable decision: how often the
        // gate alone keeps expansion out of play. `flag` separates the
        // switched-off volume from the rest.
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "skipped_gate",
            flag = state.config.ccr_proactive_expansion,
            "ccr: expansion gated out before consulting the tracker"
        );
        return false;
    }
    // Compact-continuation summaries are already context; expanding them
    // re-adds stale session state to later turns (port of upstream skipping
    // proactive tracking for `looks_like_claude_code_compact_summary`).
    if headroom_core::ccr::context_tracker::looks_like_compact_summary(&[user_query]) {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "compact_summary",
            "ccr: skipping proactive expansion for a compact-continuation summary"
        );
        return false;
    }
    true
}

/// Load the CCR tracker plus the offload runtime, both of which must be
/// present. Each miss logs its distinct `ccr_expansion_evaluation` outcome.
/// Extracted from `recommend_ccr_expansions` without behavior change.
fn ccr_tracker_and_runtime<'a>(
    state: &'a AppState,
    request_id: &str,
) -> Option<(
    &'a std::sync::Arc<std::sync::Mutex<headroom_core::ccr::context_tracker::ContextTracker>>,
    &'a CtxOffloadRuntime,
)> {
    let Some(tracker) = state.ccr_context_tracker.as_ref() else {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "no_tracker",
            "ccr: expansion has no context tracker to consult"
        );
        return None;
    };
    let Some(runtime) = state.ctx_offload.as_ref() else {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "no_runtime",
            "ccr: expansion has no offload runtime to fetch from"
        );
        return None;
    };
    Some((tracker, runtime))
}

/// Lock the tracker and analyze the query. A poisoned lock or an empty
/// recommendation set each log and decline.
/// Extracted from `recommend_ccr_expansions` without behavior change.
fn query_ccr_tracker(
    tracker: &std::sync::Arc<std::sync::Mutex<headroom_core::ccr::context_tracker::ContextTracker>>,
    user_query: &str,
    turn_number: u32,
    workspace_key: &str,
    request_id: &str,
) -> Option<Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>> {
    let mut guard = match tracker.lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!(
                event = "ccr_tracker_poisoned_proactive",
                request_id = %request_id,
                "CCR Phase 4: tracker mutex poisoned; skipping proactive expansion"
            );
            return None;
        }
    };
    let recommendations = guard.analyze_query(user_query, Some(turn_number), workspace_key);
    if recommendations.is_empty() {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "no_match",
            "ccr: tracker consulted, nothing relevant to expand"
        );
        return None;
    }
    Some(recommendations)
}

/// Consult the CCR tracker: both the tracker and the offload runtime must
/// be present, the lock must succeed, and the query must match something.
/// Each miss logs its distinct `ccr_expansion_evaluation` outcome.
/// Extracted from `check_ccr_expansion_prereqs` without behavior change.
fn recommend_ccr_expansions<'a>(
    state: &'a AppState,
    user_query: &str,
    turn_number: u32,
    workspace_key: &str,
    request_id: &str,
) -> Option<(
    &'a CtxOffloadRuntime,
    Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>,
)> {
    let (tracker, runtime) = ccr_tracker_and_runtime(state, request_id)?;
    let recommendations =
        query_ccr_tracker(tracker, user_query, turn_number, workspace_key, request_id)?;
    Some((runtime, recommendations))
}

/// CCR expansion gates: query/flag/cache-mode, compact-summary skip, and
/// the tracker + offload runtime both present and lockable. Each gate logs
/// its distinct `ccr_expansion_evaluation` outcome. Returns the runtime plus
/// the tracker's recommendations on success.
/// Extracted from `maybe_append_ccr_proactive_expansion` without behavior
/// change.
fn check_ccr_expansion_prereqs<'a>(
    state: &'a AppState,
    user_query: &str,
    turn_number: u32,
    workspace_key: &str,
    request_id: &str,
) -> Option<(
    &'a CtxOffloadRuntime,
    Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>,
)> {
    if !expansion_gate_passes(state, user_query, request_id) {
        return None;
    }
    recommend_ccr_expansions(state, user_query, turn_number, workspace_key, request_id)
}

/// Fetch the tracker's recommended contents from the CCR store, skipping
/// hashes the store no longer has.
/// Extracted from `maybe_append_ccr_proactive_expansion` without behavior
/// change.
fn fetch_expansion_contents(
    ccr: &std::sync::Arc<dyn headroom_core::ccr::CcrStore>,
    recommendations: Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>,
) -> Vec<headroom_core::ccr::context_tracker::ExpansionContent> {
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
    expansions
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
    let Some((runtime, recommendations)) =
        check_ccr_expansion_prereqs(state, user_query, turn_number, workspace_key, request_id)
    else {
        return false;
    };

    let ccr = runtime.store.ccr();
    let rec_count = recommendations.len();
    let expansions = fetch_expansion_contents(&ccr, recommendations);
    if expansions.is_empty() {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "store_miss",
            recs = rec_count,
            "ccr: tracker recommended content the store no longer has"
        );
        return false;
    }

    let expansion_text =
        headroom_core::ccr::context_tracker::ContextTracker::format_expansions_for_context(
            &expansions,
            workspace_label,
        );
    // Charge the shared budget. Expansion appends to the live tail, which is
    // re-sent every turn, so clipping it here is cache-safe.
    let expansion_bytes_uncapped = expansion_text.len() as u64;
    let Some(expansion_text) = budget.take(
        crate::injection_budget::InjectionStage::ProactiveExpansion,
        expansion_text,
    ) else {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "over_budget",
            bytes = expansion_bytes_uncapped,
            "ccr: expansion did not fit the injection budget"
        );
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
                event = "ccr_tracker_poisoned_tracking",
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

/// Attribution recorded when the tool-search stage ran on `tools[]`.
///
/// `mode` names who deferred: `client` when the client already sent the
/// server-side shape (stand-down — the deferral is real but not ours to
/// book, so a stand-down must not read as the feature being off),
/// `headroom` only when we actually deferred something, `none` otherwise.
pub(crate) struct ToolSearchAttribution {
    pub deferred_tools: usize,
    pub deferred_tokens: i64,
    /// Tokens of deferred tools that are core under the default set
    /// (disjoint slice of `deferred_tokens`, not additive to it).
    pub core_deferred_tokens: i64,
    pub stripped_third_party: usize,
    pub mode: &'static str,
}

/// Server-side tool-search deferral (+ third-party search-tool strip) for
/// Anthropic `/v1/messages`.
///
/// Port of the tools stages in upstream `handlers/anthropic.py`: on
/// third-party Anthropic-compatible upstreams, strip client-originated
/// first-party search tools (they reject that shape); on first-party
/// Anthropic with `HEADROOM_TOOL_SEARCH` on (default), defer non-core
/// schemas behind an injected search tool so they stop billing context.
///
/// Runs after pruning (which settles the tool set); compaction and the
/// cache-control stages below then see the final array, breakpoint move
/// included. Forwards the original bytes untouched on any parse/serialize
/// failure or when nothing changed, so no-op turns stay byte-identical.
pub(crate) fn maybe_inject_tool_search(
    body: bytes::Bytes,
    upstream_base_url: &str,
    model: &str,
    request_id: &str,
    enabled: bool,
) -> (bytes::Bytes, Option<ToolSearchAttribution>) {
    use crate::tool_search_deferral as tsd;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, None),
    };
    let Some(tools) = value.get("tools").and_then(|t| t.as_array()).cloned() else {
        return (body, None);
    };
    let custom = tsd::is_custom_anthropic_base_url(Some(upstream_base_url));

    // Third-party routes reject the first-party search shape: strip
    // client-originated search tools. Not env-gated — a poisoned transcript
    // must recover even with injection off.
    let client_defers = tsd::client_uses_tool_search(&tools);
    let mut tools_vec = tools;
    let mut stripped_third_party = 0usize;
    if custom {
        let strip = tsd::strip_for_third_party_upstream(tools_vec);
        stripped_third_party = strip.removed;
        tools_vec = strip.tools;
    }

    // Deferral injection: first-party only, env-gated. The input array is
    // handed back unchanged when injection doesn't apply.
    let mut deferred_tools = 0usize;
    let mut deferred_tokens = 0i64;
    let mut core_deferred_tokens = 0i64;
    if !custom && enabled {
        let inject = tsd::inject_deferral(tools_vec);
        if inject.changed {
            deferred_tools = inject.deferred.len();
            let tokenizer = headroom_core::tokenizer::get_tokenizer(model);
            let deferred_json = serde_json::to_string(&inject.deferred).unwrap_or_default();
            deferred_tokens = tokenizer.count_text(&deferred_json) as i64;
            let core_json = serde_json::to_string(&inject.core_deferred).unwrap_or_default();
            core_deferred_tokens = tokenizer.count_text(&core_json) as i64;
            tracing::info!(
                event = "tool_search_deferral",
                request_id = %request_id,
                deferred_tools = deferred_tools,
                deferred_tokens = deferred_tokens,
                core_deferred_tokens = core_deferred_tokens,
                "deferred non-core tool schemas behind the search tool"
            );
        }
        tools_vec = inject.tools;
    }

    // "headroom" only when we actually deferred something: injection also
    // declines on a small tool surface or when nothing is deferrable, and
    // calling that "headroom" would overstate our role exactly where we did
    // nothing.
    let mode = if client_defers {
        "client"
    } else if deferred_tools > 0 {
        "headroom"
    } else {
        "none"
    };
    if stripped_third_party == 0 && deferred_tools == 0 && !client_defers {
        return (body, None);
    }
    value["tools"] = serde_json::Value::Array(tools_vec);
    match serde_json::to_vec(&value) {
        Ok(bytes) => (
            bytes::Bytes::from(bytes),
            Some(ToolSearchAttribution {
                deferred_tools,
                deferred_tokens,
                core_deferred_tokens,
                stripped_third_party,
                mode,
            }),
        ),
        Err(_) => (body, None),
    }
}

/// Tool-search history repair (upstream #2805). Drops `server_tool_use` /
/// `tool_search_tool_result` blocks the final tools array cannot support:
/// side-requests replaying a transcript against a smaller tools array, or
/// transcripts poisoned while deferral was on.
///
/// Unconditional and last: runs after turn hooks (which may rewrite tools)
/// and after every other tools/messages mutator, validating against the
/// final outbound array. Returns the neutralized-block count; zero means the
/// original bytes are forwarded untouched.
pub(crate) fn maybe_repair_tool_search_history(
    body: bytes::Bytes,
    request_id: &str,
) -> (bytes::Bytes, usize) {
    use crate::tool_search_deferral as tsd;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
    };
    let tools: Vec<serde_json::Value> = value
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let messages: Vec<serde_json::Value> = match value.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return (body, 0),
    };
    let repair = tsd::strip_unsupported_blocks(messages, &tools);
    if repair.neutralized == 0 {
        return (body, 0);
    }
    value["messages"] = serde_json::Value::Array(repair.messages);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "tool_search_history_repair",
                request_id = %request_id,
                neutralized_blocks = repair.neutralized,
                "repaired tool-search history blocks the tools array cannot support (replaced with text in place)"
            );
            (bytes::Bytes::from(bytes), repair.neutralized)
        }
        Err(_) => (body, 0),
    }
}

/// CCR retrieve history repair (upstream #2814). Neutralizes
/// `headroom_retrieve` history references the outbound tools array cannot
/// support: a side-request forwarded without declaring the tool would
/// otherwise 400 on the historical `tool_use`.
///
/// Runs beside the tool-search repair, after both normal CCR injection and
/// turn hooks, validating against the final outbound tools array.
/// Neutralize-in-place (never drops messages) so user/assistant alternation
/// survives. Returns the neutralized-block count; zero means the original
/// bytes are forwarded untouched.
pub(crate) fn maybe_repair_ccr_retrieve_history(
    body: bytes::Bytes,
    request_id: &str,
) -> (bytes::Bytes, usize) {
    use crate::ccr_retrieve_repair as ccr;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
    };
    let tools: Vec<serde_json::Value> = value
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let messages: Vec<serde_json::Value> = match value.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return (body, 0),
    };
    let repair = ccr::strip_unsupported_ccr_blocks(messages, &tools);
    if repair.neutralized == 0 {
        return (body, 0);
    }
    value["messages"] = serde_json::Value::Array(repair.messages);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "ccr_retrieve_history_repair",
                request_id = %request_id,
                neutralized_blocks = repair.neutralized,
                "neutralized headroom_retrieve history blocks the tools array does not declare"
            );
            (bytes::Bytes::from(bytes), repair.neutralized)
        }
        Err(_) => (body, 0),
    }
}

/// Orphan client `tool_result` repair. Neutralizes history results with no
/// preceding matching `tool_use`, then history calls with no matching
/// result in the next message — Anthropic rejects both directions
/// independently of the tools array (so neither sibling repair covers the
/// declared-tool case). Results run first: the siblings only ever remove
/// calls, which can only strand more results, and a result neutralized
/// here strands its call for the second pass. Returns the
/// neutralized-block count; zero means the original bytes are forwarded
/// untouched.
pub(crate) fn maybe_repair_orphan_tool_results(
    body: bytes::Bytes,
    request_id: &str,
) -> (bytes::Bytes, usize) {
    use crate::orphan_tool_result as otr;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
    };
    let messages: Vec<serde_json::Value> = match value.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return (body, 0),
    };
    let first = otr::strip_orphan_tool_results(messages);
    let second = otr::strip_dangling_tool_calls(first.messages);
    let neutralized = first.neutralized + second.neutralized;
    if neutralized == 0 {
        return (body, 0);
    }
    value["messages"] = serde_json::Value::Array(second.messages);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "orphan_tool_repair",
                request_id = %request_id,
                neutralized_blocks = neutralized,
                "neutralized history tool blocks with no matching pair"
            );
            (bytes::Bytes::from(bytes), neutralized)
        }
        Err(_) => (body, 0),
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

/// B3: put back tools the client dropped from this session's roster, so the
/// `tools` prefix stays byte-stable through a one-tool flap. Runs before B2
/// so the order replay sees a complete roster. Same passthrough rules as
/// [`maybe_stabilize_tool_order`]: no `tools`, empty `session_key`, or any
/// parse/serialize failure forwards the original bytes.
pub(crate) fn maybe_pin_tool_roster(
    body: bytes::Bytes,
    store: &cache_stabilization::tool_roster_pin::RosterPinStore,
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
    let Some(tools) = value
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return body;
    };
    let outcome = store.pin(session_key, &model, tools);
    if !outcome.changed() {
        return body;
    }
    tracing::info!(
        event = "tool_roster_pinned",
        request_id = %request_id,
        model = %model,
        reinserted = %outcome.reinserted.join(","),
        appended = %outcome.appended.join(","),
        "tool roster pinned to the session's remembered set"
    );
    match serde_json::to_vec(&value) {
        Ok(bytes) => bytes::Bytes::from(bytes),
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

/// Count the image blocks in the client's own messages, and the placeholders
/// left where it has already dropped one.
///
/// Claude Code sheds an aged-out image by rewriting its `tool_result` to the
/// literal `[image]`. That edits a message deep in the prefix, so everything
/// after it re-caches: 107,003 tokens on 2026-08-24, logged as
/// `early_messages` drift. Two ways out, and the cheaper one depends on
/// numbers nobody has: holding the image costs its tokens re-read every
/// remaining turn, dropping it early costs one rebuild in sessions that might
/// never have collapsed at all.
///
/// So measure before choosing. `image_blocks` is the tax holding would carry,
/// `collapsed_blocks` marks the turn the client let go, and the gap to the
/// next rebuild boundary — already in the log as
/// `prefix_replay_invalidated_on_rebuild` and `no_previous_turn` — is how long
/// that tax would run. Counting only; nothing here changes what is forwarded.
fn image_census(messages: &[serde_json::Value]) -> (usize, usize, usize) {
    let mut images = 0usize;
    let mut collapsed = 0usize;
    let mut b64_bytes = 0usize;
    for message in messages {
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            // An image arrives inside a `tool_result`'s own content array, or
            // on its own as a top-level block.
            let inner = block.get("content").and_then(|c| c.as_array());
            let candidates = inner
                .map(|v| v.as_slice())
                .unwrap_or(std::slice::from_ref(block));
            for candidate in candidates {
                match candidate.get("type").and_then(|t| t.as_str()) {
                    Some("image") => {
                        images += 1;
                        b64_bytes += candidate
                            .get("source")
                            .and_then(|s| s.get("data"))
                            .and_then(|d| d.as_str())
                            .map_or(0, str::len);
                    }
                    Some("text")
                        if candidate.get("text").and_then(|t| t.as_str()) == Some("[image]") =>
                    {
                        collapsed += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    (images, collapsed, b64_bytes)
}

/// Hold this conversation's working-directory line still, restating the live
/// directory at the message tail.
///
/// Byte-equal passthrough — the same cache-safety invariant the other body
/// rewrites keep — when the body is not JSON, `system` names no working
/// directory, this is the conversation's first sight, or the live directory
/// already matches the pin. See [`cache_stabilization::working_dir`].
/// Run every `system` hold this config enables, in place.
///
/// One entry point on purpose. The holds used to be applied inline in
/// `forward_http` and nowhere else, so a routed turn — anything reaching
/// an upstream through `handlers::local_model` rather than through
/// `forward_http` — was forwarded unheld, and the gate conditions had no
/// single place to be read off. Callers that hold an Anthropic body as a
/// `Value` should call this; the byte-level wrappers below exist for the
/// one caller that has bytes.
///
/// `AnthropicMessages`-shaped bodies only: the pins read `system`, which
/// is where Claude Code puts the volatile lines and is not a field the
/// other endpoints carry in that shape.
pub(crate) fn apply_system_holds(
    state: &AppState,
    value: &mut serde_json::Value,
    session_key: &str,
    request_id: &str,
) {
    // Both holds depend on `--prefix-replay`: `working_dir` restates the
    // live directory at the tail and needs replay to carry that note into
    // later turns, and without it the note would break the prefix every
    // turn — causing the churn the hold exists to stop.
    if !state.config.prefix_replay || session_key.is_empty() {
        return;
    }
    if state.config.hold_working_directory {
        hold_working_directory_value(value, &state.working_dir_pins, session_key, request_id);
    }
    if state.config.hold_role_sentence {
        hold_role_sentence_value(value, &state.role_sentence_pins, session_key, request_id);
    }
}

/// Inherit hold pins along message lineage onto a fresh lane, before the
/// preview/hold stages read them.
///
/// A lane switch (`cd`, preamble edit) mints a lane with no pins, so the
/// preview misses and the hold latches the live form — even when the new
/// lane continues another lane's history, in which case the lineage's pin is
/// still the right one and the turn replays at zero cost instead of
/// re-caching. The donor bar is the adoption bar (same floor, same head
/// match), so an unrelated stream can never donate; a lane that already
/// latched its own pin keeps it.
///
/// The probe reads the same snapshot replay does, on both paths, so there
/// is no injection skew to be best-effort about: the Claude hook runs on
/// `parsed` (client bytes — injection, CCR expansion, offload and the
/// thinking strip all mutate downstream copies), and the replay snapshot,
/// the stored histories and the adoption search all derive from those same
/// client bytes. The routed hook runs post-CTX-transforms for the same
/// reason — that path's replay snapshot is taken there too. When no donor
/// is found this is a silent no-op and the adoption gate still keeps the
/// turn honest.
pub(crate) fn inherit_lane_pins(
    state: &AppState,
    lane_key: &str,
    messages: &[serde_json::Value],
    request_id: &str,
) -> Option<String> {
    if lane_key.is_empty() {
        return None;
    }
    let donor = state.replay_store.lineage_donor_lane(lane_key, messages)?;
    let dir = state.working_dir_pins.inherit_pin(&donor, lane_key);
    let sentence = state.role_sentence_pins.inherit_pin(&donor, lane_key);
    if dir || sentence {
        tracing::info!(
            event = "lane_pins_inherited",
            request_id = %request_id,
            donor_lane_hash = %cache_stabilization::drift_detector::session_key_log_prefix(&donor),
            lane_hash = %cache_stabilization::drift_detector::session_key_log_prefix(lane_key),
            working_dir = dir,
            role_sentence = sentence,
            "a lane switch continues another lane's history; its hold pins travel with it"
        );
    }
    Some(donor)
}

/// [`apply_system_holds`] for a caller that holds bytes.
///
/// Byte-equal passthrough when nothing was held, so a turn no hold
/// touched is not re-serialized and cannot pick up a formatting
/// difference on its way through.
fn apply_system_holds_to_bytes(
    state: &AppState,
    body: bytes::Bytes,
    session_key: &str,
    request_id: &str,
) -> bytes::Bytes {
    if !state.config.prefix_replay
        || session_key.is_empty()
        || !(state.config.hold_working_directory || state.config.hold_role_sentence)
    {
        return body;
    }
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let mut held = false;
    if state.config.hold_working_directory {
        held |= hold_working_directory_value(
            &mut value,
            &state.working_dir_pins,
            session_key,
            request_id,
        );
    }
    if state.config.hold_role_sentence {
        held |= hold_role_sentence_value(
            &mut value,
            &state.role_sentence_pins,
            session_key,
            request_id,
        );
    }
    if !held {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => bytes::Bytes::from(bytes),
        Err(_) => body,
    }
}

/// The hold itself. Returns whether `value` was rewritten.
fn hold_working_directory_value(
    value: &mut serde_json::Value,
    pins: &cache_stabilization::working_dir::WorkingDirPins,
    session_key: &str,
    request_id: &str,
) -> bool {
    let outcome = pins.hold(value, session_key);
    let Some(live) = outcome.rewrote() else {
        // Every no-op reason gets a line. "Never fired" and "fired and
        // found nothing to do" are the same count of zero otherwise, and
        // only one of them means the hold is working. The proxy runs at
        // `info`, so the outcomes that mean something went wrong are
        // logged there; the healthy steady state stays at `debug` rather
        // than putting a line on every turn.
        let session_key_hash =
            cache_stabilization::drift_detector::session_key_log_prefix(session_key);
        if outcome.is_noteworthy() {
            tracing::info!(
                event = "working_directory_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "working-directory hold changed nothing"
            );
        } else {
            tracing::debug!(
                event = "working_directory_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "working-directory hold changed nothing"
            );
        }
        return false;
    };
    // The path is the operator's own filesystem, and the session key is
    // hashed for the same reason every other event here hashes it.
    tracing::info!(
        event = "working_directory_held",
        request_id = %request_id,
        session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
        live_directory = %live,
        "held the system preamble's working directory and restated the live one at the tail"
    );
    true
}

/// Hold this conversation's opening role sentence still.
///
/// Byte-equal passthrough on the same terms as [`hold_working_directory`]:
/// not JSON, no such sentence, first sight, or already matching the pin. See
/// [`cache_stabilization::role_sentence`].
/// The hold itself. Returns whether `value` was rewritten.
fn hold_role_sentence_value(
    value: &mut serde_json::Value,
    pins: &cache_stabilization::role_sentence::RoleSentencePins,
    session_key: &str,
    request_id: &str,
) -> bool {
    let outcome = pins.hold(value, session_key);
    let Some(live) = outcome.rewrote() else {
        // Same split as the working-directory hold: an outcome that means
        // the hold wanted to act and could not is worth an `info` line at
        // the proxy's default level, the steady state is not.
        let session_key_hash =
            cache_stabilization::drift_detector::session_key_log_prefix(session_key);
        if outcome.is_noteworthy() {
            tracing::info!(
                event = "role_sentence_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "role-sentence hold changed nothing"
            );
        } else {
            tracing::debug!(
                event = "role_sentence_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "role-sentence hold changed nothing"
            );
        }
        return false;
    };
    tracing::info!(
        event = "role_sentence_held",
        request_id = %request_id,
        live_sentence_len = live.len(),
        "held the opening role sentence to the conversation's opening form"
    );
    true
}

/// How many times retrieval and memory may hand work back to each other.
///
/// Each resolver only runs the calls standing when it starts, and either one's
/// continuation can come back asking for the other: a `memory_search` answered
/// server-side can leave the model asking for a `headroom_retrieve`, and that
/// call arrives after retrieval has already had its turn. Running the pair once
/// in a fixed order left such a call with nobody to run it, so the splice
/// dropped it and downgraded the turn.
///
/// This bounds the alternation between the two, which is not the same quantity
/// as `--ccr-max-retrieval-rounds` — that one bounds a chain of calls of the
/// *same* kind, and each resolver still applies it internally. Handoffs are
/// rare, so a small fixed number covers them; a pass with nothing to do costs
/// no upstream call, only a parse.
pub(crate) const MAX_RESOLVER_ALTERNATIONS: usize = 4;

/// Read one upstream `usage` block in any wire shape booking accepts.
/// Responses reports `input_tokens`/`output_tokens`, Chat Completions
/// reports `prompt_tokens`/`completion_tokens`, Anthropic carries
/// `cache_read_input_tokens` directly. Max-convention throughout: a body
/// carrying both takes the larger, never the sum — matching the outcome
/// funnel. Shared by round folding and passthrough booking so the two
/// cannot drift into reading different numbers off the same block.
pub(crate) fn usage_counts(usage: &serde_json::Value) -> (i64, i64, i64) {
    let get = |key: &str| usage.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    let input = get("input_tokens").max(get("prompt_tokens"));
    let output = get("output_tokens").max(get("completion_tokens"));
    let cached = get("cache_read_input_tokens").max(
        usage
            .get("input_tokens_details")
            .or_else(|| usage.get("prompt_tokens_details"))
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
    );
    (input, output, cached)
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
    pub(crate) fn add_response(&mut self, response: &serde_json::Value) {
        let Some(usage) = response.get("usage") else {
            return;
        };
        let get = |key: &str| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        };
        // Both wire shapes (see `usage_counts`): Responses reports
        // `input_tokens`/`output_tokens`, Chat Completions reports
        // `prompt_tokens`/`completion_tokens`. Same max-convention as the
        // outcome funnel (`book_routed_outcome_with_ccr`); a body carrying
        // both takes the larger, never the sum. Cache reads keep the direct
        // Anthropic key as a fallback: older usage blocks carry no details
        // section.
        let (input, output, cached) = usage_counts(usage);
        if self.rounds == 0 {
            self.client_input_tokens = input.max(0) as u64;
            self.client_cache_read_tokens = cached.max(0) as u64;
            self.client_cache_write_tokens = get("cache_creation_input_tokens").max(0) as u64;
        }
        self.rounds += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.cache_read_tokens += cached;
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
#[allow(clippy::type_complexity)]
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
    request_id: &str,
    original: &bytes::Bytes,
    on_the_wire: &bytes::Bytes,
) {
    let Ok(before) = serde_json::from_slice::<serde_json::Value>(original) else {
        return;
    };
    let Ok(after) = serde_json::from_slice::<serde_json::Value>(on_the_wire) else {
        return;
    };
    audit_tool_pairing(request_id, &before, &after);
    tracker.record_proxy_overhead(prefix_head_bytes(&before), prefix_head_bytes(&after));
    let (definitions, calls) = tool_inventory_of(&after);
    tracker.record_tools(&definitions, &calls);
}

/// `record_request_footprint` on the blocking pool, so the request does not
/// wait for it. The handle is for tests; the proxy drops it.
fn spawn_request_footprint(
    tracker: Arc<headroom_core::savings_tracker::SavingsTracker>,
    request_id: String,
    original: bytes::Bytes,
    on_the_wire: bytes::Bytes,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        record_request_footprint(&tracker, &request_id, &original, &on_the_wire)
    })
}

/// Every `tool_use` block Anthropic will find unanswered in `value`.
///
/// The rule the API enforces: a `tool_use` needs a `tool_result` carrying its
/// id in the very next message. A turn that breaks it is refused whole, with
/// a 400 that names one id and no indication of who dropped it.
fn unanswered_tool_uses(value: &serde_json::Value) -> Vec<(usize, String)> {
    let Some(messages) = value.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        let answered: std::collections::HashSet<&str> = messages
            .get(index + 1)
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .map(|next| {
                next.iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    .filter_map(|b| b.get("tool_use_id").and_then(|i| i.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            if !answered.contains(id) {
                out.push((index, id.to_string()));
            }
        }
    }
    out
}

/// Say, before the 400 arrives, whether the request left here unpaired — and
/// whether it arrived that way.
///
/// Written after 2026-09-03, when a turn was refused for an unanswered
/// `tool_use` and nothing in the logs could settle whether the client had
/// sent it broken or the proxy had broken it. The two bodies are already
/// parsed here, so the answer costs a walk of the messages array.
fn audit_tool_pairing(request_id: &str, before: &serde_json::Value, after: &serde_json::Value) {
    let forwarded = unanswered_tool_uses(after);
    if forwarded.is_empty() {
        return;
    }
    let arrived = unanswered_tool_uses(before);
    let origin = if arrived.is_empty() {
        "proxy"
    } else {
        "client"
    };
    tracing::warn!(
        target: "headroom.proxy",
        event = "unanswered_tool_use_forwarded",
        request_id = %request_id,
        origin = origin,
        unanswered_count = forwarded.len(),
        unanswered = %forwarded
            .iter()
            .map(|(i, id)| format!("{i}:{id}"))
            .collect::<Vec<_>>()
            .join(","),
        arrived_unanswered_count = arrived.len(),
        "a tool_use is going upstream without its tool_result; the request will be refused"
    );
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

/// A chosen upstream and the only transport permitted to connect to it.
/// Keeping these together prevents later retry/continuation paths from
/// accidentally switching a caller-controlled URL back to the trusted client.
struct SelectedUpstream {
    base: url::Url,
    client: reqwest::Client,
    configured_http_proxy: bool,
    allow_slow_path_probe: bool,
}

/// Resolve a per-request upstream override from the `x-headroom-base-url`
/// request header. Returns `None` when the header is absent, empty, or
/// whitespace-only (after trimming), or when the value does not parse as a
/// URL — in all those cases the caller falls back to the default upstream.
/// The value is trimmed and a single trailing `/` is stripped, matching the
/// Python proxy's `.strip().rstrip("/")` contract.
async fn header_upstream_override(
    headers: &HeaderMap,
) -> Option<crate::upstream_guard::ResolvedCallerUpstream> {
    let raw = headers
        .get(crate::headers::UPSTREAM_OVERRIDE_HEADER)
        .and_then(|v| v.to_str().ok())?;
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match url::Url::parse(trimmed) {
        Ok(url) => {
            let resolved =
                crate::upstream_guard::ResolvedCallerUpstream::resolve(url.clone()).await;
            if resolved.is_none() {
                tracing::warn!(
                    event = "upstream_override_rejected",
                    header = crate::headers::UPSTREAM_OVERRIDE_HEADER,
                    value = %url,
                    "ignoring unsafe x-headroom-base-url; using default upstream"
                );
            }
            resolved
        }
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

/// Requests inside `forward_http` right now, for the `inflight` field of
/// `stage_timings`.
///
/// Memory p50 measured 1.7 s with two or fewer requests in flight and 5.6 s
/// with six or seven (2026-09-03). Without the count on the line a slow query
/// and a busy proxy read the same.
static INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// SSE bodies actively streaming to the client, after the `Response` was
/// dispatched and the pipeline `InflightGuard` already dropped. Held by
/// `TrackedStream` below, so `GET /debug/inflight` sees streaming turns and
/// the rotation drain defers instead of RSTing them.
static STREAMING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Holds one slot in `STREAMING` for the life of an SSE body. Created when a
/// streaming `Response` is built, dropped when the body ends or is dropped
/// (client gone, rotation RST, upstream error).
pub(crate) struct StreamingGuard;

impl StreamingGuard {
    pub(crate) fn enter() -> Self {
        STREAMING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for StreamingGuard {
    fn drop(&mut self) {
        STREAMING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pin_project_lite::pin_project! {
    /// A stream that keeps one `STREAMING` slot alive until it ends or is
    /// dropped. Wrap every SSE body before `Body::from_stream` so the drain
    /// check covers bytes that flow after the pipeline guard drops.
    pub(crate) struct TrackedStream<S> {
        _guard: StreamingGuard,
        #[pin]
        inner: S,
    }
}

impl<S> TrackedStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            _guard: StreamingGuard::enter(),
            inner,
        }
    }
}

impl<S: futures_util::Stream> futures_util::Stream for TrackedStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

/// Wrap an SSE stream so `GET /debug/inflight` counts it until the last byte.
pub(crate) fn track_streaming<S>(inner: S) -> TrackedStream<S> {
    TrackedStream::new(inner)
}

/// Holds one slot in `INFLIGHT` from `forward_http` entry to any exit,
/// including `?` returns.
pub(crate) struct InflightGuard;

impl InflightGuard {
    pub(crate) fn enter() -> Self {
        INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }

    /// Requests in flight, this one included.
    fn count(&self) -> usize {
        INFLIGHT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Process-wide in-flight requests, for the rotation drain check
    /// (`GET /debug/inflight`). Covers the pipeline guards (`forward_http`
    /// and routed `handle_messages`, held until the response is dispatched)
    /// plus `STREAMING` bodies actively flowing to the client. A streaming
    /// turn therefore reads nonzero from headers until the last byte.
    pub(crate) fn count_global() -> usize {
        INFLIGHT.load(std::sync::atomic::Ordering::Relaxed)
            + STREAMING.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Marks a request that the cost-aware router must leave alone, carrying the
/// id of the routed attempt it replaces.
///
/// Set on the re-dispatch of a turn whose routed upstream failed: the request
/// is on this path precisely because the router's choice did not work, and
/// re-applying the same rules would send it straight back. Reusing the id
/// keeps the two attempts on one thread in the logs, and lets the second
/// attempt close out the replay and usage entries the first one parked.
#[derive(Clone)]
pub(crate) struct SkipModelRouting(pub(crate) String);

/// Seconds a shed turn is asked to wait before retrying. One commit window:
/// the sibling turn whose overlap tripped the cap is seconds from done, and
/// the client's own 429 backoff stacks on top of this.
const CONCURRENCY_SHED_RETRY_AFTER_SECS: u64 = 1;

/// 429 for a turn shed by `--max-conversation-concurrency`, shared by the
/// passthrough and routed paths (both serve Anthropic-shaped clients).
///
/// Status 429 so the client's standard rate-limit retry fires — clients
/// retry on the status, and the forward paths do the same upstream. The body
/// type mirrors Anthropic's own rate-limit shape for that compatibility,
/// while the message and the `x-headroom-shed` header say this is the proxy
/// pacing one conversation's fan-out, not the provider throttling the
/// account: the two must not be confused on a dashboard.
pub(crate) fn conversation_concurrency_shed_response(
    in_flight: usize,
    cap: usize,
) -> Response<Body> {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "rate_limit_error",
            "message": format!(
                "headroom: conversation concurrency cap exceeded ({in_flight} in flight, cap {cap}); retrying shortly lands against a committed prefix"
            ),
        },
    });
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(
            http::header::RETRY_AFTER,
            CONCURRENCY_SHED_RETRY_AFTER_SECS.to_string(),
        )
        .header("content-type", "application/json")
        .header("x-headroom-shed", "conversation-concurrency")
        .body(Body::from(body.to_string()))
        .expect("static shed response")
}

pub(crate) async fn forward_http(
    state: AppState,
    client_addr: SocketAddr,
    mut req: Request<Body>,
) -> Result<Response<Body>, ProxyError> {
    let start = Instant::now();
    let inflight = InflightGuard::enter();
    // Read before the body is taken, since the extension travels on the
    // request rather than on the wire — a header would leak to the upstream.
    let routing_skipped = req.extensions().get::<SkipModelRouting>().cloned();
    let skip_model_routing = routing_skipped.is_some();
    let request_id = match routing_skipped {
        Some(SkipModelRouting(id)) => id,
        None => ensure_request_id(req.headers()),
    };
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

    forward::select_compression_policy(
        &mut req,
        &state,
        auth_mode,
        &request_id,
        &method,
        &path_for_log,
        body_bytes_hint,
    );

    let selected_upstream =
        forward::resolve_selected_upstream(req.extensions(), req.headers(), &state).await?;
    let upstream_url = build_upstream_url(&selected_upstream.base, &uri)?;
    let configured_http_proxy = selected_upstream.configured_http_proxy;
    let allow_slow_path_probe = selected_upstream.allow_slow_path_probe;
    let upstream_client = selected_upstream.client;

    // Forwarded-Host: prefer client's Host. Forwarded-Proto: assume http for
    // now (we don't terminate TLS in this binary; if a TLS terminator is in
    // front, it should rewrite this — which we'd handle by not overwriting
    // an existing one in a future change).
    //
    // Borrowed: every use of `req` between here and `into_body()` is a
    // shared borrow, so NLL ends this borrow at the forward-headers call
    // and no per-request `String` alloc is needed.
    let forwarded_host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok());

    let (mut outgoing_headers, _strip_internal, _pre_strip_internal_count) =
        forward::build_outgoing_headers(
            &req,
            &state,
            &client_addr,
            forwarded_host,
            &request_id,
            auth_mode,
        );

    // ─── COMPRESSION GATE ──────────────────────────────────────────────
    //
    // PR-A1 lockdown (per `docs/notes/realignment/03-phase-A-lockdown.md`): the
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
    // license_allows: hardcoded true, and staying that way while this binary
    // has no licensing. Python derives it from a `UsageReporter`, which it
    // builds only when HEADROOM_LICENSE_KEY is set — so `true` is exactly what
    // Python computes for every unlicensed deployment, and diverges only for a
    // key that has expired past its grace period. `main` warns at startup when
    // a key is set, so the divergence is announced rather than silent.
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

    // Every POST to a compressible path tees the response into the SSE
    // state machine (usage observer, hit-rate metrics, re-cache watchdog),
    // and the CCR stream rewriter re-frames the body on the passthrough
    // branch too. Those parsers read the raw byte stream, so a gzip/br-encoded
    // upstream response is opaque to them — Claude Code sends
    // `accept-encoding: gzip, deflate, br, zstd` and Anthropic gzips SSE.
    // With `--compression` off that used to blind telemetry and, worse, the
    // rewriter forwarded an empty body under a `content-encoding: gzip`
    // header (client-side ZlibError, "stream ended before any data"). Force
    // identity upstream whenever a parser may attach, not only when
    // intercepting; the client receives uncompressed bytes (we never
    // re-encode), which HTTP permits regardless of what it advertised.
    if method == axum::http::Method::POST && compression::is_compressible_path(uri.path()) {
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

    // The same bytes again, kept for the CCR and memory continuation rounds.
    // A separate binding because `retry_body` is moved by the stream-retry
    // filter below, and because the two want it for opposite reasons: the
    // retry resends it unchanged, the continuation appends to it.
    //
    // Continuations used to start from `original_buffered`, the raw client
    // request. That threw away every transform — injected tools, offloaded
    // content, routed model — so each continuation round presented the
    // provider with a prefix it had never cached. `None` on the passthrough
    // branch, which never builds a forwarded body; callers fall back to the
    // original there, which is what it was.
    let mut forwarded_body: Option<bytes::Bytes> = None;
    // Set inside the intercept arm when a streaming `/v1/responses` request
    // offering `headroom_retrieve` is flipped to a buffered upstream call so
    // CCR can resolve (see `openai_buffered_ccr`); read after the upstream
    // responds to resynthesize SSE for the client. `false` on passthrough.
    let mut buffered_responses_ccr = false;
    // Same-head stampede gate: the key this turn went out under and, for a
    // leader, the token that marks the head readable once headers arrive.
    // `None` on passthrough, on non-Anthropic endpoints, and on heads with no
    // cache marker.
    let mut stampede: Option<(
        String,
        Option<cache_stabilization::prefix_stampede::LeaderToken>,
    )> = None;
    let mut slow_upstream_probe = None;
    let upstream_resp = if should_intercept {
        // Buffer up to `compression_max_body_bytes`. If the body
        let max = state.config.compression_max_body_bytes as usize;
        let body_read_start = Instant::now();
        let buffered =
            forward::read_buffered_body(req, max, body_bytes_hint, &request_id, &path_for_log)
                .await;
        stage_timer.record("buffer", body_read_start.elapsed().as_secs_f64() * 1000.0);
        let buffered = buffered?;
        // Plan 2 step 1: `parse` gap starts where the buffer stage ends.
        // Recorded unconditionally after the ctx block closes below, so an
        // early return between here and there yields a null placeholder.
        let parse_gap_start = Instant::now();

        // Save the original buffer for semantic cache key computation.
        // CTX transforms and compression may modify `buffered`; the
        // cache key must reflect the original request.
        original_buffered = buffered.clone();

        // Claude Code's spinner-text sidecar leaves here, before the endpoint
        // dispatcher below and everything downstream of it — ctx injection,
        // memory tools, the turn fingerprint, the replay store, the cache
        // tracker, offload, compression, cache_control placement. It resends
        // the whole conversation for a four-word status line, so forwarding it
        // whole billed a full prefix read; worse, the prefix it stored made the
        // next real turn read as an unexplained re-cache. See `crate::sidecar`.
        //
        // Nothing is deserialised yet at this point — the first parse is in the
        // volatile/drift block below — so the gate is a substring search rather
        // than a structural check. The phrase is one JSON string with nothing in
        // it that needs escaping, so it survives serialisation verbatim.
        //
        // `memmem` and not `windows().any()`: measured on the largest body in a
        // 2,648-request capture (1.6 MB), the naive scan costs 2,475 us, a full
        // `serde_json` parse costs 1,213 us, and `memmem::find` costs 50 us. The
        // naive scan is the one option slower than the parse it was meant to
        // avoid. A non-sidecar request pays only that 50 us worst case, and the
        // structural predicate that follows costs 0.094 us on a parsed body.
        //
        // A tail-only scan would be cheaper still and is wrong: Claude Code
        // serialises `messages` second, ahead of `system` and 27-39 tool
        // schemas, so the block sits nearer the middle of the body than the end.
        //
        // A `None` back means either that this was not a sidecar or that the
        // shrunk request failed; both fall through to the dispatcher below with
        // `buffered` untouched, which is what the proxy did before this existed.
        if let Some(resp) = forward::maybe_handle_sidecar(
            &buffered,
            uri.path(),
            &state,
            &request_id,
            &upstream_client,
            &upstream_url,
            &headers_snapshot,
        )
        .await
        {
            return Ok(resp);
        }
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
        let buffered = forward::apply_buffered_body_transforms(
            buffered,
            endpoint,
            &headers_snapshot,
            state.config.ccr_handle_responses,
            &request_id,
            &mut buffered_responses_ccr,
            &selected_upstream.base,
        );

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
        // How much of what this lane forwarded last turn the client is still
        // sending, read *before* the boundary invalidation below wipes the
        // tracker it lives in. Same lane key, same field: `invalidate` clears
        // `last_forwarded_messages` and `forwarded_agreement_len` returns
        // `None` on exactly that being empty, so reading it after the
        // invalidation reports "no agreement" on every boundary turn — the
        // turns it was added to speak for. The gate it feeds then had nothing
        // left to check, and the log line published the erased value as
        // evidence the drop was free.
        let mut pre_boundary_agreement: Option<usize> = None;
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
        // Per-stream lane inside the session: same-opener subagent streams
        // share the session key but carry different systems, and keying the
        // drift baseline / replay tracker / pins by session lets them wipe
        // each other's state on every alternation. The lane is derived from
        // the UNMUTATED body (previews below borrow it briefly and put it
        // back byte-identical, so the key is the same either way) and every
        // behavior store downstream takes it instead of the session key.
        // Logging keeps the session hash so existing queries still join.
        let mut request_lane_key = String::new();
        // Same reason, for the conversation the turn belongs to. The
        // fingerprint below is diffed turn-against-turn offline, and the
        // session key alone cannot separate two conversations sharing one
        // session — which is exactly the fan-out case.
        let mut request_conversation_key = String::new();
        // Kept so the outbound hash below is taken with the same shape rules
        // as the inbound one. `None` means no session identity, and nothing
        // downstream observes drift either way.
        let mut request_api_kind: Option<ApiKind> = None;
        if let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(&buffered) {
            if let Some(shed) = forward::analyze_buffered_session(
                &mut parsed,
                forward::RequestScope {
                    state: &state,
                    endpoint,
                    request_id: &request_id,
                    headers_snapshot: &headers_snapshot,
                },
                &client_addr,
                forward::SessionAnalysisOut {
                    session_key: &mut request_session_key,
                    lane_key: &mut request_lane_key,
                    conversation_key: &mut request_conversation_key,
                    api_kind: &mut request_api_kind,
                    rebuild_boundary: &mut rebuild_boundary,
                    pre_boundary_agreement: &mut pre_boundary_agreement,
                    outgoing_headers: &mut outgoing_headers,
                },
            ) {
                return Ok(shed);
            }
        }
        if let Some(hit) =
            forward::check_semantic_cache(&state, &buffered, &request_id, &path_for_log)
        {
            return Ok(hit);
        }
        let (
            buffered,
            additional_tools_restore_plan,
            effective_auth_mode,
            replay_original_messages,
        ) = forward::prepare_replay_inputs(buffered, endpoint, &state, &request_id, auth_mode);
        // The client's own bytes, before ctx_offload and the working-directory
        // hold rewrite them, which is the form whose collapse costs the rebuild.
        forward::log_image_prefix_census(
            &replay_original_messages,
            &headers_snapshot,
            &request_id,
            &request_session_key,
        );
        let (buffered, history_rewritten, offload_boundary) = forward::apply_offload_boundary(
            buffered,
            endpoint,
            &state,
            &request_id,
            &request_lane_key,
            &replay_original_messages,
            rebuild_boundary,
            pre_boundary_agreement,
        );
        let buffered = forward::run_ctx_transform_gate(
            buffered,
            endpoint,
            forward::CtxGateInputs {
                state: &state,
                request_id: &request_id,
                request_lane_key: &request_lane_key,
                request_session_key: &request_session_key,
                headers_snapshot: &headers_snapshot,
                rebuild_boundary,
                history_rewritten,
                offload_boundary,
            },
            &mut stage_timer,
            &mut ctx_transform_tokens_saved,
            &mut proactive_expansion_applied,
        )
        .await;

        // Plan 2 step 1: `parse` gap ends where the ctx block closes. Covers
        // buffer end through ctx/memory transforms; subtract `memory` to
        // isolate repeated parses. Unconditional so the stage always reports.
        stage_timer.record("parse", parse_gap_start.elapsed().as_secs_f64() * 1000.0);

        // Phase 3: refine the ingestion `gate_decision` now that the body is
        // parsed and `has_messages` is known. The header/config inputs are
        // unchanged from the gate (bypass + master switch already routed
        // bypass/off requests to the streaming arm — we only reach here when
        // both are open), so the only input that can flip the result now is
        // `has_messages` → a `no_messages` passthrough. This single
        // `CompressionDecision` drives BOTH the `compression_decision` tracing
        // event and whether the live-zone dispatchers run — the old ad hoc
        // string literals must not coexist with it.
        let (decision, decision_headers) = forward::refine_compression_decision(
            &buffered,
            endpoint,
            &headers_snapshot,
            &state,
            &request_id,
        )?;

        // Per-request tags for RequestOutcome: operator `x-headroom-*` slicing
        // tags plus the canonical `passthrough_reason` when this request was
        // passed through uncompressed.
        let mut _tags = crate::headers::extract_tags(&decision_headers);
        decision.apply_to_tags(&mut _tags);

        // Captured above, BEFORE the CTX stage rewrote `buffered` — see the
        // snapshot next to that reassignment for why the ordering is the whole
        // point.

        let compression_start = Instant::now();
        let forward::CompressionStageOut {
            outcome,
            original_buffered_len,
            outcome_is_passthrough_class,
            compress_tokens_before,
            compress_tokens_saved,
            compress_strategies,
        } = forward::run_compression_stage(forward::CompressionStageIn {
            buffered: &buffered,
            endpoint,
            state: &state,
            decision: &decision,
            effective_auth_mode,
            auth_mode,
            request_id: &request_id,
            path_for_log: &path_for_log,
        });

        outcome_ctx = Some(forward::build_outcome_context(
            &buffered,
            endpoint,
            &headers_snapshot,
            &state,
            &start,
            forward::CompressionTotals {
                tokens_before: compress_tokens_before,
                tokens_saved: compress_tokens_saved,
                strategies: &compress_strategies,
                proactive_expansion_applied,
            },
            _tags.clone(),
        ));
        forward::record_compression_observations(
            &state,
            &request_id,
            endpoint,
            &buffered,
            &start,
            compress_tokens_before,
            compress_tokens_saved,
            &compress_strategies,
        );
        let compression_ms = compression_start.elapsed().as_secs_f64() * 1000.0;
        stage_timer.record("compression", compression_ms);
        // Same number the stage timer just took: this is headroom's own cost,
        // which is what `overhead_ms` means everywhere it is reported.
        if let Some(ctx) = outcome_ctx.as_mut() {
            ctx.overhead_ms = compression_ms;
        }

        let body_to_send = forward::consume_compression_outcome(
            outcome,
            buffered,
            outcome_is_passthrough_class,
            original_buffered_len,
            &state,
            &request_id,
            &path_for_log,
        );

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
        let body_to_send = forward::run_prereplay_holds(
            body_to_send,
            endpoint,
            &state,
            &request_id,
            &request_lane_key,
        );

        // `headers_snapshot` is always `Some` on this buffered branch;
        // `replay_original_messages` is `Some` only when the flag is on
        // and the body carried a messages array.
        let replay_start = Instant::now();
        let body_to_send = forward::run_prefix_replay(
            body_to_send,
            replay_original_messages,
            forward::RequestScope {
                state: &state,
                endpoint,
                request_id: &request_id,
                headers_snapshot: &headers_snapshot,
            },
            &request_lane_key,
            &mut outcome_ctx,
            &mut stage_timer,
            &replay_start,
        );
        // The replay stage above may have adopted another session's prefix
        // for this turn. Hand the donor to the observer so the first-turn
        // event files it as session-key drift rather than new history.
        if let Some(donor_session_key_hash) = state.replay_store.take_adoption(&request_lane_key) {
            state.usage_observer.note_prefix_adoption(
                &request_id,
                cache_stabilization::usage_observer::PrefixAdoption {
                    donor_session_key_hash,
                },
            );
        }

        // Snapshot the prefix as the replay stage leaves it, to be checked
        // against what actually goes out. See [`message_digests`]. Gated the
        // same way as the outbound drift reading below, since both want a body
        // whose shape this code understands.
        let post_replay_digests = request_api_kind.and_then(|_| message_digests(&body_to_send));

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
        // Plan 2 step 1: `rewrite` gap starts before the router/prune/image
        // chain. Recorded unconditionally after the match below.
        let rewrite_start = Instant::now();
        let body_to_send = forward::run_endpoint_rewrite(
            body_to_send,
            forward::RequestScope {
                state: &state,
                endpoint,
                request_id: &request_id,
                headers_snapshot: &headers_snapshot,
            },
            &path_for_log,
            auth_mode,
            selected_upstream.base.as_str(),
            skip_model_routing,
            &mut outcome_ctx,
        );
        stage_timer.record("rewrite", rewrite_start.elapsed().as_secs_f64() * 1000.0);

        let body_to_send = forward::run_tool_shape_stages(
            body_to_send,
            endpoint,
            &state,
            &request_id,
            &request_lane_key,
            decision.should_compress,
        );

        let body_to_send =
            forward::run_ttl_pin(body_to_send, endpoint, &state, &request_id, auth_mode);

        forward::record_wire_ledger(
            &body_to_send,
            endpoint,
            &state,
            &request_id,
            &original_buffered,
            &mut stage_timer,
        );

        // Plan 2 step 1: `post` gap starts where the footprint stage ends.
        // Recorded unconditionally just before `pre_forward` below.
        let post_start = Instant::now();

        let body_to_send = forward::run_presend_seam(
            body_to_send,
            endpoint,
            &state,
            &request_id,
            &mut outgoing_headers,
            &mut outcome_ctx,
        );
        let body_to_send = forward::run_finalize_pipeline(
            body_to_send,
            endpoint,
            &state,
            &request_id,
            &original_buffered,
            auth_mode,
            &mut outcome_ctx,
            &additional_tools_restore_plan,
        );

        forward::observe_presend(
            forward::PresendBodies {
                body_to_send: &body_to_send,
                original_buffered: &original_buffered,
                original_buffered_len,
            },
            forward::RequestScope {
                state: &state,
                endpoint,
                request_id: &request_id,
                headers_snapshot: &headers_snapshot,
            },
            forward::RequestKeys {
                lane_key: &request_lane_key,
                session_key: &request_session_key,
                conversation_key: &request_conversation_key,
                api_kind: request_api_kind,
            },
            &path_for_log,
            &post_replay_digests,
            forward::PresendTiming {
                start: &start,
                post_start: &post_start,
            },
            forward::PresendSinks {
                outcome_ctx: &mut outcome_ctx,
                stage_timer: &mut stage_timer,
                retry_body: &mut retry_body,
                forwarded_body: &mut forwarded_body,
            },
        );

        let send_headers = forward::prepare_buffered_send(
            &state,
            &request_id,
            endpoint,
            &outgoing_headers,
            &body_to_send,
            buffered_responses_ccr,
            &mut stage_timer,
            &mut stampede,
        )
        .await;

        if allow_slow_path_probe {
            slow_upstream_probe = crate::upstream_route_probe::SlowUpstreamProbe::arm(
                upstream_client.clone(),
                &upstream_url,
                &request_id,
                configured_http_proxy,
            );
        }

        // Forward the request with retry on transient errors (429, 529, 5xx).
        forward::send_buffered_with_retry(
            &state,
            &request_id,
            &request_session_key,
            &reqwest_method,
            forward::UpstreamCall {
                client: &upstream_client,
                url: &upstream_url,
                headers: &send_headers,
            },
            &body_to_send,
            &mut outcome_ctx,
        )
        .await?
    } else {
        // Pure streaming path — the original passthrough behaviour. No peek
        // here: passthrough is byte-faithful by contract, and this path has no
        // retry loop to feed anyway.
        let body_stream =
            TryStreamExt::map_err(req.into_body().into_data_stream(), std::io::Error::other);
        let reqwest_body = reqwest::Body::wrap_stream(body_stream);
        if allow_slow_path_probe {
            slow_upstream_probe = crate::upstream_route_probe::SlowUpstreamProbe::arm(
                upstream_client.clone(),
                &upstream_url,
                &request_id,
                configured_http_proxy,
            );
        }
        (
            upstream_client
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
    let (
        status,
        resp_headers,
        is_sse,
        sse_kind,
        (upstream_request_id_anthropic, upstream_request_id_openai, upstream_request_id),
    ) = forward::observe_upstream_head(
        &upstream_resp,
        &mut stampede,
        &mut stage_timer,
        &start,
        &state,
        &path_for_log,
        &request_id,
        state.config.enable_responses_streaming,
        configured_http_proxy,
    );

    let (upstream_body, is_sse, sse_kind, ccr_round_usage, continuation_base) =
        forward::assemble_response_stream(
            upstream_resp,
            sse_prefix,
            retry_body,
            forward::ResponseStreamCtx {
                state: &state,
                request_id: &request_id,
                path_for_log: &path_for_log,
                slow_upstream_probe,
                status,
                is_sse,
                sse_kind,
                buffered_responses_ccr,
                forwarded_body: &forwarded_body,
                original_buffered: &original_buffered,
                upstream: forward::UpstreamCall {
                    client: &upstream_client,
                    url: &upstream_url,
                    headers: &outgoing_headers,
                },
                reqwest_method: &reqwest_method,
                headers_snapshot: &headers_snapshot,
            },
        )
        .await;
    let resp_stream = forward::tee_response_stream(
        upstream_body,
        sse_kind,
        &state,
        outcome_ctx.clone(),
        ccr_round_usage.clone(),
        status,
        &request_id,
    );
    let (body, status, resp_headers) = forward::build_response_body(
        resp_stream,
        forward::ResponseBodyCtx {
            state: &state,
            request_id: &request_id,
            path_for_log: &path_for_log,
            headers_snapshot: &headers_snapshot,
            original_buffered: &original_buffered,
            continuation_base: &continuation_base,
            upstream_url: &upstream_url,
            upstream_client: &upstream_client,
            outgoing_headers: &outgoing_headers,
            outcome_ctx: &outcome_ctx,
            buffered_responses_ccr,
            is_sse,
            sse_kind,
        },
        status,
        resp_headers,
    )
    .await;
    forward::build_final_response(
        forward::FinalResponseParts {
            status,
            resp_headers,
            body,
        },
        &request_id,
        &method,
        &path_for_log,
        forward::ResponseTiming {
            start: &start,
            stage_timer: &stage_timer,
        },
        &inflight,
        forward::UpstreamRequestIds {
            generic: &upstream_request_id,
            anthropic: &upstream_request_id_anthropic,
            openai: &upstream_request_id_openai,
        },
    )
}

/// Tee every streamed chunk toward the SSE state machine without ever holding
/// up the client: `try_send` failures (parser behind, queue full/closed) are
/// logged + counted, never awaited. Mid-stream transport errors are logged with
/// their `Debug`-only source chain — the only thing separating a TLS record
/// failure from an idle drop — and passed through unchanged.
fn tee_stream_to_parser<S>(
    upstream_body: S,
    parser_tx: Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
    parser_telemetry: std::sync::Arc<ParserTelemetry>,
    request_id: String,
) -> futures_util::stream::Map<
    S,
    impl FnMut(Result<bytes::Bytes, reqwest::Error>) -> Result<bytes::Bytes, reqwest::Error>,
>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
{
    let rid = request_id;
    upstream_body.map(move |r| match r {
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
            // `cause` for the same reason as `stream_finisher`: the source
            // chain is `Debug`-only and it is the only thing that separates a
            // TLS record failure from an idle drop.
            tracing::warn!(
                event = "upstream_stream_mid_response_error",
                request_id = %rid,
                error = %e,
                cause = ?e,
                "upstream stream error mid-response"
            );
            Err(e)
        }
    })
}

/// Spawn the SSE state-machine tee: bytes flow to the client unchanged while a
/// spawned task sinks them into an mpsc the state machine drains. The parser is
/// detached from forwarding but its JoinHandle is kept: a waiter task makes
/// panics/cancellation operator-visible instead of erasing the only completion
/// record. The mpsc is bounded; when the parser falls behind `try_send` fails
/// and chunks are logged + dropped — the byte path is never blocked.
fn spawn_sse_parser_tee(
    sse_kind: SseStreamKind,
    state: &AppState,
    outcome_ctx: Option<OutcomeContext>,
    ccr_round_usage: Option<Arc<Mutex<CcrRoundUsage>>>,
    status: StatusCode,
    request_id: &str,
    parser_telemetry: &std::sync::Arc<ParserTelemetry>,
) -> Option<tokio::sync::mpsc::Sender<bytes::Bytes>> {
    if !matches!(sse_kind, SseStreamKind::None) {
        let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(SSE_PARSER_QUEUE_DEPTH);
        let rid_for_parser = request_id.to_owned();
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
            outcome_ctx,
            replay_store_for_parser,
            ccr_round_usage,
            status,
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
                    event = "sse_missed_chunks",
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
                    event = "sse_task_failed",
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
    }
}

/// Streamed-path CCR retrieval: answer the offered `headroom_retrieve` tool call
/// inside the Anthropic SSE stream — suppressing the block, running the
/// continuation against `continuation_base`, splicing the result back in — so
/// the streamed turn behaves like the buffered one. When `ccr_stream_eligible`
/// is false the upstream body is handed on untouched.
//
// Ten request-context args for one call site; a params struct would churn
// both without buying clarity, so the lint stays off here by decision.
#[allow(clippy::too_many_arguments)]
async fn maybe_rewrite_anthropic_stream(
    upstream_body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
    >,
    ccr_stream_eligible: bool,
    continuation_base: bytes::Bytes,
    original_buffered: bytes::Bytes,
    state: &AppState,
    upstream_client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    headers_snapshot: &Option<http::HeaderMap>,
    request_id: &str,
) -> (
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    Option<Arc<Mutex<CcrRoundUsage>>>,
) {
    if ccr_stream_eligible {
        let ctx = crate::sse::ccr_stream::CcrStreamContext {
            client: upstream_client.clone(),
            upstream_url: upstream_url.clone(),
            outgoing_headers: outgoing_headers.clone(),
            forwarded_request: continuation_base.clone(),
            ccr_store: state
                .ctx_offload
                .as_ref()
                .expect("ctx_offload checked above")
                .store
                .ccr(),
            ccr_stores: state.ctx_offload.as_ref().map(|r| r.store.stores()),
            config: state.config.clone(),
            request_id: request_id.to_owned(),
            shape: crate::sse::ccr_stream::CcrShape::Anthropic,
            memory: memory_tool_context(
                state,
                headers_snapshot,
                Some("anthropic"),
                &original_buffered,
            )
            .await,
            // Anthropic path: redaction lives on routed translate paths only.
            redact: None,
            // Anthropic path: the caller folds the returned handle itself.
            rounds_sink: None,
        };
        let (stream, usage) = crate::sse::ccr_stream::rewrite_anthropic_stream(upstream_body, ctx);
        (Box::pin(stream), Some(usage))
    } else {
        (Box::pin(upstream_body), None)
    }
}

/// #2613 edge, port of `_openai_responses_from_sse`: some OpenAI-compatible
/// upstreams answer a `stream: false` request with a valid 200 SSE body. When
/// this request was buffered for Responses CCR, collect that stream and
/// reassemble the terminal JSON so the buffered arm below still resolves
/// retrieval. Without a terminal event the collected bytes stream through
/// unchanged (previous behaviour, error included).
async fn reframe_buffered_responses_sse(
    mut upstream_body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
    >,
    is_sse: bool,
    sse_kind: SseStreamKind,
    buffered_responses_ccr: bool,
    status: StatusCode,
    request_id: &str,
) -> (
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    bool,
    SseStreamKind,
) {
    let (mut is_sse, mut sse_kind) = (is_sse, sse_kind);
    if buffered_responses_ccr && is_sse && status.is_success() {
        use futures_util::StreamExt as _;
        let mut collected = bytes::BytesMut::new();
        let mut first_err: Option<reqwest::Error> = None;
        {
            let s = &mut upstream_body;
            while let Some(chunk) = s.next().await {
                match chunk {
                    Ok(b) => collected.extend_from_slice(&b),
                    Err(e) => {
                        first_err = Some(e);
                        break;
                    }
                }
            }
        }
        let reassembled: Option<bytes::Bytes> = if first_err.is_none() {
            // Whole stream collected cleanly: reassemble the terminal JSON
            // when one is present.
            crate::openai_buffered_ccr::responses_completed_from_sse(&String::from_utf8_lossy(
                &collected,
            ))
            .and_then(|completed| serde_json::to_vec(&completed).ok())
            .map(bytes::Bytes::from)
        } else {
            None
        };
        if let Some(json_bytes) = reassembled {
            tracing::info!(
                request_id = %request_id,
                event = "buffered_responses_ccr_sse_answer",
                "upstream answered stream:false with SSE; reassembled terminal JSON for buffered handling"
            );
            upstream_body = Box::pin(futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(
                json_bytes,
            )]));
            is_sse = false;
            sse_kind = SseStreamKind::None;
        } else {
            let mut items: Vec<reqwest::Result<bytes::Bytes>> = vec![Ok(collected.freeze())];
            if let Some(e) = first_err {
                items.push(Err(e));
            }
            upstream_body = Box::pin(futures_util::stream::iter(items));
        }
    }
    (upstream_body, is_sse, sse_kind)
}

/// Assemble the client-bound upstream byte stream: re-prepend bytes peeked while
/// checking for a leading in-band error (`sse_prefix` is empty on paths that did
/// not peek, so that step is a no-op there), then wrap the stream so an early
/// drop retries from a held-back opening instead of reaching the client. The
/// retry loop above only ever saw the headers; this wrapper sits below CCR and
/// below the telemetry tee, so a discarded attempt is invisible to both.
fn assemble_upstream_body(
    upstream_resp: reqwest::Response,
    sse_prefix: bytes::Bytes,
    retry_body: Option<bytes::Bytes>,
    is_sse: bool,
    status: StatusCode,
    retry: UpstreamBodyRetry,
    slow_upstream_probe: Option<crate::upstream_route_probe::SlowUpstreamProbe>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>> {
    let upstream_body = {
        let rest = upstream_resp.bytes_stream();
        let head =
            futures_util::stream::iter((!sse_prefix.is_empty()).then(|| Ok(sse_prefix.clone())));
        Box::pin(head.chain(rest))
    };
    let upstream_body =
        crate::upstream_route_probe::cancel_on_first_chunk(upstream_body, slow_upstream_probe);
    if let Some(body) = retry_body.filter(|_| {
        is_sse
            && status.is_success()
            && retry.enabled
            && retry.hold_bytes > 0
            && retry.max_attempts > 1
    }) {
        Box::pin(crate::sse::stream_retry::retry_on_early_drop(
            upstream_body,
            crate::sse::stream_retry::RetryContext {
                client: retry.client,
                method: retry.method,
                url: retry.url,
                headers: retry.headers,
                body,
                request_id: retry.request_id,
                max_attempts: retry.max_attempts,
                base_delay_ms: retry.base_delay_ms,
                max_delay_ms: retry.max_delay_ms,
                hold_bytes: retry.hold_bytes,
            },
        ))
    } else {
        Box::pin(upstream_body)
    }
}

/// By-value retry inputs for [`assemble_upstream_body`], cloned out of the live
/// request state at the call site so the stream wrapper owns what it needs.
struct UpstreamBodyRetry {
    enabled: bool,
    hold_bytes: usize,
    max_attempts: u32,
    client: reqwest::Client,
    method: reqwest::Method,
    url: String,
    headers: http::HeaderMap,
    request_id: String,
    base_delay_ms: u64,
    max_delay_ms: u64,
}

/// Phase G PR-G3: extract upstream rate-limit headers from this response and
/// record them as gauges. The `provider` label comes from which upstream
/// `request-id` shape was seen (Anthropic vs OpenAI); when neither was detected
/// emission is skipped rather than guessed ("no silent fallbacks").
///
/// Also parses + records the Subscription/OAuth `unified-*` family, which the
/// `*-remaining` gauges never see on a Claude-subscription plan. A non-empty
/// unified snapshot is self-attributing, so no provider label is needed.
fn record_upstream_rate_limits(
    headers: &http::HeaderMap,
    has_anthropic_request_id: bool,
    has_openai_request_id: bool,
    request_path: &str,
    request_id: &str,
) {
    let rate_limit_snapshot = crate::observability::extract_rate_limit_snapshot(headers);
    let rate_limit_provider: Option<&'static str> = if has_anthropic_request_id {
        Some(crate::observability::cache_hit_rate_provider::ANTHROPIC)
    } else if has_openai_request_id {
        // We can't distinguish chat vs responses purely from the
        // request-id header; the `request_path` is more specific.
        Some(if request_path.contains("/v1/responses") {
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
            request_id,
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
            path = %request_path,
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
    let unified_snapshot = crate::observability::extract_unified_rate_limit(headers);
    if !unified_snapshot.windows.is_empty()
        || unified_snapshot.overall_status.is_some()
        || unified_snapshot.fallback_percentage.is_some()
    {
        crate::observability::record_unified_rate_limit(&unified_snapshot, request_id);
    }
}

/// PR-C1: classify the SSE flavor for the response state machine from the request
/// path. Bytes flow to the client unchanged; the state machine sinks bytes into
/// a channel in a spawned task that never blocks the byte path.
///
/// PR-C4: the OpenAI Responses arm is gated by `enable_responses_streaming`.
/// When false the tee short-circuits to `None` so the framer + state machine
/// don't spin up and bytes flow opaquely. Other providers' state machines are
/// unaffected.
fn classify_sse_kind(
    is_sse: bool,
    request_path: &str,
    enable_responses_streaming: bool,
    request_id: &str,
) -> SseStreamKind {
    if is_sse {
        let kind = SseStreamKind::for_request_path(request_path);
        if matches!(kind, SseStreamKind::OpenAiResponses) && !enable_responses_streaming {
            tracing::info!(
                request_id = %request_id,
                path = %request_path,
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
    }
}

/// PR-A8 / P5-57: capture the upstream request id BEFORE the caller moves
/// `upstream_resp.headers()` into the response filter. Anthropic emits
/// `request-id` (lowercase, no `x-`); OpenAI emits `x-request-id`.
/// Returns `(anthropic, openai, preferred)`; when both are present the
/// Anthropic one wins since it is the path-shape the cache invariants lock down.
fn capture_upstream_request_ids(
    headers: &http::HeaderMap,
) -> (Option<String>, Option<String>, Option<String>) {
    let anthropic = headers
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let openai = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // Prefer the provider-specific id whichever was set. Both
    // present is unusual but legal; prefer Anthropic since it's the
    // path-shape we lockdown with cache invariants.
    let preferred = anthropic.clone().or_else(|| openai.clone());
    (anthropic, openai, preferred)
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
/// Whether `after` still carries every signed reasoning block the provider
/// will read back. Anthropic reads the LAST assistant message's blocks back
/// (they must stay while a tool loop is open) and refuses any block that comes
/// back altered. Blocks from earlier assistant turns may be dropped whole:
/// `compression::prior_thinking` does so on a rebuild boundary, and the replay
/// store repeats the stripped bytes on every steady turn after. So: the last
/// assistant message's blocks match exactly, and the rest of `after` is a
/// subsequence of `before` — nothing edited, nothing invented.
fn signed_reasoning_preserved(before: &[serde_json::Value], after: &[serde_json::Value]) -> bool {
    fn last_assistant_blocks(messages: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .map(|m| signed_reasoning_blocks(std::slice::from_ref(m)))
            .unwrap_or_default()
    }
    if last_assistant_blocks(before) != last_assistant_blocks(after) {
        return false;
    }
    // Length gate: `after` as a subsequence of `before` needs at most
    // as many blocks (pigeonhole) — free exact pre-check before the
    // O(n·m) deep-compare scan below.
    let before_blocks = signed_reasoning_blocks(before);
    let after_blocks = signed_reasoning_blocks(after);
    if after_blocks.len() > before_blocks.len() {
        return false;
    }
    let mut remaining = before_blocks.into_iter();
    after_blocks
        .into_iter()
        .all(|block| remaining.any(|kept| kept == block))
}

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
            // Genuinely signed *by the provider*, as the name says. What this
            // guards is Anthropic's refusal of a signed block that came back
            // altered, and neither an unsigned block nor one carrying our own
            // envelope has anything to violate. Counting those would make the
            // two drop stages meant to remove them look like tampering, and
            // the restore below would put back the block upstream is about to
            // refuse.
            is_reasoning && !is_unsigned_reasoning(b) && !is_headroom_signed_reasoning(b)
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
    let Some((out, dropped, markers_moved)) =
        drop_reasoning_blocks_where(&body_to_send, is_unsigned_reasoning)
    else {
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
    out
}

/// Drop `thinking` blocks this proxy signed itself.
///
/// A routed turn answered by an OpenAI-shaped upstream comes back with its
/// reasoning item packed into a signature only this proxy can read — see
/// [`crate::handlers::reasoning_signature`]. That works while the
/// conversation stays on the routed model, which is what the `:translate`
/// routes were built for. The cost-aware router (#1706) broke that
/// assumption: it sends one tool-less turn to a cheap model and leaves the
/// next one, which usually declares tools, on Anthropic. The client stores
/// the block and hands it back, and Anthropic refuses a signature it never
/// issued — one cheap turn poisoning every turn after it.
///
/// So our own envelopes come off on the way to Anthropic. Nothing is lost
/// that Anthropic could have used: it cannot read the envelope, and the model
/// that wrote the reasoning is not the one being asked to continue it. The
/// `:translate` paths do not call this, so replay to the routed upstream is
/// untouched.
///
/// The gate is the prefix itself rather than `"thinking"`, so a body that
/// never met a routed turn skips this on a substring scan.
pub(crate) fn drop_headroom_signed_reasoning_blocks(
    body_to_send: bytes::Bytes,
    request_id: &str,
) -> bytes::Bytes {
    const MARKER: &[u8] = b"headroom:codex:v1:";
    if !body_to_send.windows(MARKER.len()).any(|w| w == MARKER) {
        return body_to_send;
    }
    let Some((out, dropped, markers_moved)) =
        drop_reasoning_blocks_where(&body_to_send, is_headroom_signed_reasoning)
    else {
        return body_to_send;
    };
    tracing::info!(
        request_id = %request_id,
        event = "headroom_signed_reasoning_blocks_dropped",
        dropped,
        markers_moved,
        "removed thinking blocks carrying this proxy's own reasoning envelope; \
         a routed turn wrote them and Anthropic would refuse a signature it \
         did not issue"
    );
    out
}

/// The message-array surgery both drop stages share.
///
/// Returns `None` when there is nothing to do — unparseable body, no
/// `messages`, or no block the predicate claims — so the caller can hand back
/// its original `Bytes` untouched rather than pay a re-serialize that would
/// change nothing.
fn drop_reasoning_blocks_where(
    body_to_send: &bytes::Bytes,
    doomed: fn(&serde_json::Value) -> bool,
) -> Option<(bytes::Bytes, usize, usize)> {
    let mut v = serde_json::from_slice::<serde_json::Value>(body_to_send).ok()?;
    let messages = v.get_mut("messages").and_then(|m| m.as_array_mut())?;
    let mut dropped = 0usize;
    let mut markers_moved = 0usize;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        // Removing every block would leave a message with empty content, which
        // upstream refuses just as firmly as the doomed block does. Nothing
        // this proxy writes looks like that — `stream_finisher` always leaves a
        // text block behind — but history the proxy did not write reaches here
        // too, and trading one bad turn for a different bad turn is no trade.
        if content.iter().all(&doomed) {
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
            if doomed(&block) {
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
        return None;
    }
    let out = serde_json::to_vec(&v).ok()?;
    Some((bytes::Bytes::from(out), dropped, markers_moved))
}

/// A `thinking` block this proxy signed on a routed turn.
///
/// `redacted_thinking` is included for the same reason it is everywhere else
/// here: the two types travel together and a caller that handled one and not
/// the other would leave half the problem on the wire.
fn is_headroom_signed_reasoning(block: &serde_json::Value) -> bool {
    let is_reasoning = matches!(
        block.get("type").and_then(|t| t.as_str()),
        Some("thinking") | Some("redacted_thinking")
    );
    let ours = block
        .get("signature")
        .and_then(|s| s.as_str())
        .is_some_and(crate::handlers::reasoning_signature::is_headroom_reasoning_signature);
    is_reasoning && ours
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
        .map_or(true, |s| s.is_empty());
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
///
/// It costs less than that now. Putting the whole array back also reverted
/// messages nobody had complained about, including the opening ones — and
/// those are in the cached prefix. Measured over 09-02, the four turns that
/// took the wholesale restore are the four costliest proxy-caused re-caches
/// of the day: 573,940 of 622,325 wasted tokens, one of them dropping a
/// session's cache read from 267,681 to 17,238 in a single turn. So the
/// repair now names the messages that broke the invariant and puts back only
/// those, falling back to the whole array when it cannot map one array onto
/// the other or when the narrow repair does not settle it.
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
    if signed_reasoning_preserved(before_messages, after_messages) {
        return body_to_send;
    }

    let block_count = signed_reasoning_blocks(before_messages).len();
    let message_count = before_messages.len();
    let (restored, scope, restored_count) =
        match repair_signed_reasoning(before_messages, after_messages) {
            Some((messages, count)) => (messages, "offending_messages", count),
            None => (before_messages.clone(), "all_messages", message_count),
        };
    let Some(map) = after.as_object_mut() else {
        return body_to_send;
    };
    map.insert("messages".to_string(), serde_json::Value::Array(restored));
    match serde_json::to_vec(&after) {
        Ok(bytes) => {
            tracing::warn!(
                target: "headroom.proxy",
                event = "signed_reasoning_blocks_restored",
                request_id = %request_id,
                signed_blocks = block_count,
                messages_before = message_count,
                messages_restored = restored_count,
                restore_scope = scope,
                "outbound body altered the client's signed reasoning blocks; \
                 forwarding the client's copy of the messages that changed"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body_to_send,
    }
}

/// Put back only the messages whose signed reasoning the outbound chain broke.
///
/// Two things make a body unacceptable to Anthropic, and each names its own
/// messages: the last assistant message's signed blocks must arrive
/// unchanged, and no signed block may appear that the client did not send. A
/// message that merely *lost* a signed block breaks neither — that is
/// [`crate::compression::prior_thinking`] doing its job, and reverting it
/// would undo the saving for nothing.
///
/// Returns `None` when the arrays cannot be lined up index for index, or when
/// the narrow repair leaves the invariant still broken. The caller falls back
/// to the whole array on either.
fn repair_signed_reasoning(
    before: &[serde_json::Value],
    after: &[serde_json::Value],
) -> Option<(Vec<serde_json::Value>, usize)> {
    if before.len() != after.len() {
        return None;
    }
    let last_assistant = before
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
    let sent = signed_reasoning_blocks(before);

    let mut targets = Vec::new();
    for index in 0..after.len() {
        let ours = signed_reasoning_blocks(std::slice::from_ref(&after[index]));
        let theirs = signed_reasoning_blocks(std::slice::from_ref(&before[index]));
        let last_assistant_changed = Some(index) == last_assistant && ours != theirs;
        let invented = ours.iter().any(|block| !sent.contains(block));
        if last_assistant_changed || invented {
            targets.push(index);
        }
    }
    if targets.is_empty() {
        return None;
    }

    let mut repaired = after.to_vec();
    for index in &targets {
        repaired[*index] = before[*index].clone();
    }
    if !signed_reasoning_preserved(before, &repaired) {
        return None;
    }
    Some((repaired, targets.len()))
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
/// One cheap digest per message, in order.
///
/// Used to check a single invariant inside one request: whatever the prefix
/// replay spliced in must still be there when the bytes go out. Everything
/// between those two points — breakpoint placement, memory injection, context
/// injection, PAYG rewrites — is supposed to leave the settled prefix alone,
/// and nothing verified that it did.
///
/// Returns `None` for a body without a `messages` array, which is not the
/// shape this checks.
/// Every property of the forwarded request that the provider's cache key
/// depends on, in one line, so the residue can be diffed turn-against-turn
/// offline instead of needing a separate instrument per hypothesis.
///
/// Returns `(model, marker_map, breakpoints)`. `marker_map` names each
/// `cache_control` position and its TTL — `sys:1h,m12:1h,m30:5m` — because a
/// breakpoint that moves behind the settled prefix, or a fifth one that pushes
/// an earlier one out, kills the read while every byte still matches.
fn cache_key_fingerprint(body: &[u8]) -> Option<(String, String, usize)> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("?")
        .to_string();

    let ttl_of = |v: &serde_json::Value| {
        v.get("cache_control")
            .and_then(|c| c.get("ttl"))
            .and_then(|t| t.as_str())
            .unwrap_or("5m")
            .to_string()
    };
    let mut marks: Vec<String> = Vec::new();
    let mut scan = |label: String, container: Option<&serde_json::Value>| {
        let Some(blocks) = container.and_then(|c| c.as_array()) else {
            return;
        };
        for (i, b) in blocks.iter().enumerate() {
            if b.get("cache_control").is_some() {
                marks.push(format!("{label}[{i}]:{}", ttl_of(b)));
            }
        }
    };
    scan("sys".into(), parsed.get("system"));
    scan("tools".into(), parsed.get("tools"));

    if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
        for (i, m) in msgs.iter().enumerate() {
            if m.get("cache_control").is_some() {
                marks.push(format!("m{i}:{}", ttl_of(m)));
            }
            if let Some(blocks) = m.get("content").and_then(|c| c.as_array()) {
                for (j, b) in blocks.iter().enumerate() {
                    if b.get("cache_control").is_some() {
                        marks.push(format!("m{i}.{j}:{}", ttl_of(b)));
                    }
                }
            }
        }
    }
    let count = marks.len();
    Some((model, marks.join(","), count))
}

/// Cumulative digests of the forwarded message prefix at doubling depths —
/// `1:a1b2,2:c3d4,4:...,8:...`. Two turns of one conversation share every
/// checkpoint up to the point where their forwarded bytes first differ, so the
/// smallest depth whose digest moved localizes the divergence without logging
/// a digest per message.
///
/// This is the one property the whole cache rests on: the forwarded prefix is
/// byte-stable turn over turn, except for the tail this turn appends.
fn prefix_digest_ladder(body: &[u8]) -> Option<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    let mut hasher = DefaultHasher::new();
    let mut out = Vec::new();
    let mut depth = 1usize;
    for (i, m) in messages.iter().enumerate() {
        cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(m)
            .to_string()
            .hash(&mut hasher);
        if i + 1 == depth {
            out.push(format!("{depth}:{:04x}", hasher.finish() & 0xffff));
            depth *= 2;
        }
    }
    Some(out.join(","))
}

/// Windowed digests of the forwarded tail — `t1` covers the last message,
/// `t2` the last two, `t4` the last four — so churn in the tail can be placed
/// without logging a digest per message.
///
/// The head-anchored [`prefix_digest_ladder`] goes blind exactly where the
/// residual misses live: its checkpoints stop doubling at 32, while the
/// disputed region on a 50-message turn is messages 33+. Two turns sharing
/// every head checkpoint can still differ anywhere in the tail, and that is
/// the whole residue. Read this one back to front: the smallest window whose
/// digest moved bounds the churn to that many tail messages (`t1` moved: the
/// last message; `t1` held but `t2` moved: the second-to-last).
///
/// Same projection and hasher as the head ladder, so the two agree on what
/// counts as a difference. Windows clamp to the messages there are, keeping
/// the `t1,t2,t4` schema fixed for log queries. Emitted as its own
/// `tail_ladder` field rather than folded into `prefix_ladder`, so existing
/// parsers of that field keep working untouched.
fn tail_digest_ladder(body: &[u8]) -> Option<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    let mut out = Vec::new();
    for k in [1usize, 2, 4] {
        let start = messages.len().saturating_sub(k);
        let mut hasher = DefaultHasher::new();
        for m in &messages[start..] {
            cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(m)
                .to_string()
                .hash(&mut hasher);
        }
        out.push(format!("t{k}:{:04x}", hasher.finish() & 0xffff));
    }
    Some(out.join(","))
}

/// Digests of the two fields that precede every message in the provider's
/// cached prefix. A change to either kills the whole cache, and no message
/// digest can see it — `tools` and `system` are rewritten by four stages that
/// run after the replay stage (`maybe_prune_tools`, `maybe_compact_tool_
/// schemas`, the stable tool order pass, and `maybe_inject_context_management`).
fn preamble_digests(body: &[u8]) -> Option<(u64, u64)> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let digest = |v: Option<&serde_json::Value>| {
        let mut hasher = DefaultHasher::new();
        match v {
            Some(v) => {
                cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(v)
                    .to_string()
                    .hash(&mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
        hasher.finish()
    };
    Some((digest(parsed.get("system")), digest(parsed.get("tools"))))
}

fn message_digests(body: &[u8]) -> Option<Vec<u64>> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?;
    Some(
        messages
            .iter()
            .map(|m| {
                let mut hasher = DefaultHasher::new();
                // The same projection prefix replay compares on, not the raw
                // bytes. `maybe_push_tail_breakpoint` moves the cache_control
                // marker after the replay stage on nearly every turn, and the
                // provider's prefix key ignores it — hashing it raw reports a
                // mutation on almost every request and hides real ones.
                cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(m)
                    .to_string()
                    .hash(&mut hasher);
                hasher.finish()
            })
            .collect(),
    )
}

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
/// Last tool roster forwarded on each session.
///
/// The composition line fingerprints the tool names, which says *that* the
/// array moved but never *what* moved — and a tool arriving or leaving
/// invalidates the whole cached prefix behind it, since tools sit at the
/// front of the cache key. Measured over 09-02, five such turns cost 961k
/// tokens between them, every one of them a tool the client dropped. Naming
/// it is the difference between knowing a tool churns and being able to prune
/// it.
fn tool_rosters() -> &'static Mutex<std::collections::HashMap<String, Vec<String>>> {
    static ROSTERS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Vec<String>>>> =
        std::sync::OnceLock::new();
    ROSTERS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Sessions tracked before the map starts forgetting. One entry is a session
/// key and a list of tool names; a few hundred of those is nothing, and the
/// cap only has to stop an unbounded process from growing one.
const TOOL_ROSTER_CAPACITY: usize = 512;

/// Log which tools joined or left this session's array since the last turn.
///
/// Silent on the first turn of a session: there is nothing to compare against,
/// and "every tool appeared" is not news.
fn note_tool_roster(session_key: &str, request_id: &str, names: &[&str]) {
    if session_key.is_empty() {
        return;
    }
    let current: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let Ok(mut rosters) = tool_rosters().lock() else {
        return;
    };
    let previous = rosters.insert(session_key.to_string(), current.clone());
    if rosters.len() > TOOL_ROSTER_CAPACITY {
        // Whichever the map hands over first. Losing a baseline costs one
        // missed comparison, not a wrong one.
        if let Some(victim) = rosters
            .keys()
            .find(|key| key.as_str() != session_key)
            .cloned()
        {
            rosters.remove(&victim);
        }
    }
    drop(rosters);

    let Some(previous) = previous else {
        return;
    };
    if previous == current {
        return;
    }
    let added: Vec<&str> = current
        .iter()
        .filter(|name| !previous.contains(name))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = previous
        .iter()
        .filter(|name| !current.contains(name))
        .map(String::as_str)
        .collect();
    if added.is_empty() && removed.is_empty() {
        // Same set, different order. Worth its own reading: order is part of
        // the cache key too, and `cache_stable_tool_order` exists to hold it.
        tracing::info!(
            target: "headroom.proxy",
            event = "tool_roster_reordered",
            request_id = %request_id,
            tool_count = current.len(),
            "the tools array kept its members and changed their order"
        );
        return;
    }
    tracing::warn!(
        target: "headroom.proxy",
        event = "tool_roster_changed",
        request_id = %request_id,
        added = %added.join(","),
        removed = %removed.join(","),
        count_before = previous.len(),
        count_after = current.len(),
        "the forwarded tools array changed; the cached prefix behind it is dead"
    );
}

/// One `tool_use` block with no matching `tool_result` in the next message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PairingOffense {
    message_index: usize,
    tool_use_id: String,
}

/// Every `tool_use` in `body` that the upstream would refuse: Anthropic
/// requires each one (except in the final message, where a turn may legally
/// end on a tool call whose result arrives next turn) to have a
/// `tool_result` carrying its id in the immediately following message.
/// Blocks without a string id are skipped — a missing id is a different
/// malformation with its own error, and flagging it here would misname it.
fn outbound_tool_pairing_offenses(body: &serde_json::Value) -> Vec<PairingOffense> {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut offenses = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if index + 1 >= messages.len() {
            break;
        }
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = block
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let answered = messages[index + 1]
                .get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|results| {
                    results.iter().any(|r| {
                        r.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                            && r.get("tool_use_id").and_then(|v| v.as_str()) == Some(id)
                    })
                });
            if !answered {
                offenses.push(PairingOffense {
                    message_index: index,
                    tool_use_id: id.to_string(),
                });
            }
        }
    }
    offenses
}

/// Boundary between "the client sent it broken" and "a pipeline stage broke
/// it", logged at the wire-footprint site on the exact bytes about to leave.
/// Runs only on Anthropic-shaped bodies: other wire formats pair calls
/// differently, and applying this rule to them would false-positive.
///
/// Log-only in both cases. An unanswerable `tool_use` cannot be repaired —
/// its result does not exist anywhere — so blocking would only swap whose
/// 400 the client sees. What this buys is attribution the `upstream_rejected`
/// line cannot give: whether to look at the client's transcript or at the
/// stages between it and the wire.
fn check_outbound_tool_pairing(
    request_id: &str,
    session_key: &str,
    client_body: &[u8],
    wire_body: &[u8],
) {
    // Cheap gate first: no `tool_use` substring anywhere means no offense is
    // possible, and most turns carry none. Over-approximate on purpose
    // (`tool_use_id` contains it too) — the parse below decides.
    const MARKER: &[u8] = b"tool_use";
    if !wire_body
        .windows(MARKER.len())
        .any(|window| window == MARKER)
    {
        return;
    }
    let Ok(wire) = serde_json::from_slice::<serde_json::Value>(wire_body) else {
        return;
    };
    let offenses = outbound_tool_pairing_offenses(&wire);
    if offenses.is_empty() {
        return;
    }
    // Attribute: an offense already present in the client's own bytes was
    // sent broken (interrupted flow, compaction seam, concurrent writers to
    // one transcript). Anything else unpaired at the wire but paired on
    // arrival, a pipeline stage unpaired.
    let client_broken: std::collections::HashSet<String> =
        serde_json::from_slice::<serde_json::Value>(client_body)
            .map(|client| outbound_tool_pairing_offenses(&client))
            .unwrap_or_default()
            .into_iter()
            .map(|offense| offense.tool_use_id)
            .collect();
    for offense in offenses.iter().take(10) {
        tracing::warn!(
            target: "headroom.proxy",
            event = "outbound_tool_pairing_broken",
            request_id = %request_id,
            session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
            message_index = offense.message_index,
            tool_use_id = %offense.tool_use_id,
            offenses_total = offenses.len(),
            origin = if client_broken.contains(&offense.tool_use_id) {
                "client"
            } else {
                "proxy"
            },
            "assistant tool_use has no matching tool_result in the next message; \
             forwarding anyway — the upstream will refuse this turn"
        );
    }
}

#[cfg(test)]
mod cold_fork_tests {
    use super::*;

    fn msgs() -> Vec<serde_json::Value> {
        vec![serde_json::json!({"role": "user", "content": "hi"})]
    }

    #[test]
    fn disabled_flag_never_forks() {
        assert!(maybe_cold_fork(
            false,
            true,
            "claude-opus-5",
            &msgs(),
            None,
            Some(99999.0),
            None
        )
        .is_none());
    }

    #[test]
    fn unknown_session_is_warm() {
        // No idle reading (first turn): conservative warm, replay untouched.
        assert!(maybe_cold_fork(true, true, "claude-opus-5", &msgs(), None, None, None).is_none());
    }

    #[test]
    fn warm_lane_is_untouched() {
        assert!(
            maybe_cold_fork(true, true, "claude-opus-5", &msgs(), None, Some(10.0), None).is_none()
        );
    }

    #[test]
    fn cold_lane_recompacts_in_cache_mode() {
        let fork = maybe_cold_fork(
            true,
            true,
            "claude-opus-5",
            &msgs(),
            None,
            Some(99999.0),
            None,
        )
        .expect("idle-past-TTL lane must fork");
        assert_eq!(fork.ttl_desc, "300s");
        // Trivial messages: lossless passes fold nothing, input survives.
        assert_eq!(fork.messages, msgs());
    }
}

#[cfg(test)]
mod tool_search_wiring_tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> serde_json::Value {
        json!({"name": name, "input_schema": {"type": "object"}})
    }

    fn fourteen_tools() -> Vec<serde_json::Value> {
        [
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "Task",
            "WebFetch",
            "Slack_post",
            "Linear_get",
            "Sentry_get",
            "Notion_read",
            "Snowflake_q",
            "PagerDuty_get",
        ]
        .iter()
        .map(|n| tool(n))
        .collect()
    }

    fn body_with(tools: Vec<serde_json::Value>, messages: serde_json::Value) -> bytes::Bytes {
        bytes::Bytes::from(
            serde_json::to_vec(&json!({
                "model": "claude-opus-5",
                "max_tokens": 64,
                "messages": messages,
                "tools": tools,
            }))
            .unwrap(),
        )
    }

    #[test]
    fn injection_fires_on_first_party_wire_bytes() {
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body,
            "https://api.anthropic.com",
            "claude-opus-5",
            "req-test",
            true,
        );
        let attr = attr.expect("14 first-party tools must defer");
        assert_eq!(attr.deferred_tools, 6);
        assert!(attr.deferred_tokens > 0);
        assert_eq!(attr.core_deferred_tokens, 0);
        assert_eq!(attr.mode, "headroom");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], json!("tool_search_tool_regex"));
        assert_eq!(tools.len(), 15);
    }

    #[test]
    fn injection_disabled_is_byte_identical() {
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body.clone(),
            "https://api.anthropic.com",
            "claude-opus-5",
            "req-test",
            false,
        );
        assert!(attr.is_none());
        assert_eq!(out, body);
    }

    #[test]
    fn tool_search_mode_names_who_deferred() {
        // headroom: we deferred.
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (_, attr) = maybe_inject_tool_search(
            body,
            "https://api.anthropic.com",
            "claude-opus-5",
            "req",
            true,
        );
        assert_eq!(attr.expect("must defer").mode, "headroom");

        // client: the array already carries the server-side shape —
        // stand down and name it, rather than reading as feature-off.
        let mut tools = fourteen_tools();
        tools.push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let body = body_with(tools, json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body.clone(),
            "https://api.anthropic.com",
            "claude-opus-5",
            "req",
            true,
        );
        let attr = attr.expect("client stand-down must still report");
        assert_eq!(attr.mode, "client");
        assert_eq!(attr.deferred_tools, 0);
        assert_eq!(out, body, "stand-down must not rewrite the array");

        // none: too few tools to defer, client not deferring either.
        let body = body_with(
            vec![tool("read"), tool("write")],
            json!([{"role": "user", "content": "hi"}]),
        );
        let (_, attr) = maybe_inject_tool_search(
            body,
            "https://api.anthropic.com",
            "claude-opus-5",
            "req",
            true,
        );
        assert!(attr.is_none(), "nothing happened, nothing to report");
    }

    #[test]
    fn small_tool_array_is_byte_identical() {
        let body = body_with(
            vec![tool("read"), tool("write")],
            json!([{"role": "user", "content": "hi"}]),
        );
        let (out, attr) = maybe_inject_tool_search(
            body.clone(),
            "https://api.anthropic.com",
            "claude-opus-5",
            "req-test",
            true,
        );
        assert!(attr.is_none());
        assert_eq!(out, body);
    }

    #[test]
    fn third_party_strips_search_tools_and_skips_injection() {
        let mut tools = fourteen_tools();
        tools.push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let body = body_with(tools, json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body,
            "https://gateway.internal/v1",
            "claude-opus-5",
            "req-test",
            true,
        );
        let attr = attr.expect("strip must report");
        assert_eq!(attr.stripped_third_party, 1);
        assert_eq!(attr.deferred_tools, 0);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tools"].as_array().unwrap().len(), 14);
    }

    /// An inference-profile ARN names a Bedrock deployment, and Bedrock
    /// rejects the first-party `tool_search_tool_*` + `defer_loading` shape.
    /// Upstream's Python gate sniffs the model id for `arn:` because its
    /// gateway hook sees nothing else; here the selected upstream decides, so
    /// an ARN rides the same third-party strip as any other gateway model.
    /// Pinned so a later change cannot hand Bedrock a shape it will reject.
    #[test]
    fn an_arn_model_on_a_gateway_never_gets_first_party_search() {
        let mut tools = fourteen_tools();
        tools.push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let body = body_with(tools, json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body,
            "https://bedrock-gateway.internal/v1",
            "arn:aws:bedrock:ap-southeast-1:1:application-inference-profile/x57j1es",
            "req-test",
            true,
        );
        let attr = attr.expect("strip must report");
        assert_eq!(attr.stripped_third_party, 1);
        assert_eq!(
            attr.deferred_tools, 0,
            "a Bedrock ARN must never be handed first-party deferral"
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tools"].as_array().unwrap().len(), 14);
    }

    #[test]
    fn repair_is_byte_identical_without_search_blocks() {
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (out, neutralized) = maybe_repair_tool_search_history(body.clone(), "req-test");
        assert_eq!(neutralized, 0);
        assert_eq!(out, body);
    }

    #[test]
    fn repair_neutralizes_unsupported_pair_in_place() {
        let body = body_with(
            vec![tool("read")],
            json!([{
                "role": "assistant",
                "content": [
                    {"type": "server_tool_use", "id": "srv_1", "name": "tool_search_tool_regex"},
                    {"type": "tool_search_tool_result", "tool_use_id": "srv_1",
                     "content": [{"type": "tool_reference", "tool_name": "Slack_post"}]},
                ],
            }]),
        );
        let (out, neutralized) = maybe_repair_tool_search_history(body, "req-test");
        assert_eq!(neutralized, 2);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // Message slot kept, pair replaced with text — never dropped, so
        // signed thinking coordinates downstream are undisturbed.
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(content.iter().all(|b| b["type"] == json!("text")));
    }

    #[test]
    fn ccr_repair_declared_tool_is_byte_identical() {
        let mut tools = vec![tool("read")];
        tools.push(tool("headroom_retrieve"));
        let body = body_with(
            tools,
            json!([{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "r1", "name": "headroom_retrieve",
                     "input": {"hash": "abc"}},
                ],
            }]),
        );
        let (out, neutralized) = maybe_repair_ccr_retrieve_history(body.clone(), "req-test");
        assert_eq!(neutralized, 0);
        assert_eq!(out, body);
    }

    #[test]
    fn ccr_repair_neutralizes_undeclared_pair_in_place() {
        let body = body_with(
            vec![tool("read")],
            json!([
                {"role": "assistant", "content": [
                    {"type": "text", "text": "looking it up"},
                    {"type": "tool_use", "id": "r1", "name": "headroom_retrieve",
                     "input": {"hash": "abc"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "r1",
                     "content": "the original content"},
                ]},
            ]),
        );
        let (out, neutralized) = maybe_repair_ccr_retrieve_history(body, "req-test");
        assert_eq!(neutralized, 2);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // Alternation safety: same messages, same roles, text in place.
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(messages[1]["role"], json!("user"));
        assert_eq!(
            messages[0]["content"][1]["text"],
            json!("[headroom_retrieve call omitted: tool not available this turn]")
        );
        assert_eq!(
            messages[1]["content"][0]["text"],
            json!("the original content")
        );
    }
}

#[cfg(test)]
mod outbound_tool_pairing_tests {
    use super::*;
    use serde_json::json;
    use serde_json::Value;

    fn tool_use(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "bash", "input": {}})
    }

    fn tool_result(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": "ok"})
    }

    fn user_msg(blocks: Vec<Value>) -> Value {
        json!({"role": "user", "content": blocks})
    }

    fn assistant_msg(blocks: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": blocks})
    }

    fn body(messages: Vec<Value>) -> Value {
        json!({"model": "m", "messages": messages})
    }

    #[test]
    fn paired_turn_has_no_offenses() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
            user_msg(vec![tool_result("call_1")]),
            assistant_msg(vec![json!({"type": "text", "text": "done"})]),
        ]);
        assert!(outbound_tool_pairing_offenses(&b).is_empty());
    }

    /// The incident shape: a `tool_use` whose next message carries no result
    /// for it is exactly what the upstream refuses.
    #[test]
    fn orphan_tool_use_is_named_with_index_and_id() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_9")]),
            user_msg(vec![json!({"type": "text", "text": "meanwhile"})]),
            assistant_msg(vec![json!({"type": "text", "text": "done"})]),
        ]);
        assert_eq!(
            outbound_tool_pairing_offenses(&b),
            vec![PairingOffense {
                message_index: 1,
                tool_use_id: "call_9".to_string(),
            }]
        );
    }

    /// A turn may legally end on a tool call — the result arrives next turn.
    #[test]
    fn trailing_tool_use_is_legal() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
        ]);
        assert!(outbound_tool_pairing_offenses(&b).is_empty());
    }

    #[test]
    fn wrong_id_result_is_still_an_orphan() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
            user_msg(vec![tool_result("call_other")]),
        ]);
        assert_eq!(outbound_tool_pairing_offenses(&b).len(), 1);
    }

    /// A result two messages down does not satisfy the next-message rule.
    #[test]
    fn non_immediate_result_is_still_an_orphan() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
            user_msg(vec![json!({"type": "text", "text": "chatter"})]),
            user_msg(vec![tool_result("call_1")]),
        ]);
        assert_eq!(outbound_tool_pairing_offenses(&b).len(), 1);
    }

    #[test]
    fn blocks_without_ids_and_string_content_are_skipped() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![json!({"type": "tool_use", "name": "bash"})]),
            user_msg(vec![json!({"type": "text", "text": "plain string"})]),
        ]);
        assert!(outbound_tool_pairing_offenses(&b).is_empty());
    }

    #[test]
    fn multiple_orphans_are_all_named() {
        let b = body(vec![
            assistant_msg(vec![tool_use("call_1"), tool_use("call_2")]),
            user_msg(vec![tool_result("call_1")]),
            assistant_msg(vec![json!({"type": "text", "text": "done"})]),
        ]);
        assert_eq!(
            outbound_tool_pairing_offenses(&b),
            vec![PairingOffense {
                message_index: 0,
                tool_use_id: "call_2".to_string(),
            }]
        );
    }

    #[test]
    fn body_without_messages_has_no_offenses() {
        assert!(outbound_tool_pairing_offenses(&json!({"model": "m"})).is_empty());
        assert!(outbound_tool_pairing_offenses(&json!({})).is_empty());
    }
}

fn log_prefix_composition(request_id: &str, session_key: &str, body: &[u8]) {
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
    note_tool_roster(session_key, request_id, &names);
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

/// Whether the client asked for a streamed answer.
///
/// Anthropic treats a missing `stream` as false, so an absent key means the
/// caller wants one JSON body. An unreadable body is the one case that reads
/// as `true`: the streaming path is what the proxy did before this check
/// existed, so a body we cannot parse keeps that behaviour rather than
/// converting a stream the client may well have wanted.
fn client_wants_stream(client_body: &bytes::Bytes) -> bool {
    match serde_json::from_slice::<serde_json::Value>(client_body) {
        Ok(v) => v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
        Err(_) => true,
    }
}

/// Read an Anthropic event stream to its end and rebuild the single JSON reply
/// it describes.
///
/// `Err` when the stream never reached `message_stop`, which is the only
/// honest answer for a caller that cannot be handed a partial turn: it asked
/// for one complete message and there is no way to say "half" in that shape.
async fn buffer_sse_as_message<S, E>(stream: S, request_id: &str) -> Result<Vec<u8>, String>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;

    let mut framer = crate::sse::framing::SseFramer::new();
    let mut state = crate::sse::anthropic::AnthropicStreamState::new();
    let mut stream = stream;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
        framer.push(&chunk);
        while let Some(event) = framer.next_event() {
            let event = event.map_err(|e| format!("framing error: {e}"))?;
            if event.is_done_sentinel() {
                continue;
            }
            // A state error means the stream contradicted itself; the rebuilt
            // message would be a guess, so fail instead.
            state
                .apply(event)
                .map_err(|e| format!("stream state error: {e}"))?;
        }
    }

    if state.status != crate::sse::anthropic::StreamStatus::MessageStop {
        return Err(format!(
            "stream ended in {:?} without message_stop",
            state.status
        ));
    }

    let message = crate::sse::ccr_stream::rebuild_message(&state);
    tracing::debug!(
        request_id = %request_id,
        event = "sse_buffered_for_non_streaming_client",
        "rebuilt a non-streaming reply from an event stream"
    );
    serde_json::to_vec(&message).map_err(|e| format!("serialize error: {e}"))
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
/// Cold-prefix fork decision output.
pub(crate) struct ColdFork {
    pub messages: Vec<serde_json::Value>,
    pub transforms: Vec<String>,
    pub ttl_desc: String,
    pub idle_secs: f64,
}

/// Cold-prefix fork (port of upstream `HEADROOM_COLD_RECOMPACT`).
///
/// When the lane has been idle past the request's real cache TTL, the
/// provider cache is dead and the byte-identical splice preserves nothing.
/// Returns `Some` only when the operator opted in AND the turn is cold;
/// warm lanes (and the default-off flag) return `None` so the caller runs
/// the normal replay with byte-identical behavior.
///
/// The caller skips the overlay replay on `Some` (both modes — a dead
/// splice helps nothing); `messages` already carries the lossless
/// whole-prefix recompaction in cache mode, and the untouched originals in
/// token mode (whose recompression already ran with no frozen prefix to
/// preserve).
pub(crate) fn maybe_cold_fork(
    enabled: bool,
    cache_mode: bool,
    model: &str,
    messages: &[serde_json::Value],
    system: Option<&serde_json::Value>,
    idle_secs: Option<f64>,
    ccr_store: Option<std::sync::Arc<dyn headroom_core::ccr::CcrStore>>,
) -> Option<ColdFork> {
    use headroom_core::transforms::cold_prefix as cp;
    if !enabled {
        return None;
    }
    // Authoritative request-level tier (not a guess): the same value that
    // would drive net-cost pricing. `None` = caching disabled = nothing to
    // bust = every turn recompactable.
    let ttl = cp::anthropic_cache_ttl_seconds(model, messages, system);
    if !cp::should_cold_recompact(idle_secs, ttl) {
        return None;
    }
    let idle = idle_secs.unwrap_or(0.0);
    // Cache mode: lossless whole-prefix recompaction, then the Spark
    // reasoning-summary strip (unsigned advisory text only — signed
    // thinking and redacted blocks are untouchable). Token mode skips the
    // dead splice without rewriting: its recompression already ran with no
    // frozen prefix to preserve.
    let (messages, mut transforms) = if cache_mode {
        cp::cold_recompact_messages(messages, ccr_store)
    } else {
        (messages.to_vec(), Vec::new())
    };
    let (messages, transforms) = if cache_mode {
        let (stripped, n) = cp::strip_spark_reasoning_summaries(messages);
        if n > 0 {
            transforms.push(format!("cold:spark_summary:{n}blocks"));
        }
        (stripped, transforms)
    } else {
        (messages, transforms)
    };
    Some(ColdFork {
        messages,
        transforms,
        ttl_desc: ttl
            .map(|t| format!("{t}s"))
            .unwrap_or_else(|| "disabled".to_string()),
        idle_secs: idle,
    })
}

/// Parse the post-dispatch body for prefix replay: the JSON value plus its
/// `messages` array. Returns `None` (forwarding the body unchanged) when
/// the body is not JSON or has no messages array.
/// Extracted from `apply_prefix_replay` without behavior change.
fn parse_replay_body(
    body: &bytes::Bytes,
    request_id: &str,
) -> Option<(serde_json::Value, Vec<serde_json::Value>)> {
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "prefix_replay_skipped",
                request_id = %request_id,
                error = %e,
                "prefix replay: post-dispatch body is not JSON; forwarding unchanged"
            );
            return None;
        }
    };
    let Some(optimized) = parsed.get("messages").and_then(|m| m.as_array()).cloned() else {
        tracing::debug!(
            event = "prefix_replay_skipped",
            request_id = %request_id,
            reason = "no_messages_array",
            "prefix replay: post-dispatch body has no messages array; forwarding unchanged"
        );
        return None;
    };
    Some((parsed, optimized))
}

/// Log a replay decline with the full divergence diagnosis: which reason,
/// where it first disagreed, the block shapes and text kinds on each side,
/// and how much was salvaged anyway. Also tells the observer, so a
/// re-cache event a turn later can name the cause instead of falling
/// through to "unattributable".
/// Extracted from `apply_prefix_replay` without behavior change.
#[allow(clippy::too_many_arguments)]
fn log_replay_decline(
    reason: crate::cache_stabilization::prefix_replay::ReplaySkip,
    prefix_miss: Option<crate::cache_stabilization::prefix_replay::PrefixMiss>,
    prev_orig: Option<&[serde_json::Value]>,
    original_messages: &[serde_json::Value],
    optimized: &[serde_json::Value],
    chain_id: u64,
    session_key: &str,
    request_id: &str,
    observer: Option<&crate::cache_stabilization::usage_observer::UsageObserver>,
    uptime_seconds: u64,
) {
    use crate::cache_stabilization::prefix_replay::ReplaySkip;

    // A turn that does not replay is where the money goes: measured over the
    // 2026-08-08/09 logs, non-replaying turns were 19% of traffic and carried
    // 97% of booked re-cache waste. `replayed_prefix` alone cannot say which of
    // five reasons applied, and they need opposite responses — a diverged
    // client prefix is not our doing, while a turn shorter than the stored
    // prefix means two streams are sharing one session slot.
    // The two heads below share one canonical comparison, so `first_diff_path`
    // and the text it names cannot disagree. Computed here rather than inline
    // in the event for that reason, and only on a decline — a replaying turn
    // never pays for it.
    let (diff_stored_text_head, diff_current_text_head) = match (Some(reason), prev_orig) {
        (
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index, ..
            }),
            Some(prev),
        ) => crate::cache_stabilization::prefix_replay::divergence_text_heads(
            prev,
            original_messages,
            first_diff_index,
        )
        .unwrap_or_default(),
        _ => (String::new(), String::new()),
    };
    if let Some(observer) = observer {
        observer.note_replay_skip(
            request_id,
            crate::cache_stabilization::usage_observer::ReplaySkipEvidence::from_inbound_original_histories(
                reason,
                prev_orig,
                original_messages,
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
        session_key_hash = %crate::cache_stabilization::drift_detector::session_key_log_prefix(session_key),
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
        first_diff_index = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
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
        first_diff_path = match (Some(reason), prev_orig) {
            (
                Some(ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                    ..
                }),
                Some(prev),
            ) => crate::cache_stabilization::prefix_replay::describe_divergence(
                prev,
                original_messages,
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
        replayed_prefix_msgs = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                replayed_prefix_msgs,
                ..
            }) => replayed_prefix_msgs as i64,
            _ => -1,
        },
        // Which block kinds sat on each side of that difference. A
        // `tool_result` that vanished points at something collapsing tool
        // output in front of the client; an ordinary text change points at
        // a real edit. Type names only, never block contents.
        diff_shape_stored = match (Some(reason), prev_orig) {
            (
                Some(ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                    ..
                }),
                Some(prev),
            ) => prev
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::block_type_shape)
                .unwrap_or_default(),
            _ => String::new(),
        },
        diff_shape_current = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index,
                    ..
            }) => original_messages
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::block_type_shape)
                .unwrap_or_default(),
            _ => String::new(),
        },
        // What the text blocks on each side WERE. The shapes above say a
        // `text` block came or went; these say whether it was the client's
        // own ephemeral scaffolding or real content, which is the
        // difference between churn we could neutralise and an edit we must
        // respect. Closed vocabulary, never the text.
        diff_text_kinds_stored = match (Some(reason), prev_orig) {
            (
                Some(ReplaySkip::PrefixContentDiverged {
                    first_diff_index,
                    ..
                }),
                Some(prev),
            ) => prev
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::text_block_kinds)
                .unwrap_or_default(),
            _ => String::new(),
        },
        diff_text_kinds_current = match Some(reason) {
            Some(ReplaySkip::PrefixContentDiverged {
                first_diff_index,
                    ..
            }) => original_messages
                .get(first_diff_index)
                .map(crate::cache_stabilization::prefix_replay::text_block_kinds)
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
        stored_prefix_msgs = prev_orig.map(|o| o.len()).unwrap_or(0),
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

/// Place tail + scaffold + system cache breakpoints within the provider
/// budget. Returns the normalized messages, breakpoints placed, and system
/// markers trimmed. Anthropic counts `cache_control` across `system`,
/// `tools` and `messages` together and refuses the whole request past 4.
/// `system` yields its second marker before the message tail yields either
/// of its two, and the scaffolding marker yields before the tail as well;
/// `tools` is PR-E3's and is never touched.
/// Extracted from `apply_prefix_replay` without behavior change.
fn place_replay_breakpoints(
    parsed: &mut serde_json::Value,
    overlaid: Vec<serde_json::Value>,
    tail_breakpoints: usize,
    request_id: &str,
) -> (Vec<serde_json::Value>, usize, usize) {
    use crate::cache_stabilization::prefix_replay::{
        message_slots_within_budget, place_tail_cache_breakpoints,
        trim_system_breakpoints_to_budget, ANTHROPIC_CACHE_CONTROL_LIMIT,
    };

    // Anthropic counts `cache_control` across `system`, `tools` and `messages`
    // together and refuses the whole request past 4. `system` yields its second
    // marker before the message tail yields either of its two, and the
    // scaffolding marker yields before the tail as well; `tools` is PR-E3's and
    // is never touched.
    let slots = message_slots_within_budget(parsed, &overlaid, tail_breakpoints);
    if slots.tail < tail_breakpoints {
        tracing::warn!(
            event = "cache_marker_budget_clamped",
            request_id = %request_id,
            requested = tail_breakpoints,
            allowed = slots.tail,
            reserved_by_system_and_tools = slots.reserved,
            limit = ANTHROPIC_CACHE_CONTROL_LIMIT,
            "cache_control budget: placing fewer message breakpoints than asked \
             to keep the request under the provider's limit"
        );
    }
    let (normalized, breakpoints_placed) =
        place_tail_cache_breakpoints(overlaid, slots.tail, slots.scaffold);
    // Pay for the scaffolding marker out of `system`, and do it from what is
    // really on the body rather than from what was planned, so a plan that did
    // not come off cannot leave the request over the limit.
    let system_markers_trimmed = trim_system_breakpoints_to_budget(parsed, breakpoints_placed);
    if system_markers_trimmed > 0 {
        tracing::debug!(
            event = "system_marker_yielded",
            request_id = %request_id,
            dropped = system_markers_trimmed,
            "gave up a system breakpoint so the message tail keeps both of its own"
        );
    }
    (normalized, breakpoints_placed, system_markers_trimmed)
}

/// Report which messages this proxy altered before forwarding, singling
/// out altered ones carrying thinking blocks (a rejection at those indices
/// means the proxy caused it) plus signed blocks altered on the wire
/// (refused outright — always a defect). Indices and counts only.
/// Anthropic refuses a turn whose signed `thinking` blocks changed, naming a
/// message index but not who changed it. Both the client and this proxy
/// rewrite history, so a rejection is unattributable without knowing which
/// messages WE altered.
/// Extracted from `apply_prefix_replay` without behavior change.
fn note_rewritten_messages(
    original_messages: &[serde_json::Value],
    normalized: &[serde_json::Value],
    request_id: &str,
) {
    // Anthropic refuses a turn whose signed `thinking` blocks changed, naming a
    // message index but not who changed it. Both the client and this proxy
    // rewrite history, so a rejection is unattributable without knowing which
    // messages WE altered. Report that, and single out the altered ones that
    // carry a thinking block — if a rejection's index appears here, the proxy
    // caused it. Indices and counts only.
    let rewritten = rewritten_message_report(original_messages, normalized);
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
            early_fingerprints = %crate::cache_stabilization::prefix_replay::early_message_fingerprints(normalized, 5),
            "messages this proxy altered before forwarding"
        );
    }
}

/// Serialize the replayed body back onto `parsed["messages"]`, booking the
/// applied replay with the observer and logging the placement. On
/// serialization failure forwards the pre-replay body (recording what was
/// ACTUALLY forwarded — the store must mirror the wire — never the
/// messages that failed to serialize).
/// Extracted from `apply_prefix_replay` without behavior change.
#[allow(clippy::too_many_arguments)]
fn serialize_replayed_body(
    changed: bool,
    parsed: &mut serde_json::Value,
    normalized: Vec<serde_json::Value>,
    optimized: Vec<serde_json::Value>,
    body: bytes::Bytes,
    replayed_prefix: bool,
    observer: Option<&crate::cache_stabilization::usage_observer::UsageObserver>,
    chain_id: u64,
    breakpoints_placed: usize,
    system_markers_dropped: usize,
    request_id: &str,
) -> (bytes::Bytes, Vec<serde_json::Value>) {
    if !changed {
        return (body, optimized);
    }
    parsed["messages"] = serde_json::Value::Array(normalized.clone());
    match serde_json::to_vec(parsed) {
        Ok(b) => {
            if replayed_prefix {
                if let Some(observer) = observer {
                    observer.note_replay_applied(
                        request_id,
                        crate::cache_stabilization::usage_observer::ReplayAppliedEvidence::new(
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
}

#[allow(clippy::too_many_arguments)]
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
        overlay_cached_prefix_reported, strip_system_cache_control,
    };

    let Some((mut parsed, optimized)) = parse_replay_body(&body, request_id) else {
        return body;
    };

    // Ask for the prefix THIS turn continues, not merely the session's last
    // one: several streams share a session key, and handing back another
    // stream's prefix guarantees the append-only guard rejects it and the turn
    // forwards fresh bytes over content the provider had cached. The system
    // digest gates the answer on the prefix lineage the provider actually
    // holds: replaying stored messages under a different system is a miss
    // that reports as a replay.
    let current_system_hash =
        cache_stabilization::prefix_replay::forwarded_system_digest(parsed.get("system"));
    let (prev_orig, prev_fwd, prefix_miss, chain_id) = match store.previous_turn_for(
        session_key,
        &original_messages,
        Some(&current_system_hash),
    ) {
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
        // Provider-confirmed floor (port of upstream `aebe9895`): the leading
        // messages the provider confirmed cached replay unconditionally, so a
        // background recompression landing a smaller form of already-forwarded
        // history cannot bust the warm cache; beyond the floor the
        // non-inflation bound still lets improvements through, and a cold
        // cache (count 0) lets every accumulated improvement land at once.
        Some(store.confirmed_frozen_count(session_key)),
    );
    let replayed_prefix = overlaid != optimized;
    if let Some(reason) = skip_reason {
        log_replay_decline(
            reason,
            prefix_miss,
            prev_orig.as_deref(),
            &original_messages,
            &optimized,
            chain_id,
            session_key,
            request_id,
            observer,
            uptime_seconds,
        );
    }
    let (normalized, breakpoints_placed, system_markers_trimmed) =
        place_replay_breakpoints(&mut parsed, overlaid, tail_breakpoints, request_id);
    note_rewritten_messages(&original_messages, &normalized, request_id);
    // Only once a message breakpoint is in place. With none placed the client's
    // system markers are the only ones on the request, and dropping them would
    // turn caching off rather than move it.
    let system_markers_dropped = if strip_system_breakpoints && breakpoints_placed > 0 {
        strip_system_cache_control(&mut parsed)
    } else {
        0
    };
    let changed =
        normalized != optimized || system_markers_dropped > 0 || system_markers_trimmed > 0;

    let (final_body, forwarded_messages) = serialize_replayed_body(
        changed,
        &mut parsed,
        normalized,
        optimized,
        body,
        replayed_prefix,
        observer,
        chain_id,
        breakpoints_placed,
        system_markers_dropped,
        request_id,
    );

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
        cache_stabilization::prefix_replay::forwarded_system_digest(parsed.get("system")),
    );
    final_body
}

/// PR-E4: OpenAI `prompt_cache_key` auto-injection helper.
///
/// Re-serialise after a key injection. If serialization fails (would be
/// very unusual — the body just parsed successfully), fall back to the
/// original bytes. No-silent-fallback rule: log it loudly so a regression
/// can't hide.
/// Extracted from `finish_key_injection` without behavior change.
fn serialize_injected_key(
    parsed: &serde_json::Value,
    body: bytes::Bytes,
    key_prefix: &str,
    request_id: &str,
    path: &str,
) -> bytes::Bytes {
    match serde_json::to_vec(parsed) {
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

/// Log a key-injection skip: the customer-visible KeyPresent skip at info;
/// the NotAnObject skip (structurally impossible past the dispatcher gate)
/// is surfaced separately for operators chasing pathological inputs.
/// Extracted from `finish_key_injection` without behavior change.
fn note_key_injection_skip(
    reason: cache_stabilization::openai_cache_key::SkipReason,
    request_id: &str,
    path: &str,
) {
    use cache_stabilization::openai_cache_key::SkipReason;

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
}

/// Finish a key injection: re-serialise on `Applied` (loudly falling back
/// to the original bytes on failure), log the skip reason on `Skipped`.
/// Extracted from `maybe_inject_openai_prompt_cache_key` without behavior
/// change.
fn finish_key_injection(
    outcome: cache_stabilization::openai_cache_key::InjectOutcome,
    parsed: &serde_json::Value,
    body: bytes::Bytes,
    request_id: &str,
    path: &str,
) -> bytes::Bytes {
    use cache_stabilization::openai_cache_key::InjectOutcome;

    match outcome {
        InjectOutcome::Applied { key_prefix } => {
            serialize_injected_key(parsed, body, &key_prefix, request_id, path)
        }
        InjectOutcome::Skipped { reason } => {
            note_key_injection_skip(reason, request_id, path);
            body
        }
    }
}

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
    use cache_stabilization::openai_cache_key::inject_prompt_cache_key;

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

    let outcome = inject_prompt_cache_key(&mut parsed, shape);
    finish_key_injection(outcome, &parsed, body, request_id, path)
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
    /// Conversation identity for novel-vs-repeat savings attribution
    /// (upstream `427fa76f`). `Some` only when the request carries a whole
    /// transcript under an explicit conversation id (`/v1/responses`
    /// shape); `None` keeps ordinary per-request accounting, which is
    /// already novel-only on frozen-prefix paths.
    conversation_key: Option<String>,
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

/// Book a completed OpenAI-shaped stream.
///
/// Chat Completions and the Responses API report the same three numbers under
/// different names; once they have been read, the outcome is identical, and the
/// two copies of this literal were byte-for-byte the same bar a comment. The
/// Anthropic arm is deliberately not folded in — it also carries cache-write
/// tiers, a wire footprint, and CCR continuation totals that neither OpenAI
/// shape has.
fn emit_openai_stream_outcome(
    ctx: &OutcomeContext,
    request_id: &str,
    ttfb_ms: f64,
    input_tok: i64,
    cached_tok: i64,
    output_tok: i64,
    // Upstream 4949cd55: the HTTP status the stream arrived with. The old
    // code left this at the `Default` 0 (success), so an exhausted 529
    // served as `text/event-stream` booked a success at stream close. A
    // real >= 500 diverts through the failure funnel in
    // `emit_request_outcome`; anything else behaves exactly as before.
    status_code: i64,
) {
    let outcome = headroom_core::request_outcome::RequestOutcome {
        request_id: request_id.to_string(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        original_tokens: ctx.sizes(input_tok).0,
        optimized_tokens: ctx.sizes(input_tok).1,
        output_tokens: output_tok,
        tokens_saved: ctx.tokens_saved,
        conversation_key: ctx.conversation_key.clone(),
        conversation_tokens_saved: Some(ctx.tokens_saved),
        attempted_input_tokens: ctx.attempted(input_tok),
        cache_read_tokens: cached_tok,
        // Both providers report a total that includes the cached prefix, unlike
        // Anthropic's `input_tokens`, so the uncached remainder is a subtraction.
        uncached_input_tokens: input_tok.saturating_sub(cached_tok),
        total_latency_ms: ctx.total_latency_ms,
        overhead_ms: ctx.overhead_ms,
        ttfb_ms,
        transforms_applied: ctx.transforms_applied.clone(),
        num_messages: ctx.num_messages,
        tags: ctx.tags.clone(),
        client: ctx.client.clone(),
        project: ctx.project.clone(),
        status_code,
        ..Default::default()
    };
    headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
}

/// How long to wait before retry number `attempt` (0-based), in milliseconds.
///
/// Every retry site in the proxy used to inline this formula, and the copies
/// had drifted: two jittered, the in-band-SSE branch did not, and the local
/// model's transport branch ignored the configured base entirely and applied no
/// ceiling. `headroom_core::retry::jitter_delay_ms` was written to be the one
/// copy and had no caller at all. Jitter is not decoration here — an overloaded
/// upstream returns 529 to every in-flight request at once, and unjittered
/// backoff sends them all back in the same millisecond.
pub(crate) fn backoff_ms(state: &AppState, attempt: u32) -> u64 {
    headroom_core::retry::jitter_delay_ms(
        state.config.retry_base_delay_ms as i64,
        state.config.retry_max_delay_ms as i64,
        attempt,
    ) as u64
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
#[allow(clippy::too_many_arguments)]
async fn run_sse_state_machine(
    kind: SseStreamKind,
    rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    request_id: String,
    usage_observer: Arc<cache_stabilization::usage_observer::UsageObserver>,
    outcome_ctx: Option<OutcomeContext>,
    replay_store: Option<SessionReplayStore>,
    // Usage of CCR continuation rounds the client never saw, filled in by
    // `sse::ccr_stream` before this task's channel closes. `None` when the
    // rewriter did not run.
    ccr_round_usage: Option<Arc<Mutex<CcrRoundUsage>>>,
    // HTTP status the stream arrived with (upstream 4949cd55). The close
    // arms stamp it onto the outcome so a 5xx served as SSE books failed
    // instead of success. `Copy`, so the spawn site just moves it in.
    upstream_status: StatusCode,
) {
    use crate::sse::framing::SseFramer;

    let framer = SseFramer::new();
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
            let state = sse_anthropic::drive_anthropic_stream(
                rx,
                framer,
                &request_id,
                &outcome_ctx,
                &mut ttfb_ms,
            )
            .await;
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
            let close = sse_anthropic::AnthropicClose {
                state: &state,
                ccr_rounds,
                cache_baseline_input,
                cache_baseline_read,
                cache_baseline_write,
                usage_observer: &usage_observer,
                outcome_ctx: &outcome_ctx,
                replay_store: &replay_store,
                request_id: &request_id,
                ttfb_ms,
                upstream_status,
            };
            sse_anthropic::note_anthropic_billed_totals(&close);
            sse_anthropic::note_anthropic_hit_rate(&close);
            sse_anthropic::complete_anthropic_watchdog(&close);
            sse_anthropic::complete_anthropic_replay(&close);
            sse_anthropic::log_anthropic_close(&close);
            sse_anthropic::book_anthropic_incomplete(&close);
            sse_anthropic::emit_anthropic_outcome(&close);
        }
        SseStreamKind::OpenAiChat => {
            let state = sse_openai::drive_openai_chat_stream(
                rx,
                framer,
                &request_id,
                &outcome_ctx,
                &mut ttfb_ms,
            )
            .await;
            sse_openai::close_openai_chat_stream(
                &state,
                &request_id,
                ttfb_ms,
                &outcome_ctx,
                upstream_status,
            );
        }
        SseStreamKind::OpenAiResponses => {
            let state = sse_openai::drive_openai_responses_stream(
                rx,
                framer,
                &request_id,
                &outcome_ctx,
                &mut ttfb_ms,
            )
            .await;
            sse_openai::close_openai_responses_stream(
                &state,
                &request_id,
                ttfb_ms,
                &outcome_ctx,
                upstream_status,
            );
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

/// Conversation-stable session key for the redaction seam on passthrough
/// paths. Per-request ids mint a fresh token (and rotate the provider
/// prefix) for the same secret every turn; the conversation key keeps one
/// turn's placeholders valid for the next, matching the routed path's
/// `prepared.redact_session_key`. Falls back to [`ensure_request_id`] when
/// the body is not JSON. Only call when redaction is on: deriving costs the
/// same canonical-hash pass the drift detector pays, and doing it twice per
/// request (here + `forward_http`) would put that on every hot path.
pub(crate) fn redact_session_key(
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    body: &bytes::Bytes,
    kind: ApiKind,
) -> String {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(parsed) => derive_session_key(headers, client_addr, &parsed, kind),
        Err(_) => ensure_request_id(headers),
    }
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
/// them. Returns the body unchanged on any parse/serialize failure, plus the
/// tool-schema tokens hooks removed (measured on the final tools object, so
/// in-place shrinks count; growth clamps to zero). Callers MUST gate on a
/// non-empty registry so the empty-registry path is a byte-identical no-op
/// (this fn re-serializes and would perturb bytes).
pub(crate) fn apply_request_hooks(
    body: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    request_id: &str,
) -> (bytes::Bytes, i64) {
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
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

    // A hook may shrink the tool array either by replacing it or in place,
    // so the baseline is counted BEFORE the hooks run and the saving off the
    // FINAL tools object. Deferral-shaped (removes schemas counting never
    // saw), hence a tag rather than a fold — mirrors upstream
    // `handlers/anthropic.py` + the OpenAI chat path.
    let tok = headroom_core::tokenizer::get_tokenizer(&model);
    let count_tools = |t: &Option<serde_json::Value>| -> i64 {
        match t {
            Some(v) => serde_json::to_string(v)
                .map(|s| tok.count_text(&s) as i64)
                .unwrap_or(0),
            None => 0,
        }
    };

    let mut ctx = crate::turn_hooks::TurnContext {
        provider: turn_hook_provider(endpoint).to_string(),
        model,
        messages,
        tools,
        config: None,
    };
    let tools_before = count_tools(&ctx.tools);
    crate::turn_hooks::run_request_hooks(&mut ctx);
    let tools_saved = tools_before.saturating_sub(count_tools(&ctx.tools));

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
        Ok(v) => (bytes::Bytes::from(v), tools_saved),
        Err(e) => {
            tracing::warn!(event = "turn_hooks_reserialize_failed", request_id = %request_id, error = %e, "turn hooks: re-serialize failed; forwarding original body");
            (body, 0)
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
                tracing::warn!(event = "turn_hooks_call_model_serialize_failed", request_id = %self.request_id, error = %e, "turn hooks call_model: serialize failed");
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
                    tracing::warn!(event = "turn_hooks_call_model_read_failed", request_id = %self.request_id, error = %e, "turn hooks call_model: read body failed");
                    serde_json::Value::Null
                }
            },
            Err(e) => {
                tracing::warn!(event = "turn_hooks_call_model_upstream_failed", request_id = %self.request_id, error = %e, "turn hooks call_model: upstream request failed");
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
#[allow(clippy::too_many_arguments)]
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
/// Move the message breakpoint onto the tail of a continuation body.
///
/// The first round's marker sits on what was then the last block. Each round
/// appends an assistant turn and its tool results behind it, so without this
/// the marker drifts backwards through the request and everything after it is
/// written fresh on the round that follows.
///
/// `push_newest_marker_to_tail` relocates the existing marker object rather
/// than adding one, so calling it every round cannot breach Anthropic's cap of
/// four and carries whatever TTL the pin gave it. With two tail slots only the
/// newer marker moves; the older one stays on the prefix the provider already
/// holds. It returns false when the marker is already at the tail.
///
/// Anthropic only: no other shape here has `cache_control`.
fn retail_continuation_breakpoint(
    request: &mut serde_json::Value,
    provider: &str,
    config: &Config,
    request_id: &str,
    round: usize,
) {
    if provider != "anthropic" || !config.cache_tail_breakpoint {
        return;
    }
    if cache_stabilization::message_breakpoints::push_newest_marker_to_tail(request) {
        tracing::debug!(
            request_id = %request_id,
            event = "continuation_tail_breakpoint",
            round = round,
            "moved the message breakpoint onto the continuation tail"
        );
    }
}

/// Map a continuation handler's provider label onto the drift detector's
/// shape enum. `None` for a label neither knows, which skips the check
/// rather than hashing a body under the wrong shape rules.
fn continuation_api_kind(provider: &str) -> Option<ApiKind> {
    match provider {
        "anthropic" => Some(ApiKind::Anthropic),
        "openai" => Some(ApiKind::OpenAiChat),
        "openai_responses" => Some(ApiKind::OpenAiResponses),
        _ => None,
    }
}

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

/// Read a continuation response into the turn JSON the CCR machinery speaks.
///
/// A routed continuation comes back as JSON, which parses directly. Anthropic
/// continuations stream, and so do those on a Responses backend that mandates
/// it (the chatgpt codex gateway answers `stream: false` with `400 Stream must
/// be set to true`); both answer SSE, which `serde_json` cannot read — fold it
/// back into a turn first, in the shape the provider speaks. JSON-first, so
/// a JSON body never changes shape no matter its content type; the fold only
/// runs when plain parsing already failed. Returns `None` when the body is
/// neither, and the caller ends the round as it always has.
/// True when a body opens like an SSE stream (`event:` or `data:` field,
/// after any leading blank lines), for responses that carry no Content-Type.
fn looks_like_sse(body: &[u8]) -> bool {
    let head = &body[..body.len().min(64)];
    let head = std::string::String::from_utf8_lossy(head);
    let first = head.trim_start_matches(['\r', '\n']);
    first.starts_with("event:") || first.starts_with("data:")
}

/// Whether a continuation body arrived as SSE (vs buffered JSON): the
/// Content-Type header wins when present, otherwise sniff the body. Shared
/// by the fold and the cut-stream check so they agree on what "should have
/// folded" for a given body.
fn continuation_body_is_sse(body: &[u8], content_type: Option<&str>) -> bool {
    match content_type.map(str::trim).filter(|ct| !ct.is_empty()) {
        Some(ct) => ct.contains("text/event-stream"),
        None => looks_like_sse(body),
    }
}

/// Truncation signature for buffered-JSON continuations (the `openai` chat
/// shape folds JSON only): a body that is not valid JSON and does not end
/// like one was cut mid-write. A complete-but-unparseable body (ends with
/// `}` or `]`) is deterministic garbage — resending it would fail the same
/// way. An empty body is never a valid turn.
fn continuation_json_looks_truncated(body: &[u8]) -> bool {
    match body.iter().rposition(|b| !b.is_ascii_whitespace()) {
        None => true,
        Some(i) => !matches!(body[i], b'}' | b']'),
    }
}

/// Whether a continuation body that failed to fold is worth resending
/// identical, same round: terminal-less SSE streams and truncated JSON are
/// transport cuts in substance. Explicit verdicts, deterministic garbage,
/// and unknown provider shapes are not. Budget is enforced by the caller —
/// CCR and memory continuations keep separate per-turn counters.
fn continuation_cut_retryable(body: &[u8], content_type: &str, provider: &str) -> bool {
    match provider {
        "openai_responses" | "anthropic" => {
            continuation_stream_terminal(body, provider).is_none()
                && continuation_body_is_sse(body, Some(content_type))
        }
        // Chat completions fold JSON only: truncated JSON (not ending like
        // a complete value) reads as cut mid-write.
        "openai" => continuation_json_looks_truncated(body),
        _ => false,
    }
}

/// Terminal marker of a streamed continuation body, if any. A 200 whose SSE
/// body carries no terminal event ended mid-generation (cut stream): on
/// 2026-09-17 a gpt-5.6-luna reasoning continuation landed as ~200 KB of
/// reasoning deltas followed by EOF, folded to zero blocks, and the turn
/// went quiet on a fallback splice. That shape is transport failure in
/// substance and worth resending. An explicit failed/incomplete verdict is
/// deterministic — the identical re-send would fail the same way — so it
/// must NOT retry. Returns None for non-SSE-fold providers and for bodies
/// with no terminal marker.
pub(crate) fn continuation_stream_terminal(body: &[u8], provider: &str) -> Option<&'static str> {
    if !matches!(provider, "openai_responses" | "anthropic") {
        return None;
    }
    // N.B. the caller already established SSE; this only classifies it.
    // `event:` names and `"type":` values share these strings, so one
    // substring scan covers both framings (including the bare/Codex forms).
    let text = std::string::String::from_utf8_lossy(body);
    if provider == "openai_responses" {
        for marker in [
            "response.failed",
            "response.incomplete",
            "response.completed",
        ] {
            if text.contains(marker) {
                return Some(match marker {
                    "response.failed" => "failed",
                    "response.incomplete" => "incomplete",
                    _ => "completed",
                });
            }
        }
        return None;
    }
    // Anthropic turns end on message_delta; without it the fold has no
    // stop_reason and yields nothing usable.
    if text.contains("message_delta") {
        Some("message_delta")
    } else {
        None
    }
}

fn continuation_turn_from_body(
    body: &bytes::Bytes,
    content_type: Option<&str>,
    provider: &str,
) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        return Some(v);
    }
    if !matches!(provider, "openai_responses" | "anthropic") {
        return None;
    }
    // A missing Content-Type is not "not SSE": on 2026-09-14 eleven
    // continuations on the gpt-5.6 Responses route came back with no
    // Content-Type at all and a 150-180 KB non-JSON body, and refusing the
    // fold here replaced the whole turn with the retrieval-failure note.
    // Sniff the body when the header is absent; a wrong header still wins.
    if !continuation_body_is_sse(body, content_type) {
        return None;
    }
    if provider == "anthropic" {
        return crate::sse::ccr_stream::anthropic_stream_to_turn(body);
    }
    let text = std::string::String::from_utf8_lossy(body);
    let (turn, _) = crate::openai::response::responses_stream_to_turn(&text);
    let has_blocks = turn
        .get("output")
        .and_then(|o| o.as_array())
        .is_some_and(|o| !o.is_empty());
    if has_blocks {
        Some(turn)
    } else {
        None
    }
}

/// Read a continuation's response body, failing on silence rather than on
/// elapsed time. `reqwest::Response::bytes` has neither bound, so a streamed
/// continuation that dies mid-body would otherwise sit until the 600s client
/// timeout. The error string is for the log line at the call site.
async fn read_continuation_body(mut resp: reqwest::Response) -> Result<bytes::Bytes, String> {
    let mut buf = bytes::BytesMut::new();
    loop {
        match tokio::time::timeout(CCR_CONTINUATION_IDLE_TIMEOUT, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => buf.extend_from_slice(&chunk),
            Ok(Ok(None)) => return Ok(buf.freeze()),
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => {
                return Err(format!(
                    "no body chunk within {}s",
                    CCR_CONTINUATION_IDLE_TIMEOUT.as_secs()
                ));
            }
        }
    }
}

/// The compression pipeline is not re-applied to continuation requests, and
/// must not be: `forwarded_request` is the body that already went upstream,
/// transforms and all, so each round only appends the assistant turn and its
/// tool results to a prefix the provider has already cached.
///
/// It has to be the forwarded body for that to hold. Passing the client's raw
/// request instead — which this did until 2026-08-22 — drops every injected
/// tool, every offloaded block and any routed model, so the continuation
/// presents a prefix the provider never saw and every round after a
/// transformed turn misses cache.
/// Prefix for content rebuilt from the FTS index after the CCR store expired
/// it. Indexing splits a block into chunks and keeps no separator, so a source
/// that chunked into more than one piece rejoins approximately. Saying so is
/// the difference between the model treating a near-copy as exact and it
/// knowing to re-read when the exact bytes matter.
const CCR_INDEX_RECOVERY_NOTE: &str = "[Recovered from the context index. \
     The original expired from the retrieval store, so this was rebuilt from \
     the indexed copy: the text is complete but whitespace between sections \
     may differ from what you first read. Re-read the source if you need the \
     exact bytes.]";

// Shared CCR hash check lives in `headroom_core::ccr::response_handler`
// so the proxy, batch, and streaming paths agree on what counts as
// malformed (see `is_plausible_ccr_hash` there). No local copy.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_ccr_response(
    body_bytes: &bytes::Bytes,
    forwarded_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    ccr_store: &dyn headroom_core::ccr::CcrStore,
    stores: Option<&std::sync::Arc<crate::ctx::projects::ProjectStores>>,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
    redact: Option<crate::redact::RedactRef>,
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
    let Some(response) = ccr_response::parse_ccr_response(body_bytes, request_id) else {
        return (body_bytes.clone(), round_usage);
    };

    if !handler.has_ccr_tool_calls(&response, provider) {
        return (body_bytes.clone(), round_usage);
    }

    let mut current_response = response.clone();
    let Some(mut current_request) = ccr_response::parse_ccr_request(forwarded_request, request_id)
    else {
        return (body_bytes.clone(), round_usage);
    };

    // The hot zone as the provider cached it on the first round. Every
    // continuation is checked against this, since none of them reach the
    // forwarding path where outbound drift is observed.
    let base_kind = continuation_api_kind(provider);
    let base_hash = base_kind.map(|kind| compute_structural_hash(&current_request, kind));

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
    // Cut-stream retries spent resending an identical continuation whose SSE
    // body ended with no terminal event (see continuation_stream_terminal).
    // Per-turn, not per-round: a sick route must fail fast to fallback (a)
    // rather than multiply re-sends across rounds.
    let mut cut_attempts: u32 = 0;
    // Last successfully fetched retrieval content, kept across rounds for
    // fallback (a): if a later round's upstream continuation dies, the turn
    // still resolves with what the store already returned.
    let mut last_fetched: Vec<headroom_core::ccr::response_handler::CcrToolResult> = Vec::new();

    loop {
        if !ccr_response::check_ccr_round_budget(rounds, max_rounds, request_id) {
            break;
        }

        let (ccr_calls, other_calls) = handler.parse_ccr_tool_calls(&current_response, provider);

        if ccr_calls.is_empty() {
            break;
        }

        // Fetch original content for each CCR call.
        let mut results: Vec<CcrToolResult> = Vec::new();
        for call in &ccr_calls {
            results.push(
                ccr_response::fetch_one_ccr_call(
                    call,
                    ccr_store,
                    stores,
                    outgoing_headers,
                    &current_request,
                    config,
                    &stale_reads,
                    &redact,
                    request_id,
                    rounds,
                )
                .await,
            );
        }

        // Mixed CCR + real tool calls, and all-failed rounds, splice in
        // place when possible; otherwise the loop continues below.
        if ccr_response::check_ccr_round_fate(
            &handler,
            &mut current_response,
            &results,
            ccr_calls.len(),
            other_calls.len(),
            provider,
            request_id,
        ) {
            break;
        }

        // Snapshot successes for fallback (a): `results` is rebuilt each
        // round, but the splice site below is outside the loop.
        last_fetched = results
            .iter()
            .filter(|r| r.success && !r.content.is_empty())
            .cloned()
            .collect();
        // Build continuation messages: append assistant message + tool results.
        let assistant_msg = handler.extract_assistant_message(&current_response, provider);
        let tool_result_msg = handler.create_tool_result_message(&results, provider);

        let Some(continuation_body) = ccr_response::build_ccr_continuation(
            &mut current_request,
            items_field,
            assistant_msg,
            tool_result_msg,
            provider,
            config,
            request_id,
            rounds,
        ) else {
            break;
        };

        // A continuation that fails is not a soft outcome: the retrieval is
        // already parsed and the content already fetched, and giving up here
        // leaves the model with an unanswered `headroom_retrieve`.
        let continuation_started = std::time::Instant::now();
        let base = base_hash.as_ref().zip(base_kind);
        let send = ccr_response::send_ccr_continuation(
            client,
            upstream_url,
            outgoing_headers,
            continuation_body,
            &results,
            max_rounds,
            base,
            &current_request,
            request_id,
            rounds,
        )
        .await;
        let attempts = send.attempts;
        let Some(resp) = ccr_response::unwrap_ccr_send(
            send.outcome,
            attempts,
            &continuation_started,
            request_id,
        ) else {
            break;
        };
        let Some(resp) = ccr_response::check_ccr_response_status(
            resp,
            provider,
            rounds,
            attempts,
            &continuation_started,
            request_id,
        )
        .await
        else {
            break;
        };

        // `bytes()` consumes the response; the fold needs the body.
        match ccr_response::read_ccr_round_body(
            resp,
            provider,
            &mut cut_attempts,
            &mut round_usage,
            &current_response,
            request_id,
        )
        .await
        {
            ccr_response::CcrRoundRead::Advance(next) => {
                current_response = next;
            }
            ccr_response::CcrRoundRead::Retry => continue,
            ccr_response::CcrRoundRead::Done => break,
        }

        rounds += 1;
    }

    // Classify the outcome before returning. Only claim success when no
    // `headroom_retrieve` remains; a retrieve left standing is a real failure.
    ccr_response::resolve_ccr_residual(
        &handler,
        &mut current_response,
        provider,
        &last_fetched,
        request_id,
    );

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
pub(crate) async fn memory_tool_context(
    state: &AppState,
    headers_snapshot: &Option<HeaderMap>,
    provider: Option<&str>,
    request_body: &bytes::Bytes,
) -> Option<MemoryToolContext> {
    let handler = state.memory_handler.as_ref()?;
    if !handler.is_initialized() {
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
            project_root_override: state.config.memory_project_root.clone(),
        },
    );
    Some(MemoryToolContext {
        handler: handler.clone(),
        provider,
        user_id,
    })
}

/// What a memory continuation needs. Assembled at the seam that has the
/// request in scope, the same way [`crate::routed::ccr::RoutedCcr`]
/// is.
pub(crate) struct MemoryToolContext {
    pub handler: Arc<crate::memory::handler::MemoryHandler>,
    pub provider: crate::memory::tool_adapter::Provider,
    pub user_id: String,
}

/// Fetch one round's memory tool results, or `None` when the turn has no
/// memory calls outstanding (loop ends).
/// Extracted from `handle_memory_response` without behavior change.
async fn fetch_memory_round_calls(
    memory: &MemoryToolContext,
    current_response: &serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    let handler = memory.handler.as_ref();
    if !handler.has_memory_tool_calls(current_response, memory.provider) {
        return None;
    }
    let results = handler
        .handle_memory_tool_calls(current_response, &memory.user_id, memory.provider, None)
        .await;
    if results.is_empty() {
        return None;
    }
    Some(results)
}

/// Redact memory answers before they join the continuation: they come
/// from the local store, which captured the client's real text.
/// Extracted from `handle_memory_response` without behavior change.
fn redact_memory_results(
    redact: &Option<crate::redact::RedactRef>,
    results: &mut [serde_json::Value],
) {
    if let Some(r) = redact.as_ref() {
        for res in results.iter_mut() {
            crate::redact::redact_value(r, res);
        }
    }
}

/// What one memory round read decided: the next turn JSON, a same-round
/// retry, or the end of the loop.
/// Extracted from `handle_memory_response` without behavior change.
enum MemoryRoundRead {
    Advance(serde_json::Value),
    Retry,
    Done,
}

/// A client tool call sharing the turn makes a continuation impossible: it
/// would send upstream an assistant turn whose client `tool_use` has no
/// `tool_result` — the client has not run it yet — and upstream rejects the
/// whole request, losing the memory answer with it. Answer the memory call
/// now and hold the answer for the next request, which carries that result.
/// Anthropic only: the holding area works in Anthropic block shapes.
/// Returns true when the turn was deferred (caller leaves it alone so the
/// client's own tool call reaches it untouched).
/// Extracted from `handle_memory_response` without behavior change.
async fn defer_mixed_memory_turn(
    response: &serde_json::Value,
    memory: &MemoryToolContext,
    request_id: &str,
    provider: &str,
) -> bool {
    if provider != "anthropic" {
        return false;
    }
    let (ours, client_ids) = crate::memory::deferred::split_tool_calls(response);
    if ours.is_empty() || client_ids.is_empty() {
        return false;
    }
    let results = {
        let handler = memory.handler.as_ref();
        handler
            .handle_memory_tool_calls(response, &memory.user_id, memory.provider, None)
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
    true
}

/// Send one memory continuation with retry: transport blips and 429/5xx
/// get another attempt; anything else is a body we built wrong, so the
/// caller keeps what upstream objected to instead of dropping it.
/// A failed continuation takes the memory call down with it: the block
/// is already suppressed, so the tool the model asked for never runs and
/// the turn reaches the client short one tool call.
/// Extracted from `handle_memory_response` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn send_memory_continuation(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    continuation_body: Vec<u8>,
    request_id: &str,
    round: usize,
    items_field: &str,
    current_request: &serde_json::Value,
) -> Option<reqwest::Response> {
    let mut attempt: u32 = 0;
    // Same presence gate `refresh_zen_request_id` uses: the header is on
    // the map iff this continuation is a Zen-route send.
    let zen_route = outgoing_headers.contains_key("x-opencode-request");
    loop {
        let sent = forward::send_memory_continuation_once(
            client,
            upstream_url,
            outgoing_headers,
            &continuation_body,
        )
        .await;
        match forward::classify_memory_send(
            sent,
            attempt,
            request_id,
            round,
            items_field,
            current_request,
            zen_route,
        )
        .await
        {
            forward::MemorySendOutcome::Done(resp) => break resp,
            forward::MemorySendOutcome::Next(next) => {
                attempt = next;
                continue;
            }
        }
    }
}

/// Body stall after 200 headers: transport-cut class, same as the CCR
/// path. Bounded; exhaustion ends the loop as before.
/// Extracted from `read_memory_round_body` without behavior change.
async fn note_memory_body_stall(
    error: String,
    mem_cut_attempts: &mut u32,
    request_id: &str,
) -> MemoryRoundRead {
    if *mem_cut_attempts < MEMORY_CONTINUATION_RETRIES {
        *mem_cut_attempts += 1;
        tracing::warn!(
            event = "memory_continuation_body_unreadable",
            request_id = %request_id,
            error = %error,
            cut_attempt = *mem_cut_attempts,
            "memory: failed to read continuation response body; retrying same round"
        );
        tokio::time::sleep(memory_continuation_backoff(*mem_cut_attempts)).await;
        return MemoryRoundRead::Retry;
    }
    tracing::warn!(
        event = "memory_continuation_body_failed",
        request_id = %request_id,
        error = %error,
        "memory: failed to read continuation response body"
    );
    MemoryRoundRead::Done
}

/// Fold a continuation body back into a turn, retrying the same round on
/// a cut-stream fold (terminal-less SSE or truncated JSON). Unlike CCR
/// there is no fallback splice here: exhaustion ends the loop with the
/// calls standing, loudly, as before.
/// Extracted from `read_memory_round_body` without behavior change.
async fn fold_or_retry_round_body(
    bytes: bytes::Bytes,
    content_type: &str,
    provider: &str,
    mem_cut_attempts: &mut u32,
    request_id: &str,
) -> MemoryRoundRead {
    match continuation_turn_from_body(&bytes, Some(content_type), provider) {
        Some(next) => MemoryRoundRead::Advance(next),
        None => {
            // Same cut-stream retry as the CCR path (terminal-less SSE or
            // truncated JSON). Unlike CCR there is no fallback splice here:
            // exhaustion breaks with the calls standing, loudly, as before.
            if continuation_cut_retryable(&bytes, content_type, provider)
                && *mem_cut_attempts < MEMORY_CONTINUATION_RETRIES
            {
                *mem_cut_attempts += 1;
                tracing::warn!(
                    event = "memory_continuation_empty_fold",
                    request_id = %request_id,
                    body_bytes = bytes.len(),
                    cut_attempt = *mem_cut_attempts,
                    "memory: continuation body folded to nothing usable; retrying same round"
                );
                tokio::time::sleep(memory_continuation_backoff(*mem_cut_attempts)).await;
                return MemoryRoundRead::Retry;
            }
            // Giving up here used to be silent, and silence is what made this
            // expensive: the tool ran, its answer was thrown away, and the
            // client was handed a turn that simply stopped. Measured
            // 2026-09-22 on Spark — a continuation came back
            // `response.incomplete` with `incomplete_details.reason:
            // max_output_tokens`, 597 of 600 output tokens spent on reasoning
            // and `output: []`. Nothing in the log said so.
            tracing::warn!(
                request_id = %request_id,
                event = "memory_continuation_folded_empty",
                body_bytes = bytes.len(),
                content_type = content_type,
                terminal = ?responses_terminal_reason(&bytes),
                "memory: continuation folded to no usable turn; the memory answer is lost"
            );
            MemoryRoundRead::Done
        }
    }
}

/// The terminal status of a Responses SSE body, plus the reason when it is
/// `incomplete`, for a log line that says why a fold came back empty.
///
/// Returns `None` for bodies that are not a Responses stream, so the caller's
/// log simply omits it rather than guessing.
fn responses_terminal_reason(bytes: &bytes::Bytes) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut terminal = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let Some(response) = v.get("response") else {
            continue;
        };
        let Some(status) = response.get("status").and_then(|s| s.as_str()) else {
            continue;
        };
        if !matches!(status, "completed" | "incomplete" | "failed") {
            continue;
        }
        terminal = Some(
            match response
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str())
            {
                Some(reason) => format!("{status}: {reason}"),
                None => status.to_string(),
            },
        );
    }
    terminal
}

/// Read one memory continuation response: fold SSE back into a turn when a
/// mandating Responses backend streams it, retrying the same round on a
/// transport-cut body or an unusable fold (bounded; exhaustion breaks with
/// the calls standing, loudly, as before).
/// Extracted from `handle_memory_response` without behavior change.
async fn read_memory_round_body(
    resp: reqwest::Response,
    provider: &str,
    mem_cut_attempts: &mut u32,
    request_id: &str,
) -> MemoryRoundRead {
    // As in `handle_ccr_response`: a mandating Responses backend answers
    // a streamed continuation with SSE, which plain JSON parsing cannot
    // read — fold it back into a turn first.
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = match read_continuation_body(resp).await {
        Ok(bytes) => bytes,
        Err(e) => return note_memory_body_stall(e, mem_cut_attempts, request_id).await,
    };
    fold_or_retry_round_body(bytes, &content_type, provider, mem_cut_attempts, request_id).await
}

/// Splice one round's assistant turn + tool results into the continuation
/// request: retail the tail breakpoint, strip unreplayable reasoning, and
/// serialize. Returns `None` when the request has no continuation array or
/// the splice does not serialize (caller ends the loop).
/// Extracted from `handle_memory_response` without behavior change.
#[allow(clippy::too_many_arguments)]
fn append_round_messages(
    current_request: &mut serde_json::Value,
    items_field: &str,
    assistant_msg: serde_json::Value,
    tool_result_msg: serde_json::Value,
    provider: &str,
    config: &Config,
    request_id: &str,
    round: usize,
    upstream_url: &url::Url,
) -> Option<Vec<u8>> {
    let Some(items) = current_request
        .get_mut(items_field)
        .and_then(|v| v.as_array_mut())
    else {
        tracing::warn!(
            event = "memory_no_continuation_array",
            request_id = %request_id,
            field = items_field,
            "memory: no continuation array in request; cannot continue"
        );
        return None;
    };
    extend_or_push(items, assistant_msg, &["_openai_responses_input_items"]);
    extend_or_push(
        items,
        tool_result_msg,
        &["_memory_tool_results", "_openai_responses_tool_results"],
    );

    retail_continuation_breakpoint(current_request, provider, config, request_id, round + 1);

    // The spliced `output[]` carries the model's own reasoning items,
    // `id` and `encrypted_content` included. The translate and sidecar
    // paths strip those for Zen (quirks.rs) because a rotation between
    // rounds invalidates the blob; this path replays the same items and
    // needs the same strip.
    crate::routed::quirks::classify_upstream(upstream_url, false)
        .strip_unreplayable_reasoning(current_request);

    serde_json::to_vec(current_request).ok()
}

/// Note stranded memory calls when the round cap hits with calls still
/// outstanding: the block is suppressed, the budget ran out, and the tool
/// never runs. The log says it to the operator; the trace says it to the
/// model, which otherwise writes its answer as if the lookup had happened.
/// Extracted from `handle_memory_response` without behavior change.
fn note_stranded_memory_calls(
    memory: &MemoryToolContext,
    current_response: &serde_json::Value,
    config: &Config,
    trace: &mut Vec<String>,
    rounds: usize,
    request_id: &str,
) {
    // The cap is the other way a memory call gets stranded: the block is
    // suppressed, the round budget runs out, and the tool never runs. Say so —
    // the alternative is a turn quietly missing work the model asked for.
    if rounds < config.ccr_max_retrieval_rounds {
        return;
    }
    let still_pending = {
        let handler = memory.handler.as_ref();
        handler.has_memory_tool_calls(current_response, memory.provider)
    };
    if !still_pending {
        return;
    }
    tracing::warn!(
        event = "memory_round_cap_reached",
        request_id = %request_id,
        rounds,
        "memory: retrieval round cap reached with calls outstanding; \
         raise HEADROOM_CCR_MAX_RETRIEVAL_ROUNDS"
    );
    // The bracketed marker makes the drop hook-matchable (see
    // retry-dropped-turn.sh): a turn ending on this note with no
    // answer of its own stalls the same way a spliced retrieval
    // does, and the client cannot re-issue a proxy-owned tool.
    let mut stranded = false;
    for name in pending_memory_call_names(current_response, memory.provider) {
        trace.push(format!(
            "{name} → not run: retrieval round cap ({}) reached",
            config.ccr_max_retrieval_rounds
        ));
        stranded = true;
    }
    if stranded {
        trace.push(crate::memory::deferred::DEFERRED_MEMORY_DROPPED_MARKER.to_string());
    }
}

/// Splice the memory trace into the turn head. Anthropic only: this is the
/// shape whose client keeps a transcript and replays it, and the only one
/// whose stream splice passes an added block through (see
/// `sse::ccr_stream::drop_reason`). Leading, because the calls ran before
/// the answer was written.
/// Extracted from `handle_memory_response` without behavior change.
fn splice_memory_trace(provider: &str, trace: &[String], current_response: &mut serde_json::Value) {
    // Anthropic only: this is the shape whose client keeps a transcript and
    // replays it, and the only one whose stream splice passes an added block
    // through (see `sse::ccr_stream::drop_reason`). Leading, because the calls
    // ran before the answer was written.
    if provider == "anthropic" && !trace.is_empty() {
        if let Some(content) = current_response
            .get_mut("content")
            .and_then(|v| v.as_array_mut())
        {
            content.insert(
                0,
                serde_json::json!({
                    "type": "text",
                    "text": format!("[headroom memory]\n{}", trace.join("\n")),
                }),
            );
        }
    }
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_memory_response(
    body_bytes: &bytes::Bytes,
    forwarded_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    memory: &MemoryToolContext,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
    redact: Option<crate::redact::RedactRef>,
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
        let handler = memory.handler.as_ref();
        if !handler.is_initialized() || !handler.has_memory_tool_calls(&response, memory.provider) {
            return (body_bytes.clone(), round_usage);
        }
    }

    let Ok(mut current_request) = serde_json::from_slice::<serde_json::Value>(forwarded_request)
    else {
        tracing::warn!(
            event = "memory_request_unparseable",
            request_id = %request_id,
            "memory: failed to parse original request; skipping tool handling"
        );
        return (body_bytes.clone(), round_usage);
    };

    // As in `handle_ccr_response`: continuations bypass the forwarding path,
    // so the only place their prefix can be checked is here, against the body
    // the first round already had cached.
    let base_kind = continuation_api_kind(provider);
    let base_hash = base_kind.map(|kind| compute_structural_hash(&current_request, kind));

    // A client tool call sharing the turn makes a continuation impossible.
    // Answer the memory call now and hold the answer for the next request.
    if defer_mixed_memory_turn(&response, memory, request_id, provider).await {
        return (body_bytes.clone(), round_usage);
    }

    // Reused purely for its provider-aware message shaping — the CCR handler
    // knows how each provider wants an assistant turn and a tool result
    // expressed, and memory results go back the same way.
    let shaper = CCRResponseHandler::new(None);
    let mut current_response = response;
    let mut rounds = 0;
    // Kept across rounds: the client sees one turn, not one per round.
    let mut trace: Vec<String> = Vec::new();
    // Cut-stream resends, shared by the body-read and fold failure arms
    // below. Per-turn budget like the CCR path; exhaustion breaks with the
    // calls standing (loud) exactly as before.
    let mut mem_cut_attempts: u32 = 0;

    while rounds < config.ccr_max_retrieval_rounds {
        let Some(mut results) = fetch_memory_round_calls(memory, &current_response).await else {
            break;
        };
        // Memory answers come from the local store, which captured the
        // client's real text — redact before they join the continuation.
        redact_memory_results(&redact, &mut results);
        trace.extend(memory_trace_lines(
            &current_response,
            &results,
            memory.provider,
        ));

        // Answer the memory calls in
        // place the way the CCR mixed branch does, and let the client's
        // calls reach the client untouched. Anthropic keeps its deferral
        // (a real tool_result beats prose); every other shape takes the
        // in-place answer. No continuation is sent, so there is nothing
        // to book and the loop ends with the turn resolved. When the
        // splice left a memory call standing (nothing matched it, which
        // cannot happen while ids come from the same turn), fall through
        // to the legacy continuation attempt rather than stranding it.
        if provider != "anthropic" && count_non_memory_calls(&current_response, memory.provider) > 0
        {
            let spliced = splice_memory_results_as_text(&mut current_response, &results, provider);
            let standing = memory
                .handler
                .has_memory_tool_calls(&current_response, memory.provider);
            tracing::info!(
                request_id = %request_id,
                event = "memory_mixed_turn_answered_in_place",
                round = rounds + 1,
                memory_calls = results.len(),
                spliced,
                standing,
                "memory: turn also calls client tools; answering in place instead of continuing"
            );
            if !standing {
                break;
            }
        }

        // `handle_memory_tool_calls` returns provider-shaped tool results
        // already; wrap them the way the continuation array expects.
        let assistant_msg = shaper.extract_assistant_message(&current_response, provider);
        let tool_result_msg = memory_results_message(&results, provider);

        let Some(continuation_body) = append_round_messages(
            &mut current_request,
            items_field,
            assistant_msg,
            tool_result_msg,
            provider,
            config,
            request_id,
            rounds,
            upstream_url,
        ) else {
            break;
        };
        tracing::info!(
            request_id = %request_id,
            round = rounds + 1,
            results_count = results.len(),
            "memory: sending continuation request"
        );
        if let (Some(base), Some(kind)) = (base_hash.as_ref(), base_kind) {
            cache_stabilization::drift_detector::check_continuation_prefix(
                base,
                &current_request,
                kind,
                request_id,
                rounds + 1,
            );
        }
        // A failed continuation takes the memory call down with it: the block
        // is already suppressed, so the tool the model asked for never runs and
        // the turn reaches the client short one tool call. Transport blips and
        // 429/5xx get another attempt; anything else is a body we built wrong,
        // so keep what upstream objected to instead of dropping it.
        let resp = send_memory_continuation(
            client,
            upstream_url,
            outgoing_headers,
            continuation_body,
            request_id,
            rounds,
            items_field,
            &current_request,
        )
        .await;
        let Some(resp) = resp else { break };
        round_usage.add_response(&current_response);
        match read_memory_round_body(resp, provider, &mut mem_cut_attempts, request_id).await {
            MemoryRoundRead::Advance(next) => {
                current_response = next;
                rounds += 1;
            }
            MemoryRoundRead::Retry => continue,
            MemoryRoundRead::Done => break,
        }
    }

    note_stranded_memory_calls(
        memory,
        &current_response,
        config,
        &mut trace,
        rounds,
        request_id,
    );

    splice_memory_trace(provider, &trace, &mut current_response);

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

/// One line per memory call the proxy answered, for the client's transcript.
///
/// The proxy runs `memory_*` itself and splices back only the continuation's
/// final answer, so neither the call nor its result ever reaches the client.
/// The client rebuilds the next request from its own transcript, where the
/// model then reads a bare claim with no tool output behind it. A session on
/// 2026-08-31 read that back and concluded it had fabricated four searches it
/// had in fact run, all of which the `memory_tool_call` log recorded. These
/// lines are the receipt: short enough to carry every turn, specific enough to
/// check against that log.
/// Names of the memory calls still unanswered in `response`, in call order.
fn pending_memory_call_names(
    response: &serde_json::Value,
    provider: crate::memory::tool_adapter::Provider,
) -> Vec<String> {
    crate::memory::tool_adapter::extract_tool_calls(response, provider)
        .into_iter()
        .filter_map(|call| {
            call.get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(name))
                .map(str::to_string)
        })
        .collect()
}

fn memory_trace_lines(
    response: &serde_json::Value,
    results: &[serde_json::Value],
    provider: crate::memory::tool_adapter::Provider,
) -> Vec<String> {
    use serde_json::Value;

    // Char-safe, because a query can end mid-codepoint and this string goes
    // into a response body.
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            return s.replace('\n', " ");
        }
        let head: String = s.chars().take(max).collect();
        format!("{}…", head.replace('\n', " "))
    }

    let mut lines = Vec::new();
    for call in crate::memory::tool_adapter::extract_tool_calls(response, provider) {
        let Some(name) = call.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(&name) {
            continue;
        }
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        // The argument worth showing differs per tool, and only the first one
        // present is worth the width.
        let arg = call
            .get("input")
            .and_then(|input| {
                ["query", "content", "new_content", "memory_id"]
                    .iter()
                    .find_map(|key| input.get(*key).and_then(Value::as_str))
            })
            .map(|s| clip(s, 60))
            .unwrap_or_default();

        let outcome = results
            .iter()
            .find(|r| r.get("tool_use_id").and_then(Value::as_str) == Some(id))
            .and_then(|r| r.get("content").and_then(Value::as_str))
            .and_then(|c| serde_json::from_str::<Value>(c).ok())
            .map(|parsed| {
                let status = parsed
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unparsed");
                if status == "error" {
                    let detail = parsed.get("error").and_then(Value::as_str).unwrap_or("");
                    return format!("error: {}", clip(detail, 80));
                }
                if let Some(n) = parsed.get("count").and_then(Value::as_u64) {
                    return format!("{n} result{}", if n == 1 { "" } else { "s" });
                }
                match parsed.get("memory_id").and_then(Value::as_str) {
                    Some(memory_id) => format!("{status} {memory_id}"),
                    None => status.to_string(),
                }
            })
            // A call with no matching result was answered by nothing, which is
            // exactly the case the receipt exists to make visible.
            .unwrap_or_else(|| "no answer".to_string());

        if arg.is_empty() {
            lines.push(format!("{name} → {outcome}"));
        } else {
            lines.push(format!("{name}(\"{arg}\") → {outcome}"));
        }
    }
    lines
}

#[cfg(test)]
mod memory_mixed_turn_tests {
    use super::{count_non_memory_calls, splice_memory_results_as_text};
    use crate::memory::tool_adapter::Provider;
    use serde_json::json;

    fn responses_turn() -> serde_json::Value {
        json!({
            "output": [
                {"type": "function_call", "call_id": "call_mem", "name": "memory_search",
                 "arguments": "{\"query\":\"x\"}"},
                {"type": "function_call", "call_id": "call_client", "name": "Read",
                 "arguments": "{\"path\":\"f\"}"},
            ],
        })
    }

    fn chat_result(call_id: &str, content: &str) -> serde_json::Value {
        json!({"role": "tool", "tool_call_id": call_id, "content": content})
    }

    #[test]
    fn counts_only_client_calls() {
        assert_eq!(
            count_non_memory_calls(&responses_turn(), Provider::Openai),
            1
        );
        let pure = json!({
            "output": [
                {"type": "function_call", "call_id": "call_mem", "name": "memory_search",
                 "arguments": "{}"},
            ],
        });
        assert_eq!(count_non_memory_calls(&pure, Provider::Openai), 0);
    }

    #[test]
    fn responses_splice_replaces_only_the_memory_call() {
        let mut turn = responses_turn();
        let results = vec![chat_result("call_mem", "two hits")];
        assert_eq!(
            splice_memory_results_as_text(&mut turn, &results, "openai_responses"),
            1
        );
        let items = turn["output"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        // The memory call is now an answer-shaped message item; the client
        // call is untouched, with its identity intact for the client's run.
        assert_eq!(items[0]["type"], "message");
        assert!(items[0].to_string().contains("two hits"));
        assert!(!items[0].to_string().contains("retrieved_context"));
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_client");
    }

    #[test]
    fn responses_splice_leaves_unmatched_calls_standing() {
        let mut turn = responses_turn();
        let results = vec![chat_result("call_other", "stray")];
        assert_eq!(
            splice_memory_results_as_text(&mut turn, &results, "openai_responses"),
            0
        );
        assert_eq!(turn["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn chat_splice_removes_the_memory_call_and_appends_text() {
        let mut turn = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "looking it up",
                    "tool_calls": [
                        {"id": "call_mem", "type": "function",
                         "function": {"name": "memory_search", "arguments": "{}"}},
                        {"id": "call_client", "type": "function",
                         "function": {"name": "Read", "arguments": "{}"}},
                    ],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let results = vec![chat_result("call_mem", "two hits")];
        assert_eq!(
            splice_memory_results_as_text(&mut turn, &results, "openai"),
            1
        );
        let msg = &turn["choices"][0]["message"];
        let calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_client");
        assert!(msg["content"].as_str().unwrap().contains("two hits"));
    }
}

#[cfg(test)]
mod memory_trace_tests {
    use super::{memory_trace_lines, pending_memory_call_names};
    use crate::memory::tool_adapter::Provider;
    use serde_json::json;

    fn call(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    fn result(id: &str, content: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content.to_string(),
        })
    }

    #[test]
    fn search_reports_the_query_and_the_count() {
        let response =
            json!({"content": [call("t1", "memory_search", json!({"query": "raw_payloads"}))]});
        let results = vec![result("t1", json!({"status": "found", "count": 19}))];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_search(\"raw_payloads\") → 19 results"]
        );
    }

    #[test]
    fn save_reports_the_id_so_a_later_turn_can_quote_it() {
        let response = json!({"content": [call("t1", "memory_save", json!({"content": "the proxy runs on 8787"}))]});
        let results = vec![result(
            "t1",
            json!({"status": "saved", "memory_id": "m-42"}),
        )];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_save(\"the proxy runs on 8787\") → saved m-42"]
        );
    }

    #[test]
    fn one_result_is_singular() {
        let response = json!({"content": [call("t1", "memory_search", json!({"query": "q"}))]});
        let results = vec![result("t1", json!({"status": "found", "count": 1}))];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_search(\"q\") → 1 result"]
        );
    }

    #[test]
    fn an_error_says_so_rather_than_reading_as_a_hit() {
        let response = json!({"content": [call("t1", "memory_search", json!({"query": "q"}))]});
        let results = vec![result(
            "t1",
            json!({"status": "error", "error": "backend not initialized"}),
        )];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_search(\"q\") → error: backend not initialized"]
        );
    }

    #[test]
    fn a_call_nothing_answered_is_named_not_hidden() {
        let response = json!({"content": [call("t1", "memory_search", json!({"query": "q"}))]});
        assert_eq!(
            memory_trace_lines(&response, &[], Provider::Anthropic),
            vec!["memory_search(\"q\") → no answer"]
        );
    }

    #[test]
    fn client_tools_sharing_the_turn_are_not_ours_to_report() {
        let response = json!({
            "content": [
                call("t1", "Bash", json!({"command": "ls"})),
                call("t2", "memory_list", json!({})),
            ]
        });
        let results = vec![result("t2", json!({"status": "ok", "count": 44}))];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec!["memory_list → 44 results"]
        );
    }

    #[test]
    fn results_are_paired_by_id_not_by_position() {
        let response = json!({
            "content": [
                call("t1", "memory_search", json!({"query": "first"})),
                call("t2", "memory_search", json!({"query": "second"})),
            ]
        });
        let results = vec![
            result("t2", json!({"status": "found", "count": 2})),
            result("t1", json!({"status": "found", "count": 7})),
        ];
        assert_eq!(
            memory_trace_lines(&response, &results, Provider::Anthropic),
            vec![
                "memory_search(\"first\") → 7 results",
                "memory_search(\"second\") → 2 results",
            ]
        );
    }

    #[test]
    fn a_long_query_is_clipped_on_a_char_boundary() {
        let query = "Ünicode ".repeat(20);
        let response = json!({"content": [call("t1", "memory_search", json!({"query": query}))]});
        let results = vec![result("t1", json!({"status": "found", "count": 0}))];
        let lines = memory_trace_lines(&response, &results, Provider::Anthropic);
        assert!(lines[0].starts_with("memory_search(\"Ünicode"), "{lines:?}");
        assert!(lines[0].ends_with("…\") → 0 results"), "{lines:?}");
    }

    #[test]
    fn pending_names_lists_only_unrun_memory_tools() {
        let response = json!({"content": [
            call("t1", "memory_search", json!({"query": "hosts"})),
            call("t2", "Read", json!({"file_path": "/tmp/x"})),
            call("t3", "memory_save", json!({"content": "a fact"})),
        ]});
        assert_eq!(
            pending_memory_call_names(&response, Provider::Anthropic),
            vec!["memory_search".to_string(), "memory_save".to_string()],
        );
    }

    #[test]
    fn pending_names_is_empty_without_memory_calls() {
        let response = json!({"content": [call("t1", "Read", json!({"file_path": "/tmp/x"}))]});
        assert!(pending_memory_call_names(&response, Provider::Anthropic).is_empty());
    }
}

/// Wrap provider-shaped memory tool results for the continuation array.
///
/// Anthropic wants one user turn holding every `tool_result` block; the OpenAI
/// shapes want one entry per result, so those go behind a sentinel key that
/// [`extend_or_push`] expands.
/// The item an upstream 400 points at (`input[N]` / `messages[N]` in the
/// error text), with string values cut to 80 chars, or `-` when the error
/// names no index or the index is out of range.
fn rejected_item_summary(detail: &str, request: &serde_json::Value, items_field: &str) -> String {
    let idx = detail
        .find(&format!("{items_field}["))
        .map(|start| start + items_field.len() + 1)
        .and_then(|start| {
            let digits: String = detail[start..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse::<usize>().ok()
        });
    let Some(item) = idx.and_then(|i| request.get(items_field)?.get(i)) else {
        return "-".to_string();
    };
    fn shorten(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) if s.chars().count() > 80 => {
                serde_json::Value::String(format!("{}…", s.chars().take(80).collect::<String>()))
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(shorten).collect())
            }
            serde_json::Value::Object(o) => {
                serde_json::Value::Object(o.iter().map(|(k, v)| (k.clone(), shorten(v))).collect())
            }
            other => other.clone(),
        }
    }
    format!("{}[{}]={}", items_field, idx.unwrap_or(0), shorten(item))
}

fn memory_results_message(results: &[serde_json::Value], provider: &str) -> serde_json::Value {
    match provider {
        "anthropic" => serde_json::json!({"role": "user", "content": results}),
        "openai_responses" => {
            // The adapter reads Responses `function_call` items but formats
            // every OpenAI result in Chat shape (`role: tool`). A Chat item
            // in a Responses `input` is a 400: Zen answered `input[N] did
            // not match any supported type` 22 times and `Invalid value:
            // 'tool'` twice on 2026-09-14, and the memory round was lost
            // each time. Reshape here, where the wire format is known.
            let items: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    match (
                        r.get("role").and_then(|v| v.as_str()),
                        r.get("tool_call_id"),
                    ) {
                        (Some("tool"), Some(id)) => {
                            crate::memory::tool_adapter::format_responses_tool_result(
                                id.as_str().unwrap_or(""),
                                r.get("content").and_then(|c| c.as_str()).unwrap_or(""),
                            )
                        }
                        _ => r.clone(),
                    }
                })
                .collect();
            serde_json::json!({"_openai_responses_tool_results": items})
        }
        _ => serde_json::json!({"_memory_tool_results": results}),
    }
}

/// Whether this tool-call name belongs to the proxy (a memory tool the proxy
/// injected, so the proxy must answer it) rather than to the client.
fn is_proxy_memory_call(name: &str) -> bool {
    crate::memory::tool_adapter::MEMORY_TOOL_NAMES.contains(&name)
        || name == crate::memory::tool_adapter::NATIVE_MEMORY_TOOL_NAME
}

/// Count the turn's calls that are NOT proxy memory tools, in the shape the
/// provider speaks. A turn mixing memory calls with client calls cannot be
/// continued on shapes without deferral: the client's calls have no results
/// yet, and appending the assistant turn would leave them unanswered
/// upstream (Zen refuses the continuation with 400 "No tool output found
/// for function call"). Anthropic is excluded by the caller — its deferral
/// holds the memory answer for the next request instead.
fn count_non_memory_calls(
    response: &serde_json::Value,
    provider: crate::memory::tool_adapter::Provider,
) -> usize {
    use crate::memory::tool_adapter::{extract_tool_calls, get_tool_name};
    extract_tool_calls(response, provider)
        .iter()
        .filter(|call| !is_proxy_memory_call(&get_tool_name(call, provider)))
        .count()
}

/// Answer memory calls in place as assistant prose, leaving every other call
/// untouched for the client. Mirrors `splice_ccr_results_as_text` for the
/// mixed-turn case no continuation can serve — with a memory wrapper, never
/// `<retrieved_context>`, so the Stop hook's retrieval branch cannot mistake
/// it for a spliced retrieval. Returns how many calls were replaced.
///
/// Only the shapes without deferral need this (the caller gates Anthropic
/// out): `openai_responses` replaces `function_call` items with `message`
/// items, the same item type a text answer arrives as; `openai` removes the
/// calls from `message.tool_calls` and appends the text to
/// `message.content`, which the client already renders.
fn splice_memory_results_as_text(
    response: &mut serde_json::Value,
    results: &[serde_json::Value],
    provider: &str,
) -> usize {
    // `handle_memory_tool_calls` formats every result Chat-shaped
    // (`role: tool` + `tool_call_id`), whatever the turn's own shape.
    fn result_text<'a>(results: &'a [serde_json::Value], id: &str) -> Option<&'a str> {
        results
            .iter()
            .find(|r| r.get("tool_call_id").and_then(|v| v.as_str()) == Some(id))
            .and_then(|r| r.get("content").and_then(|v| v.as_str()))
    }
    fn wrapped(text: &str) -> String {
        format!("<memory_context>\n{text}\n</memory_context>")
    }
    match provider {
        "openai_responses" => {
            let Some(items) = response.get_mut("output").and_then(|v| v.as_array_mut()) else {
                return 0;
            };
            let mut spliced = 0;
            for item in items.iter_mut() {
                if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
                    continue;
                }
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                    .unwrap_or_default();
                let Some(text) = result_text(results, call_id) else {
                    continue;
                };
                *item = serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": wrapped(text),
                    }],
                });
                spliced += 1;
            }
            spliced
        }
        "openai" => {
            let hits: Vec<(String, String)> = response
                .get("choices")
                .and_then(|v| v.as_array())
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("tool_calls"))
                .and_then(|v| v.as_array())
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| {
                            let id = call.get("id").and_then(|v| v.as_str())?;
                            let text = result_text(results, id)?;
                            Some((id.to_string(), text.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if hits.is_empty() {
                return 0;
            }
            let Some(message) = response
                .get_mut("choices")
                .and_then(|v| v.as_array_mut())
                .and_then(|c| c.first_mut())
                .and_then(|c| c.get_mut("message"))
            else {
                return 0;
            };
            if let Some(calls) = message.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                calls.retain(|call| {
                    let id = call.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    !hits.iter().any(|(hid, _)| hid == id)
                });
            }
            let joined = hits
                .iter()
                .map(|(_, text)| wrapped(text))
                .collect::<Vec<_>>()
                .join("\n");
            match message.get_mut("content") {
                Some(serde_json::Value::String(prev)) => {
                    if !prev.is_empty() {
                        *prev = format!("{prev}\n{joined}");
                    } else {
                        *prev = joined;
                    }
                }
                Some(serde_json::Value::Array(blocks)) => {
                    blocks.push(serde_json::json!({"type": "text", "text": joined}));
                }
                _ => {
                    message["content"] = serde_json::Value::String(joined);
                }
            }
            hits.len()
        }
        _ => 0,
    }
}

/// Test-only helper: drain a body to bytes (uses BodyExt).
#[cfg(test)]
pub async fn body_to_bytes(body: Body) -> Result<Bytes, axum::Error> {
    use axum::Error;
    use http_body_util::BodyExt;
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

    #[tokio::test]
    async fn provider_client_routes_through_socks5h_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let socks_server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(
                greeting,
                [5, 1, 2],
                "client requests SOCKS5 username/password auth"
            );
            stream.write_all(&[5, 2]).await.unwrap();
            let mut auth_header = [0; 2];
            stream.read_exact(&mut auth_header).await.unwrap();
            assert_eq!(auth_header[0], 1, "RFC 1929 auth version");
            let mut username = vec![0; usize::from(auth_header[1])];
            stream.read_exact(&mut username).await.unwrap();
            let mut password_len = [0; 1];
            stream.read_exact(&mut password_len).await.unwrap();
            let mut password = vec![0; usize::from(password_len[0])];
            stream.read_exact(&mut password).await.unwrap();
            assert_eq!(username, b"testuser");
            assert_eq!(password, b"testpass");
            stream.write_all(&[1, 0]).await.unwrap();

            let mut request_header = [0; 4];
            stream.read_exact(&mut request_header).await.unwrap();
            assert_eq!(request_header, [5, 1, 0, 3], "domain-name CONNECT");
            let mut host_len = [0; 1];
            stream.read_exact(&mut host_len).await.unwrap();
            let mut host = vec![0; usize::from(host_len[0])];
            stream.read_exact(&mut host).await.unwrap();
            let mut port = [0; 2];
            stream.read_exact(&mut port).await.unwrap();
            assert_eq!(host, b"muse-zen.internal");
            assert_eq!(u16::from_be_bytes(port), 8087);

            // Accept the tunnel and act as the target HTTP server. No DNS or
            // external network access is needed for this end-to-end check.
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            let mut request = [0; 1024];
            let n = stream.read(&mut request).await.unwrap();
            assert!(request[..n].starts_with(b"POST /v1/messages HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await
                .unwrap();
        });

        let mut config = crate::config::Config::for_test(
            "https://api.anthropic.com".parse().expect("upstream URL"),
        );
        config.http_proxy = Some(format!("socks5h://testuser:testpass@{proxy_addr}"));
        let client = AppState::build_upstream_client(&config).expect("SOCKS5 client");
        let response = client
            .post("http://muse-zen.internal:8087/v1/messages")
            .body("test")
            .send()
            .await
            .expect("request should traverse the local SOCKS5 proxy");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
        socks_server.await.unwrap();
    }

    /// Simulate ten parallel Claude sibling lanes over ten independent SOCKS
    /// egresses. Each first-seen lane must use a different endpoint, and a
    /// later turn for the same lane must remain pinned to its original one.
    /// The per-egress count behind `/debug/inflight`'s `egress_in_flight`: a
    /// rotating egress refuses without leaving a count behind, the other keeps
    /// serving, and a guard moved into a response body lasts until that body
    /// ends or is dropped.
    #[tokio::test]
    async fn egress_guard_follows_the_response_body_and_respects_maintenance() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("x-probe", "kept")
                    .set_body_string("hello"),
            )
            .mount(&server)
            .await;
        let pool = Arc::new(ProviderEgressPool::new(
            vec![reqwest::Client::new(), reqwest::Client::new()],
            vec!["proxy-a".to_string(), "proxy-b".to_string()],
        ));
        let count = |id: &str| pool.in_flight_by_egress()[id].as_u64().unwrap();

        let guard = pool.acquire(0).expect("idle egress");
        assert_eq!((count("proxy-a"), count("proxy-b")), (1, 0));
        assert!(pool.set_maintenance("proxy-a", true));
        assert_eq!(pool.acquire(0).err().as_deref(), Some("proxy-a"));
        assert_eq!(count("proxy-a"), 1, "a refused acquire leaves no count");
        let other = pool.acquire(1).expect("the other egress keeps serving");
        assert_eq!(count("proxy-b"), 1);
        drop(other);
        assert_eq!(count("proxy-b"), 0);

        let resp = reqwest::get(server.uri()).await.unwrap();
        let url = resp.url().clone();
        let resp = attach_egress_guard(resp, Some(guard));
        assert_eq!(count("proxy-a"), 1, "the body carries the guard");
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["x-probe"], "kept");
        assert_eq!(resp.url(), &url);
        assert_eq!(resp.text().await.unwrap(), "hello");
        assert_eq!(count("proxy-a"), 0, "released at the end of the body");

        assert!(pool.set_maintenance("proxy-a", false));
        let guard = pool.acquire(0).expect("rotation finished");
        let resp = attach_egress_guard(reqwest::get(server.uri()).await.unwrap(), Some(guard));
        assert_eq!(count("proxy-a"), 1);
        drop(resp);
        assert_eq!(count("proxy-a"), 0, "released when the body is dropped");
    }

    #[tokio::test]
    async fn ten_concurrent_stream_lanes_use_distinct_sticky_socks_egresses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        const LANES: usize = 10;
        let mut socks_listeners = Vec::with_capacity(LANES);
        let mut proxy_urls = Vec::with_capacity(LANES);
        for _ in 0..LANES {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            proxy_urls.push(format!("socks5h://{}", listener.local_addr().unwrap()));
            socks_listeners.push(listener);
        }

        let socks_servers: Vec<_> = socks_listeners
            .into_iter()
            .enumerate()
            .map(|(index, listener)| {
                tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut greeting = [0; 2];
                    stream.read_exact(&mut greeting).await.unwrap();
                    assert_eq!(greeting, [5, 1]);
                    let mut method = [0; 1];
                    stream.read_exact(&mut method).await.unwrap();
                    assert_eq!(method, [0]);
                    stream.write_all(&[5, 0]).await.unwrap();

                    let mut request_header = [0; 4];
                    stream.read_exact(&mut request_header).await.unwrap();
                    assert_eq!(request_header, [5, 1, 0, 3]);
                    let mut host_len = [0; 1];
                    stream.read_exact(&mut host_len).await.unwrap();
                    let mut host = vec![0; usize::from(host_len[0])];
                    stream.read_exact(&mut host).await.unwrap();
                    let mut port = [0; 2];
                    stream.read_exact(&mut port).await.unwrap();
                    assert_eq!(host, b"muse-zen.internal");
                    assert_eq!(u16::from_be_bytes(port), 8087);
                    stream
                        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                        .await
                        .unwrap();

                    let mut request = [0; 1024];
                    let n = stream.read(&mut request).await.unwrap();
                    assert!(request[..n].starts_with(b"POST /v1/messages HTTP/1.1"));
                    let body = format!("lane-{index}");
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(reply.as_bytes()).await.unwrap();
                })
            })
            .collect();

        let mut config = crate::config::Config::for_test(
            "https://api.anthropic.com".parse().expect("upstream URL"),
        );
        config.zen_http_proxy_pool = proxy_urls;
        let pool = AppState::build_zen_egresses(&config)
            .expect("valid test SOCKS URLs")
            .expect("pool configured");
        let mut state = crate::test_support::test_state(|_| {});
        state.zen_egresses = Some(Arc::new(pool));

        let mut requests = Vec::with_capacity(LANES);
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer test-spark-token".parse().unwrap(),
        );
        let client_addr = "127.0.0.1:4242".parse::<SocketAddr>().unwrap();
        let mut shared_session_key = None;
        for index in 0..LANES {
            // Model ten sibling subagents with a shared credential and
            // opener but distinct agent system prompts, matching the stream
            // lane derivation used by real routed Claude requests.
            let body = serde_json::json!({
                "model": "claude-muse-spark-1.3",
                "system": format!("Spark subagent instructions {index}"),
                "messages": [{"role": "user", "content": "shared parent task opener"}]
            });
            let session_key = derive_session_key(&headers, &client_addr, &body, ApiKind::Anthropic);
            if let Some(shared) = shared_session_key.as_deref() {
                assert_eq!(
                    shared, session_key,
                    "siblings should share session identity"
                );
            } else {
                shared_session_key = Some(session_key.clone());
            }
            let lane_key = stream_lane_key(
                &session_key,
                &compute_structural_hash(&body, ApiKind::Anthropic),
            );
            let (client, slot, egress_id, _guard) = state
                .zen_client_for_lane(Some(&lane_key))
                .expect("new test egresses are not in maintenance");
            assert_eq!(slot, index, "first fan-out should spread one per egress");
            assert!(egress_id.starts_with("proxy-"));
            if index == 0 {
                assert!(state.set_zen_egress_maintenance(egress_id, true));
                assert!(matches!(
                    state.zen_client_for_lane(Some(&lane_key)),
                    Err(blocked_id) if blocked_id == egress_id
                ));
                assert!(state.set_zen_egress_maintenance(egress_id, false));
            }
            let (_, sticky_slot, sticky_id, _) = state
                .zen_client_for_lane(Some(&lane_key))
                .expect("new test egresses are not in maintenance");
            assert_eq!(sticky_slot, slot, "later turns must stay pinned");
            assert_eq!(sticky_id, egress_id);

            let client = client.clone();
            requests.push(tokio::spawn(async move {
                let response = client
                    .post("http://muse-zen.internal:8087/v1/messages")
                    .body("{}")
                    .send()
                    .await
                    .expect("request traverses its assigned SOCKS egress");
                response.text().await.unwrap()
            }));
        }

        for (index, request) in requests.into_iter().enumerate() {
            assert_eq!(request.await.unwrap(), format!("lane-{index}"));
        }
        for server in socks_servers {
            server.await.unwrap();
        }
    }

    /// The rotation-drain contract (`GET /debug/inflight`): guards held for
    /// whole turns must move the process counter up on entry and back down
    /// on drop. Exact asserts are safe here — no lib test drives
    /// `forward_http`/`handle_messages`, so nothing else holds a guard.
    #[test]
    fn inflight_count_tracks_whole_turn_guards() {
        let before = InflightGuard::count_global();
        let g1 = InflightGuard::enter();
        assert_eq!(g1.count(), before + 1);
        let g2 = InflightGuard::enter();
        assert_eq!(InflightGuard::count_global(), before + 2);
        drop(g1);
        assert_eq!(InflightGuard::count_global(), before + 1);
        drop(g2);
        assert_eq!(InflightGuard::count_global(), before);
    }

    /// The gap this closed: a routed turn reaches its upstream through
    /// `handlers::local_model`, never through `forward_http`, so for as
    /// long as the holds lived inline in `forward_http` a routed turn was
    /// forwarded with its volatile `system` lines intact. Both callers go
    /// through `apply_system_holds` now, and this pins that it holds.
    #[test]
    fn the_role_sentence_is_held_for_any_caller_not_just_forward_http() {
        const PLAIN: &str =
            "You are an interactive agent that helps users with software engineering tasks.";
        const STYLED: &str = "You are an interactive agent that helps users according to your \
             \"Output Style\", which describes how you should respond to user queries.";
        let state = crate::test_support::test_state(|c| {
            c.prefix_replay = true;
            c.hold_role_sentence = true;
        });

        let mut opening = serde_json::json!({"system": PLAIN, "messages": []});
        apply_system_holds(&state, &mut opening, "sess-1", "req-1");

        let mut flipped = serde_json::json!({"system": STYLED, "messages": []});
        apply_system_holds(&state, &mut flipped, "sess-1", "req-2");
        assert_eq!(
            flipped["system"], PLAIN,
            "the flipped sentence should have been held to the opening form"
        );
    }

    /// Both holds depend on `--prefix-replay`, so with replay off the body
    /// must go out exactly as it came in.
    #[test]
    fn holds_do_nothing_without_prefix_replay() {
        const STYLED: &str = "You are an interactive agent that helps users according to your \
             \"Output Style\", which describes how you should respond to user queries.";
        let state = crate::test_support::test_state(|c| {
            c.prefix_replay = false;
            c.hold_role_sentence = true;
        });

        let mut opening = serde_json::json!({"system": "You are an interactive agent that helps \
             users with software engineering tasks.", "messages": []});
        apply_system_holds(&state, &mut opening, "sess-1", "req-1");

        let mut flipped = serde_json::json!({"system": STYLED, "messages": []});
        apply_system_holds(&state, &mut flipped, "sess-1", "req-2");
        assert_eq!(flipped["system"], STYLED, "no pin should have been latched");
    }

    /// An unheld turn must come back byte-identical rather than
    /// re-serialized, or a turn no hold touched could pick up a
    /// formatting difference and break the prefix by itself.
    #[test]
    fn an_unheld_body_is_passed_through_byte_for_byte() {
        let state = crate::test_support::test_state(|c| {
            c.prefix_replay = true;
            c.hold_role_sentence = true;
        });
        let body = bytes::Bytes::from_static(br#"{"system":"Be helpful.","messages":[]}"#);
        let out = apply_system_holds_to_bytes(&state, body.clone(), "sess-1", "req-1");
        assert_eq!(out, body);
    }

    /// The footprint moved to the blocking pool; the tracker must not notice.
    #[tokio::test]
    async fn spawned_footprint_records_what_the_inline_call_did() {
        let original = bytes::Bytes::from_static(
            br#"{"system":"s","tools":[{"name":"read","description":"r"}],"messages":[{"role":"user","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#,
        );
        let on_the_wire = bytes::Bytes::from_static(
            br#"{"system":"s plus injected text","tools":[{"name":"read","description":"r"},{"name":"memory_search","description":"m"}],"messages":[{"role":"user","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#,
        );
        // A `None` path loads the live state file, which the running proxy
        // rewrites between two loads; each tracker gets its own empty file.
        let dir = tempfile::tempdir().unwrap();
        let fresh = |name: &str| {
            headroom_core::savings_tracker::SavingsTracker::new(Some(dir.path().join(name)), false)
        };
        let inline = fresh("inline.json");
        record_request_footprint(&inline, "req-inline", &original, &on_the_wire);

        let spawned = Arc::new(fresh("spawned.json"));
        spawn_request_footprint(
            spawned.clone(),
            "req-spawned".to_string(),
            original.clone(),
            on_the_wire.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            inline.proxy_overhead_report(),
            spawned.proxy_overhead_report()
        );
        assert_eq!(
            inline.tool_inventory_report(),
            spawned.tool_inventory_report()
        );
        assert_ne!(
            inline.proxy_overhead_report(),
            fresh("untouched.json").proxy_overhead_report(),
            "the fixture must move a counter or the comparison proves nothing"
        );
    }

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

        // The forwarded request (Responses shape uses `input[]`).
        let forwarded_request = bytes::Bytes::from(
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
            &forwarded_request,
            &upstream_url,
            &client,
            &store as &dyn headroom_core::ccr::CcrStore,
            None,
            &config,
            "req-test",
            &headers,
            "openai_responses",
            None,
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

    /// Query path: a `headroom_retrieve` call with `query` and no `hash`
    /// searches the current project's content index and answers inline.
    #[tokio::test]
    async fn handle_ccr_response_anthropic_query_searches_content_index() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Content index with one block containing a distinctive phrase.
        let dir = tempfile::tempdir().unwrap();
        let stores = std::sync::Arc::new(crate::ctx::projects::ProjectStores::new(
            dir.path().to_path_buf(),
        ));
        stores
            .content("testproj")
            .expect("content store opens")
            .index_content(
                "notes",
                "the needle phrase lives here among ordinary words",
                &headroom_core::ctx::IndexOpts {
                    plain_text_lines: Some(50),
                    ..Default::default()
                },
            )
            .expect("indexing works");

        let server = MockServer::start().await;
        let final_body = serde_json::json!({
            "id": "msg_2", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "done"}],
            "model": "m", "stop_reason": "end_turn",
            "usage": {"input_tokens": 20, "output_tokens": 3}
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(final_body))
            .mount(&server)
            .await;

        let forwarded_request = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "m",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "tu_1", "name": CCR_TOOL_NAME,
                     "input": {"query": "needle phrase"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 100, "output_tokens": 10}
            }))
            .unwrap(),
        );

        let config = Config::for_test(server.uri().parse().unwrap());
        let upstream_url: url::Url = format!("{}/v1/messages", server.uri()).parse().unwrap();
        let client = reqwest::Client::new();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-headroom-project-id",
            http::HeaderValue::from_static("testproj"),
        );
        // The hot CCR store stays empty: the answer must come from the
        // content index, proving the query path does not need a hash.
        let ccr_store = InMemoryCcrStore::new();

        let (body, round_usage) = handle_ccr_response(
            &upstream_reply,
            &forwarded_request,
            &upstream_url,
            &client,
            &ccr_store as &dyn headroom_core::ccr::CcrStore,
            Some(&stores),
            &config,
            "req-test",
            &headers,
            "anthropic",
            None,
        )
        .await;

        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["content"][0]["text"], "done");
        assert_eq!(round_usage.rounds, 1);

        // The continuation carried the indexed content back upstream.
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        let sent_str = serde_json::to_string(&sent).unwrap();
        assert!(
            sent_str.contains("needle phrase lives here"),
            "continuation must carry the indexed hit: {sent_str}"
        );
    }

    /// A streamed continuation response folds back into a turn: backends that
    /// mandate streaming (the chatgpt codex gateway 400s `stream: false`)
    /// answer continuations with SSE, which plain JSON parsing cannot read.
    #[tokio::test]
    async fn handle_ccr_response_openai_responses_reads_sse_continuation() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
        use headroom_core::ccr::CcrStore;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let store = InMemoryCcrStore::new();
        let hash = "abc123def456abc123def456";
        store.put(hash, "the original large content");

        // The continuation went out streamed and comes back SSE.
        let server = MockServer::start().await;
        let sse = "event: response.output_item.done\n\
                   data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\
                   \n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
                   \n";
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&server)
            .await;

        let forwarded_request = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-x",
                "stream": true,
                "input": [{"type": "message", "role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "output": [
                    {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                     "arguments": format!("{{\"hash\":\"{hash}\"}}")}
                ],
                "usage": {"input_tokens": 4_000, "output_tokens": 60}
            }))
            .unwrap(),
        );

        let config = Config::for_test(server.uri().parse().unwrap());
        let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
        let client = reqwest::Client::new();
        let headers = http::HeaderMap::new();

        let (body, round_usage) = handle_ccr_response(
            &upstream_reply,
            &forwarded_request,
            &upstream_url,
            &client,
            &store as &dyn headroom_core::ccr::CcrStore,
            None,
            &config,
            "req-test-sse",
            &headers,
            "openai_responses",
            None,
        )
        .await;

        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            parsed["output"][0]["content"][0]["text"], "done",
            "the SSE continuation must resolve like a JSON one: {parsed}"
        );
        assert_eq!(round_usage.rounds, 1);
    }

    /// Cut-stream retry: a 200 continuation whose SSE body ends with no
    /// terminal event (reasoning deltas then EOF — the 2026-09-17 luna
    /// shape, ~200 KB received, zero output blocks) is resent same-round
    /// instead of falling back to a splice. The retry resolving proves the
    /// turn answers instead of going quiet holding unanswered content.
    #[tokio::test]
    async fn handle_ccr_response_openai_responses_retries_cut_continuation() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
        use headroom_core::ccr::CcrStore;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let store = InMemoryCcrStore::new();
        let hash = "abc123def456abc123def456";
        store.put(hash, "the original large content");

        let server = MockServer::start().await;
        let cut_sse = "event: response.created\n\
                       data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_cut\",\"status\":\"in_progress\"}}\n\
                       \n\
                       event: response.reasoning_summary_text.delta\n\
                       data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"thinking about caches\"}\n\
                       \n";
        let good_sse = "event: response.output_item.done\n\
                       data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\
                       \n\
                       event: response.completed\n\
                       data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\
                       \n";
        let cut_sse = cut_sse.to_string();
        let good_sse = good_sse.to_string();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(move |_: &Request| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200).set_body_raw(cut_sse.clone(), "text/event-stream")
                } else {
                    ResponseTemplate::new(200).set_body_raw(good_sse.clone(), "text/event-stream")
                }
            })
            .mount(&server)
            .await;

        let forwarded_request = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-x",
                "stream": true,
                "input": [{"type": "message", "role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "output": [
                    {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                     "arguments": format!("{{\"hash\":\"{hash}\"}}")}
                ],
                "usage": {"input_tokens": 4_000, "output_tokens": 60}
            }))
            .unwrap(),
        );

        let config = Config::for_test(server.uri().parse().unwrap());
        let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
        let client = reqwest::Client::new();
        let headers = http::HeaderMap::new();

        let (body, round_usage) = handle_ccr_response(
            &upstream_reply,
            &forwarded_request,
            &upstream_url,
            &client,
            &store as &dyn headroom_core::ccr::CcrStore,
            None,
            &config,
            "req-test-cut",
            &headers,
            "openai_responses",
            None,
        )
        .await;

        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            parsed["output"][0]["content"][0]["text"], "done",
            "the retry must resolve the turn instead of splicing: {parsed}"
        );
        assert!(
            !serde_json::to_string(&parsed)
                .unwrap()
                .contains("retrieved_context"),
            "no in-place splice when the retry answers: {parsed}"
        );
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2, "cut body then retried re-send: {parsed}");
        // Both upstream calls were billed: the cut body and its retry.
        assert_eq!(round_usage.rounds, 2);
    }

    /// Same-project recovery on the model path: a block indexed under the
    /// requesting project but expired from the CCR store must resolve via
    /// the own-project fast path (the cross-project sweep skips it by
    /// design), through a normal continuation — no splice, no stall.
    #[tokio::test]
    async fn handle_ccr_response_recovers_same_project_block() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Hot tier deliberately empty: the block lives only in alpha's
        // content index (post-TTL shape).
        let ccr_store = InMemoryCcrStore::new();
        let hash = "abc123def456abc123def456";
        let dir = tempfile::tempdir().unwrap();
        let stores = std::sync::Arc::new(crate::ctx::projects::ProjectStores::new(
            dir.path().to_path_buf(),
        ));
        stores
            .content("/home/dev/alpha")
            .expect("content store opens")
            .index_content(
                "the tool call that produced it",
                "alpha's original block content",
                &headroom_core::ctx::IndexOpts {
                    content_hash: Some(hash.to_string()),
                    plain_text_lines: Some(50),
                    ..Default::default()
                },
            )
            .expect("index write");

        let server = MockServer::start().await;
        let final_body = serde_json::json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(final_body))
            .mount(&server)
            .await;

        let forwarded_request = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-x",
                "input": [{"type": "message", "role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "output": [
                    {"type": "function_call", "call_id": "call_1", "name": CCR_TOOL_NAME,
                     "arguments": format!("{{\"hash\":\"{hash}\"}}")}
                ],
                "usage": {"input_tokens": 100, "output_tokens": 10}
            }))
            .unwrap(),
        );

        let config = Config::for_test(server.uri().parse().unwrap());
        let upstream_url: url::Url = format!("{}/v1/responses", server.uri()).parse().unwrap();
        let client = reqwest::Client::new();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-headroom-cwd",
            http::HeaderValue::from_static("/home/dev/alpha"),
        );

        let local_before = crate::observability::ccr_retrieval::local_tier_hits_get();
        let cross_before = crate::observability::ccr_retrieval::cross_project_hits_get();
        let (body, _) = handle_ccr_response(
            &upstream_reply,
            &forwarded_request,
            &upstream_url,
            &client,
            &ccr_store as &dyn headroom_core::ccr::CcrStore,
            Some(&stores),
            &config,
            "req-test-local",
            &headers,
            "openai_responses",
            None,
        )
        .await;

        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            parsed["output"][0]["content"][0]["text"], "done",
            "same-project recovery must continue, not splice: {parsed}"
        );
        assert!(
            !serde_json::to_string(&parsed)
                .unwrap()
                .contains("retrieved_context"),
            "recovered content goes to the continuation, not the client: {parsed}"
        );

        // And the continuation carried the recovered block upstream.
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(
            serde_json::to_string(&sent)
                .unwrap()
                .contains("alpha's original block content"),
            "continuation must carry the recovered block: {sent}"
        );
        assert_eq!(
            crate::observability::ccr_retrieval::local_tier_hits_get() - local_before,
            1,
            "same-project recovery books the local tier, not the cross-project counter"
        );
        assert_eq!(
            crate::observability::ccr_retrieval::cross_project_hits_get() - cross_before,
            0,
            "no sweep needed when the requesting project's own store answers"
        );
    }

    /// Unit coverage for the continuation body reader: JSON passes through
    /// untouched, SSE folds only for the Responses shape, garbage stays loud.
    #[test]
    fn continuation_turn_from_body_reads_json_then_sse() {
        let json = bytes::Bytes::from(r#"{"output":[{"type":"message"}]}"#);
        let v = continuation_turn_from_body(&json, Some("text/event-stream"), "openai_responses")
            .expect("JSON parses regardless of content type");
        assert_eq!(v["output"][0]["type"], "message");

        let sse = bytes::Bytes::from(
            "event: response.completed\n\
             data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let v = continuation_turn_from_body(&sse, Some("text/event-stream"), "openai_responses")
            .expect("Responses SSE folds into a turn");
        assert_eq!(v["output"][0]["content"][0]["text"], "hi");

        assert!(
            continuation_turn_from_body(&sse, Some("text/event-stream"), "anthropic").is_none(),
            "a Responses body carries no Anthropic events to fold"
        );

        let anthropic_sse = bytes::Bytes::from(
            "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
             event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n",
        );
        let v = continuation_turn_from_body(&anthropic_sse, Some("text/event-stream"), "anthropic")
            .expect("Anthropic SSE folds into a turn");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 7);
        assert_eq!(v["usage"]["output_tokens"], 3);
        assert!(
            continuation_turn_from_body(&anthropic_sse, Some("application/json"), "anthropic")
                .is_none(),
            "a content type that says JSON still wins over the sniff"
        );
        let garbage = bytes::Bytes::from("data: not json\n\n");
        assert!(
            continuation_turn_from_body(&garbage, Some("text/event-stream"), "openai_responses")
                .is_none(),
            "SSE garbage must fail loudly, not resolve into an empty turn"
        );
        assert!(
            continuation_turn_from_body(&garbage, Some("application/json"), "openai_responses")
                .is_none(),
            "non-SSE garbage has no fold to try"
        );

        // No Content-Type at all: sniff the body instead of refusing.
        let v = continuation_turn_from_body(&sse, None, "openai_responses")
            .expect("SSE-shaped body with no Content-Type still folds");
        assert_eq!(v["output"][0]["content"][0]["text"], "hi");
        let v = continuation_turn_from_body(&sse, Some(""), "openai_responses")
            .expect("empty Content-Type counts as absent");
        assert_eq!(v["output"][0]["content"][0]["text"], "hi");
        assert!(
            continuation_turn_from_body(&sse, Some("application/json"), "openai_responses")
                .is_none(),
            "a wrong Content-Type still wins over the sniff"
        );
        assert!(
            continuation_turn_from_body(&bytes::Bytes::from("<html>"), None, "openai_responses")
                .is_none(),
            "non-SSE body with no Content-Type has no fold to try"
        );
    }

    /// Terminal classification for the cut-continuation retry: explicit
    /// verdicts must not retry, a missing terminal on an SSE body must.
    #[test]
    fn continuation_stream_terminal_classifies_cut_vs_verdict() {
        // The 2026-09-17 incident: ~200 KB of reasoning deltas, EOF, no
        // terminal event. Folds to nothing and must read as a cut stream.
        let cut = bytes::Bytes::from(
            "event: response.created\n\
             data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n\
             event: response.reasoning_summary_text.delta\n\
             data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"delta\":\"thinking about caches\"}}\n\n",
        );
        assert_eq!(continuation_stream_terminal(&cut, "openai_responses"), None);
        assert!(
            continuation_turn_from_body(&cut, Some("text/event-stream"), "openai_responses")
                .is_none(),
            "reasoning-only stream with no terminal folds to no blocks"
        );

        for (terminal, event) in [
            ("completed", "response.completed"),
            ("failed", "response.failed"),
            ("incomplete", "response.incomplete"),
        ] {
            let body = bytes::Bytes::from(format!(
                "event: {event}\ndata: {{\"type\":\"{event}\",\"response\":{{\"id\":\"r\"}}}}\n\n"
            ));
            assert_eq!(
                continuation_stream_terminal(&body, "openai_responses"),
                Some(terminal),
                "explicit verdicts must be named, never retried as cuts"
            );
        }

        let anthropic_cut = bytes::Bytes::from(
            "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\n\
             event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        );
        assert_eq!(
            continuation_stream_terminal(&anthropic_cut, "anthropic"),
            None,
            "Anthropic stream without message_delta is a cut"
        );
        let anthropic_done = bytes::Bytes::from(
            "event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        assert_eq!(
            continuation_stream_terminal(&anthropic_done, "anthropic"),
            Some("message_delta")
        );

        assert_eq!(continuation_stream_terminal(&cut, "openai"), None);
        assert_eq!(continuation_stream_terminal(&cut, "google"), None);
    }

    /// Truncation signature for the chat-JSON cut retry: only bodies that do
    /// not end like complete JSON read as cut mid-write.
    #[test]
    fn continuation_json_truncation_signature() {
        assert!(!continuation_json_looks_truncated(br#"{"a":1}"#));
        assert!(!continuation_json_looks_truncated(b"{\"a\":1}  \n"));
        assert!(!continuation_json_looks_truncated(br#"[1,2]"#));
        assert!(continuation_json_looks_truncated(br#"{"a":1,"#));
        assert!(continuation_json_looks_truncated(b""));
        assert!(
            continuation_json_looks_truncated(b"   "),
            "whitespace-only is never a valid turn"
        );
    }

    /// All retrievals failed: the loop must answer the errors in place
    /// instead of paying a full-prefix continuation round to deliver them.
    #[tokio::test]
    async fn handle_ccr_response_skips_continuation_when_everything_failed() {
        use headroom_core::ccr::backends::InMemoryCcrStore;
        use headroom_core::ccr::tool_injection::CCR_TOOL_NAME;
        use wiremock::MockServer;

        // Empty store: the hash below is a guaranteed miss.
        let store = InMemoryCcrStore::new();
        let server = MockServer::start().await;
        // No mock mounted on purpose — any continuation POST would 404,
        // and the received-requests assertion below proves none happened.

        let forwarded_request = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "claude-x",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        );
        let upstream_reply = bytes::Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "content": [
                    {"type": "text", "text": "Let me retrieve that."},
                    {"type": "tool_use", "id": "toolu_1", "name": CCR_TOOL_NAME,
                        "input": {"hash": "ffffffffffffffffffffffff"}},
                ],
            }))
            .unwrap(),
        );

        let config = Config::for_test(server.uri().parse().unwrap());
        let upstream_url: url::Url = format!("{}/v1/messages", server.uri()).parse().unwrap();
        let client = reqwest::Client::new();
        let headers = http::HeaderMap::new();

        let (body, round_usage) = handle_ccr_response(
            &upstream_reply,
            &forwarded_request,
            &upstream_url,
            &client,
            &store as &dyn headroom_core::ccr::CcrStore,
            None,
            &config,
            "req-test-failed",
            &headers,
            "anthropic",
            None,
        )
        .await;

        // No continuation round ran: zero billed rounds, zero upstream calls.
        assert_eq!(round_usage.rounds, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
        // The failure is answered as text, so the client sees the error
        // instead of a tool_use for a tool it never declared.
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let blocks = parsed["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "text");
        assert!(blocks[1]["text"]
            .as_str()
            .unwrap()
            .contains("ffffffffffffffffffffffff"));
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
            None,
            &config,
            "req-test",
            &http::HeaderMap::new(),
            "openai_responses",
            None,
        )
        .await;
        assert!(round_usage.is_empty());
        assert_eq!(round_usage.input_tokens, 0);
    }

    #[test]
    fn memory_results_message_reshapes_chat_results_for_responses() {
        let results = vec![
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"ok\":1}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "call_2", "output": "x"}),
        ];
        let msg = memory_results_message(&results, "openai_responses");
        let items = msg["_openai_responses_tool_results"].as_array().unwrap();
        assert_eq!(
            items[0],
            serde_json::json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"ok\":1}"}),
            "Chat-shaped result becomes a Responses function_call_output"
        );
        assert_eq!(items[1], results[1], "already-Responses items pass through");

        let chat = memory_results_message(&results[..1], "openai");
        assert_eq!(
            chat["_memory_tool_results"][0]["role"], "tool",
            "Chat provider keeps the Chat shape"
        );
    }

    #[test]
    fn rejected_item_summary_names_the_item_the_400_points_at() {
        let request = serde_json::json!({"input": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "c", "content": "x".repeat(200)},
        ]});
        let detail = r#"{"error":{"param":"input[1]","message":"`input[1]` did not match any supported type"}}"#;
        let out = rejected_item_summary(detail, &request, "input");
        assert!(out.starts_with("input[1]={"), "{out}");
        assert!(out.contains("\"role\":\"tool\""), "{out}");
        assert!(out.len() < 200, "long strings are cut: {out}");
        assert_eq!(
            rejected_item_summary("no index here", &request, "input"),
            "-"
        );
        assert_eq!(
            rejected_item_summary("input[9] bad", &request, "input"),
            "-"
        );
    }

    #[test]
    fn extend_or_push_splices_sentinel_and_pushes_plain() {
        let mut items = vec![serde_json::json!({"role": "user"})];
        // Plain entry is pushed as one.
        extend_or_push(
            &mut items,
            serde_json::json!({"role": "assistant"}),
            &["_openai_responses_input_items"],
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
            conversation_key: None,
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

        let (key, label) = resolve_ccr_workspace(Some(&headers), &body, None).unwrap();
        assert_eq!(key, "my-project");
        assert_eq!(label.as_deref(), Some("my-project"));
    }

    #[test]
    fn ccr_workspace_configured_project_root_override() {
        // Ports upstream #3606: the CLI project-root override reaches CCR
        // workspace resolution for clients without cwd metadata.
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}]
        });

        let (key, label) =
            resolve_ccr_workspace(None, &body, Some("/home/user/code/project-c")).unwrap();
        assert!(key.starts_with("project-c-"));
        assert_eq!(label.as_deref(), Some("project-c"));
    }

    #[test]
    fn ccr_workspace_empty_when_unresolved() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(resolve_ccr_workspace(None, &body, None).is_none());
    }

    #[test]
    fn ccr_workspace_system_prompt_cwd_fallback() {
        let body = serde_json::json!({
            "system": "You are helpful.\ncwd: /home/user/code/my-project\n",
            "messages": []
        });

        let (key, label) = resolve_ccr_workspace(None, &body, None).unwrap();
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

    /// Tail-anchored counterpart to the head ladder: the head checkpoints stop
    /// doubling at 32, so on a long turn the disputed tail sits past every one
    /// of them. These windows cover the last 1/2/4 forwarded messages instead.
    fn ladder_body(texts: &[&str]) -> Vec<u8> {
        let messages: Vec<serde_json::Value> = texts
            .iter()
            .map(|t| serde_json::json!({"role": "user", "content": t}))
            .collect();
        serde_json::to_vec(&serde_json::json!({"messages": messages})).unwrap()
    }

    fn ladder_map(s: &str) -> std::collections::HashMap<String, String> {
        s.split(',')
            .filter_map(|p| p.split_once(':'))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn tail_ladder_keeps_a_fixed_schema_and_is_deterministic() {
        let body = ladder_body(&["a", "b", "c", "d", "e"]);
        let first = tail_digest_ladder(&body).unwrap();
        let first_map = ladder_map(&first);
        let mut keys: Vec<&str> = first_map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["t1", "t2", "t4"]);
        assert_eq!(first, tail_digest_ladder(&body).unwrap());
        // Short bodies clamp the windows rather than dropping keys.
        let short = tail_digest_ladder(&ladder_body(&["a"])).unwrap();
        assert_eq!(ladder_map(&short).len(), 3);
        // Non-JSON is None, like the head ladder.
        assert_eq!(tail_digest_ladder(b"not json"), None);
    }

    /// The blind spot this exists for: on a 6-message turn the head ladder's
    /// checkpoints (1, 2, 4) all sit at or before the tail, so a last-message
    /// edit moves nothing on it — while the tail windows catch it.
    #[test]
    fn tail_ladder_moves_where_the_head_ladder_cannot_see() {
        let before = ladder_body(&["m0", "m1", "m2", "m3", "m4", "m5"]);
        let mut parsed: serde_json::Value = serde_json::from_slice(&before).unwrap();
        parsed["messages"][5]["content"] = serde_json::json!("m5 EDITED");
        let after = serde_json::to_vec(&parsed).unwrap();

        let head_before = ladder_map(&prefix_digest_ladder(&before).unwrap());
        let head_after = ladder_map(&prefix_digest_ladder(&after).unwrap());
        assert_eq!(
            head_before, head_after,
            "depths 1,2,4 all precede the edit, so the head ladder holds still"
        );

        let tail_before = ladder_map(&tail_digest_ladder(&before).unwrap());
        let tail_after = ladder_map(&tail_digest_ladder(&after).unwrap());
        assert_ne!(tail_before["t1"], tail_after["t1"]);
        assert_ne!(tail_before["t2"], tail_after["t2"]);
        assert_ne!(tail_before["t4"], tail_after["t4"]);
    }

    /// Gradient: an edit confined to the second-to-last message leaves the
    /// last-message window alone, bounding the churn to the tail pair.
    #[test]
    fn tail_ladder_bounds_churn_to_the_smallest_moved_window() {
        let before = ladder_body(&["m0", "m1", "m2", "m3", "m4", "m5"]);
        let mut parsed: serde_json::Value = serde_json::from_slice(&before).unwrap();
        parsed["messages"][4]["content"] = serde_json::json!("m4 EDITED");
        let after = serde_json::to_vec(&parsed).unwrap();

        let tail_before = ladder_map(&tail_digest_ladder(&before).unwrap());
        let tail_after = ladder_map(&tail_digest_ladder(&after).unwrap());
        assert_eq!(tail_before["t1"], tail_after["t1"]);
        assert_ne!(tail_before["t2"], tail_after["t2"]);
        assert_ne!(tail_before["t4"], tail_after["t4"]);

        // And the reverse direction: a head edit must not move any tail
        // window that does not cover it.
        let mut parsed: serde_json::Value = serde_json::from_slice(&before).unwrap();
        parsed["messages"][0]["content"] = serde_json::json!("m0 EDITED");
        let head_edited = serde_json::to_vec(&parsed).unwrap();
        let tail_head_edited = ladder_map(&tail_digest_ladder(&head_edited).unwrap());
        assert_eq!(tail_before["t1"], tail_head_edited["t1"]);
        assert_eq!(tail_before["t2"], tail_head_edited["t2"]);
        assert_eq!(tail_before["t4"], tail_head_edited["t4"]);
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

    /// The continuation retry log names the failure phase, so the next stall
    /// is diagnosable from one line instead of needing a repro.
    #[tokio::test]
    async fn ccr_transport_kind_names_the_failure_phase() {
        // Unresolvable numeric host: DNS fails fast with no packets, which
        // surfaces as a connect-phase error. (Localhost TCP is filtered in
        // some sandboxes, so a connect-refused target is not hermetic here.)
        let e = reqwest::Client::new()
            .post("http://invalid.invalid/")
            .body("x")
            .send()
            .await
            .unwrap_err();
        assert_eq!(ccr_transport_kind(&e), "connect");
        assert!(
            !ccr_error_chain(&e).is_empty(),
            "the chain must carry more than the top-level message"
        );

        // Listener holds the connection open without answering: the client
        // timeout fires while still waiting for headers. Built from the
        // TLS-aware constructor like every other outbound client (see
        // `tls_client_wiring`), with only a timeout added.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let slow = crate::ssl_context::client_builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let e = slow.post(server.uri()).body("x").send().await.unwrap_err();
        assert_eq!(ccr_transport_kind(&e), "timeout");
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

    // ── drop_headroom_signed_reasoning_blocks ────────────────────
    //
    // The cost-aware router can send one turn to a routed model and the next
    // back to Anthropic. The routed reply carries a signature only this proxy
    // can read, and Anthropic refuses any signature it did not issue, so the
    // envelope has to come off before the turn goes back.

    /// A signature in the shape the routed stream actually writes, so the
    /// test moves if the envelope format does.
    fn our_signature() -> String {
        crate::handlers::reasoning_signature::encode_reasoning_signature(
            &crate::handlers::reasoning_signature::ReasoningReplay {
                id: "rs_1".to_string(),
                encrypted_content: "blob".to_string(),
            },
        )
        .expect("a well-formed replay encodes")
    }

    #[test]
    fn only_our_own_envelope_comes_off_the_anthropic_bound_turn() {
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "native", "signature": "ErUBCkYIBRgCKkDzS1nT"},
                {"type": "thinking", "thinking": "routed", "signature": our_signature()},
                {"type": "text", "text": "answer"},
            ],
        }]));
        let out = drop_headroom_signed_reasoning_blocks(body, "r");
        let content = messages_of(&out)[0]["content"].clone();
        let blocks = content.as_array().unwrap();
        assert_eq!(blocks.len(), 2, "exactly one block should have gone");
        assert_eq!(blocks[0]["thinking"], "native");
        assert_eq!(blocks[0]["signature"], "ErUBCkYIBRgCKkDzS1nT");
        assert_eq!(blocks[1]["text"], "answer");
    }

    #[test]
    fn a_turn_that_never_met_a_routed_model_is_byte_identical() {
        // The prefix gate: no envelope, no parse, no re-serialize. A body
        // that came back changed would move the cached prefix for every
        // conversation on the proxy, which is most of them.
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "native", "signature": "ErUBCkYIBRgCKkDzS1nT"},
                {"type": "text", "text": "answer"},
            ],
        }]));
        assert_eq!(
            drop_headroom_signed_reasoning_blocks(body.clone(), "r"),
            body
        );
    }

    #[test]
    fn our_envelope_is_left_alone_when_dropping_would_empty_the_message() {
        // Same trade the unsigned stage refuses: an empty content array is
        // rejected just as firmly as the foreign signature.
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "routed", "signature": our_signature()}],
        }]));
        assert_eq!(
            drop_headroom_signed_reasoning_blocks(body.clone(), "r"),
            body
        );
    }

    #[test]
    fn dropping_our_envelope_carries_its_cache_breakpoint_forward() {
        let body = body_with(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "routed", "signature": our_signature(),
                 "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "answer"},
            ],
        }]));
        let out = drop_headroom_signed_reasoning_blocks(body, "r");
        let content = messages_of(&out)[0]["content"].clone();
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(
            content[0]["cache_control"]["type"], "ephemeral",
            "the breakpoint must ride to the surviving block, not vanish"
        );
    }

    #[test]
    fn the_tampering_guard_does_not_see_our_envelope_as_a_rewrite() {
        // `restore_client_reasoning_blocks` reverts the whole message array
        // when the outbound signed set stops matching the client's. Counting
        // our own envelope there would put the refused block straight back.
        let client: Vec<serde_json::Value> = serde_json::from_value(serde_json::json!([{
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "native", "signature": "ErUBCkYIBRgCKkDzS1nT"},
                {"type": "thinking", "thinking": "routed", "signature": our_signature()},
                {"type": "text", "text": "tail"},
            ],
        }]))
        .unwrap();
        let dropped = drop_headroom_signed_reasoning_blocks(
            body_with(serde_json::Value::Array(client.clone())),
            "r",
        );
        let forwarded: Vec<serde_json::Value> =
            serde_json::from_value(messages_of(&dropped)).unwrap();
        assert_eq!(
            signed_reasoning_blocks(&client),
            signed_reasoning_blocks(&forwarded),
            "dropping our own envelope must leave the provider-signed set identical"
        );
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
// The signals are computed and logged; this emits them to Prometheus, which
// no caller has asked for yet.
#[allow(dead_code)]
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
mod image_census_tests {
    use super::*;

    /// The two forms of the same screenshot: what the client sends when it
    /// still holds the image, and what it sends once it has let go. Both are
    /// counted, on their own axis, so the turn the client collapses is visible
    /// in the log without guessing from a diff.
    #[test]
    fn counts_live_images_and_the_placeholders_left_behind() {
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_live",
                    "content": [{
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
                    }]
                }]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_collapsed",
                    "content": [{"type": "text", "text": "[image]"}]
                }]
            }),
        ];

        assert_eq!(image_census(&messages), (1, 1, 4));
    }

    /// Ordinary text must not read as either, or every turn logs a census.
    #[test]
    fn ignores_text_that_merely_mentions_an_image() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "the [image] above shows the panel"}]
        })];

        assert_eq!(image_census(&messages), (0, 0, 0));
    }

    /// A top-level image, not wrapped in a tool_result, still counts.
    #[test]
    fn counts_an_image_attached_straight_to_the_message() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image", "source": {"type": "base64", "data": "AAAAAAAA"}}
            ]
        })];

        assert_eq!(image_census(&messages), (1, 0, 8));
    }
}

#[cfg(test)]
mod offload_savings_tests {
    use super::*;
    use headroom_core::request_outcome::RequestOutcome;

    /// A tool-less Opus turn the router sent to the free Spark tier: 26,015
    /// fresh input tokens, a 1,000-token cache read, 500 out.
    fn spark_turn() -> RequestOutcome {
        RequestOutcome {
            model: "muse-spark-1.3-contributor-free".into(),
            routed_from_model: Some("claude-opus-5".into()),
            status_code: 200,
            uncached_input_tokens: 26_015,
            cache_read_tokens: 1_000,
            cache_write_tokens: 0,
            output_tokens: 500,
            ..Default::default()
        }
    }

    #[test]
    fn a_rerouted_turn_is_credited_the_bill_it_never_sent() {
        let s = offload_savings(&spark_turn()).expect("a served reroute saves something");
        assert_eq!(s.from_model, "claude-opus-5");
        assert_eq!(s.tokens, 27_515);
        // Opus 5: $5/MTok in, $25/MTok out, $0.50/MTok cache read. Spark is
        // free, so the whole counterfactual is the saving.
        let expected = 26_015.0 * 5.0 / 1e6 + 500.0 * 25.0 / 1e6 + 1_000.0 * 0.5 / 1e6;
        assert!(
            (s.saved_usd - expected).abs() < 1e-12,
            "{} != {expected}",
            s.saved_usd
        );
    }

    #[test]
    fn a_turn_the_clients_own_model_served_is_not_an_offload() {
        let mut o = spark_turn();
        o.routed_from_model = None;
        assert!(offload_savings(&o).is_none());
    }

    #[test]
    fn a_failed_reroute_saves_nothing() {
        // The Zen upstream 503s and, with no fallback wired, the turn dies.
        // Booking a saving there would pay us for losing the user's turn.
        let mut o = spark_turn();
        o.status_code = 503;
        assert!(offload_savings(&o).is_none());
    }

    #[test]
    fn a_reroute_to_a_dearer_model_never_books_a_negative_saving() {
        let mut o = spark_turn();
        o.model = "claude-opus-5".into();
        o.routed_from_model = Some("claude-haiku-4-5".into());
        assert_eq!(offload_savings(&o).unwrap().saved_usd, 0.0);
    }
}

#[cfg(test)]
mod timing_field_tests {
    use super::*;
    use crate::observability::proxy_counters;
    use headroom_core::request_outcome::RequestOutcome;

    fn turn_with(assistant: serde_json::Value, user: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": assistant},
            {"role": "user", "content": user},
        ]})
    }

    #[test]
    fn a_paired_turn_reports_nothing_unanswered() {
        let body = turn_with(
            serde_json::json!([{"type": "tool_use", "id": "tu_1", "name": "Bash"}]),
            serde_json::json!([{"type": "tool_result", "tool_use_id": "tu_1"}]),
        );
        assert!(unanswered_tool_uses(&body).is_empty());
    }

    #[test]
    fn an_unanswered_call_is_reported_with_its_message_index() {
        let body = turn_with(
            serde_json::json!([
                {"type": "tool_use", "id": "tu_1", "name": "Bash"},
                {"type": "tool_use", "id": "tu_2", "name": "Skill"},
            ]),
            serde_json::json!([{"type": "tool_result", "tool_use_id": "tu_1"}]),
        );
        assert_eq!(unanswered_tool_uses(&body), vec![(1, "tu_2".to_string())]);
    }

    /// A result in a later message does not count: Anthropic wants it in the
    /// message immediately after the call.
    #[test]
    fn a_result_two_messages_later_does_not_answer_the_call() {
        let mut body = turn_with(
            serde_json::json!([{"type": "tool_use", "id": "tu_1", "name": "Bash"}]),
            serde_json::json!([{"type": "text", "text": "nothing here"}]),
        );
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "tu_1"}]
            }));
        assert_eq!(unanswered_tool_uses(&body), vec![(1, "tu_1".to_string())]);
    }

    /// The last assistant message has no next message at all. Upstream still
    /// refuses it, so it is still worth naming.
    #[test]
    fn a_trailing_call_with_no_next_message_is_unanswered() {
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "tu_1", "name": "Bash"}]},
        ]});
        assert_eq!(unanswered_tool_uses(&body), vec![(1, "tu_1".to_string())]);
    }

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
    fn a_prior_turn_reasoning_block_dropped_whole_is_forwarded_as_built() {
        let mut body = client_body_with_thinking();
        body["messages"].as_array_mut().unwrap().extend([
            serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "later reasoning", "signature": "sig456"},
                {"type": "text", "text": "43"}
            ]}),
            serde_json::json!({"role": "user", "content": "and then"}),
        ]);
        let original = as_bytes(&body);
        // The first assistant turn loses its block; the last keeps its own.
        body["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        let sent = as_bytes(&body);
        let out = restore_client_reasoning_blocks(sent.clone(), &original, "r1");
        assert_eq!(out, sent);

        // Dropping the LAST assistant turn's block still restores — but only
        // that message. The earlier turn keeps the strip it was given, which
        // is `prior_thinking` doing its job and no business of this guard.
        body["messages"][3]["content"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        let out = restore_client_reasoning_blocks(as_bytes(&body), &original, "r1");
        let restored: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            restored["messages"][3],
            client_body_with_thinking_extended()["messages"][3]
        );
        assert_eq!(restored["messages"][1]["content"][0]["type"], "text");
    }

    /// The point of repairing narrowly: a message the guard has no quarrel
    /// with keeps whatever the pipeline did to it. Reverting those too is
    /// what killed the cached prefix — the opening messages are in it.
    #[test]
    fn an_untouched_message_keeps_its_rewrite_when_another_is_restored() {
        let original = as_bytes(&client_body_with_thinking_extended());
        let mut body = client_body_with_thinking_extended();
        // Stand-in for a ctx-offload placeholder in the cached prefix.
        body["messages"][0]["content"] = serde_json::json!("[offloaded #abc123]");
        // And the breakage the guard exists for, in the last assistant turn.
        body["messages"][3]["content"][0]["thinking"] = serde_json::json!("edited");

        let out = restore_client_reasoning_blocks(as_bytes(&body), &original, "r1");
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["messages"][0]["content"], "[offloaded #abc123]");
        assert_eq!(
            parsed["messages"][3]["content"][0]["thinking"],
            "later reasoning"
        );
    }

    /// Arrays that cannot be lined up index for index fall back to the whole
    /// client array, which is the only repair that is certainly correct.
    #[test]
    fn a_changed_message_count_falls_back_to_the_whole_array() {
        let original = as_bytes(&client_body_with_thinking_extended());
        let mut body = client_body_with_thinking_extended();
        body["messages"][0]["content"] = serde_json::json!("[offloaded #abc123]");
        body["messages"][3]["content"][0]["thinking"] = serde_json::json!("edited");
        body["messages"].as_array_mut().unwrap().pop();

        let out = restore_client_reasoning_blocks(as_bytes(&body), &original, "r1");
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            parsed["messages"],
            client_body_with_thinking_extended()["messages"]
        );
    }

    fn client_body_with_thinking_extended() -> serde_json::Value {
        let mut body = client_body_with_thinking();
        body["messages"].as_array_mut().unwrap().extend([
            serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "later reasoning", "signature": "sig456"},
                {"type": "text", "text": "43"}
            ]}),
            serde_json::json!({"role": "user", "content": "and then"}),
        ]);
        body
    }

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
            client: crate::ssl_context::client_builder()
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
