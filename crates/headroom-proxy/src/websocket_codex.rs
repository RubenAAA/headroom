//! Codex Responses WebSocket handler: frame-by-frame compression + usage
//! accounting for the Codex WS protocol.
//!
//! Rust port of `headroom/proxy/handlers/openai.py::handle_openai_responses_ws`
//! (~L3824-6050) plus `_ws_http_fallback` (L6052+). The generic byte pump stays
//! in [`crate::websocket`]; this module owns the four Codex responses paths:
//! `/v1/responses`, `/v1/codex/responses`, `/backend-api/responses`,
//! `/backend-api/codex/responses`.
//!
//! Deliberate divergences from the Python handler (see PR description):
//! - Memory injection and the (dead) memory-tool suppression path are NOT
//!   ported: upstream→client is byte-equal passthrough plus observation, and
//!   `response_completed_seen` is set only on an actual `response.completed`.
//! - The codex rate-limit state (`get_codex_rate_limit_state`) and throttled
//!   usage-endpoint poll (`maybe_schedule_usage_poll`) have no Rust
//!   counterpart — skipped. The `x-codex-*` upstream handshake headers ARE
//!   still forwarded onto the client-facing 101 so the Codex desktop gauge
//!   keeps working.
//! - `SessionBetaTracker` is a per-connection no-op (WS session ids are fresh
//!   uuids), so only the deterministic `merge_openai_beta` merge is ported.
//! - Token accounting: `saved = tokens_before - tokens_after` and
//!   `attempted = tokens_before` from the compressor outcome. Python derives
//!   these from per-unit tokenizer counts + tool schema bytes, so the
//!   active-savings-percent denominator differs slightly.
//! - Stage timings are one structured `tracing::info!` line instead of the
//!   Python `StageTimer` subsystem.
//! - Client binary frames are forwarded uncompressed (Python's
//!   `receive_text()` would end the relay; forwarding is a harmless superset).
//! - `response.completed` usage is NOT fed into `state.usage_observer`: the
//!   observer's `begin_request`/`complete` contract is keyed to the HTTP
//!   request path (conversation identity + drift dims captured pre-forward)
//!   and has no natural WS analog — skipped rather than half-wired.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use base64::Engine as _;
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message as TgMsg;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use headroom_core::auth_mode::AuthMode;
use headroom_core::request_outcome::{emit_request_outcome, OutcomeSink, RequestOutcome};

use crate::compression::{self, Outcome, PassthroughReason};
use crate::compression_failure::{
    decide_compression_failure_action, oversize_threshold_bytes, WS_COMPRESSION_OVERSIZE_BYTES_ENV,
};
use crate::config::CompressionMode;
use crate::observability::proxy_counters::{
    dec_active_relay_tasks, dec_active_ws_sessions, inc_active_relay_tasks, inc_active_ws_sessions,
    record_ws_session_duration,
};
use crate::proxy::AppState;
use crate::websocket::{ax_to_tg, tg_to_ax};
use crate::ws_session_registry::{TerminationCause, WSSessionHandle};

/// OpenAI-Beta token required for the Responses WebSocket protocol.
pub const RESPONSES_WS_REQUIRED_BETA: &str = "responses_websockets=2026-02-06";

/// Client-only lite header that OpenAI rejects when it leaks upstream
/// (newer Codex models 4xx). Port of `CODEX_RESPONSES_LITE_HEADER`.
pub const CODEX_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Hardcoded ChatGPT subscription backend (Python parity).
const CHATGPT_CODEX_WS_URL: &str = "wss://chatgpt.com/backend-api/codex/responses";
const CHATGPT_CODEX_HTTP_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Cap on the wait for the first client frame after the WS handshake.
/// Port of `WS_FIRST_FRAME_TIMEOUT_SECONDS = 60.0` (openai.py:611).
pub const WS_FIRST_FRAME_TIMEOUT_SECONDS: f64 = 60.0;

/// Env override for the first-frame timeout (primarily for tests; the
/// Python constant is not overridable).
const WS_FIRST_FRAME_TIMEOUT_ENV: &str = "HEADROOM_WS_FIRST_FRAME_TIMEOUT_SECONDS";

/// Compression stage timeout. Port of `COMPRESSION_TIMEOUT_SECONDS`
/// (helpers.py:775-782): env `HEADROOM_COMPRESSION_TIMEOUT_SECONDS`,
/// default 30, fallback 30 on unparseable value.
pub const COMPRESSION_TIMEOUT_SECONDS_DEFAULT: f64 = 30.0;
const COMPRESSION_TIMEOUT_ENV: &str = "HEADROOM_COMPRESSION_TIMEOUT_SECONDS";

const WS_ALLOWED_ORIGINS_ENV: &str = "HEADROOM_WS_ORIGINS";
const CORS_ALLOWED_ORIGINS_ENV: &str = "HEADROOM_CORS_ORIGINS";

/// Headers never forwarded on the upstream WS handshake (Python 3936-3949
/// skip-set; tungstenite regenerates the handshake-specific ones).
const WS_SKIP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-accept",
    "sec-websocket-protocol",
    "content-length",
    "transfer-encoding",
];

/// UA substring → client name map. Port of `auth_mode.CLIENT_UA_MAP`.
const CLIENT_UA_MAP: &[(&str, &str)] = &[
    ("claude-code/", "claude-code"),
    ("claude-cli/", "claude-code"),
    ("claude-vscode/", "claude-vscode"),
    ("anthropic-cli/", "anthropic-cli"),
    ("codex-cli/", "codex"),
    ("cursor/", "cursor"),
    ("zed/", "zed"),
    ("aider/", "aider"),
    ("droid/", "droid"),
    ("opencode/", "opencode"),
    ("github-copilot/", "copilot"),
    ("antigravity/", "antigravity"),
    ("strands-agents/", "strands"),
];

/// The four Codex responses WS paths (proxy_routes.py L552-603).
pub fn is_codex_responses_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/responses"
            | "/v1/codex/responses"
            | "/backend-api/responses"
            | "/backend-api/codex/responses"
    )
}

// ─────────────────────────────────────────────────────────────────────────
// Pure helpers (unit-tested below)
// ─────────────────────────────────────────────────────────────────────────

/// Normalize an origin to `scheme://host[:port]` with default ports elided.
/// Port of `openai.py::_normalize_origin`.
fn normalize_origin(origin: &str) -> Option<String> {
    let parsed = url::Url::parse(origin.trim()).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss") {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    // `Url::port()` already elides the scheme-default port for
    // http/https/ws/wss, matching the Python default-port logic.
    match parsed.port() {
        Some(p) => Some(format!("{scheme}://{host}:{p}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Loopback-origin check. Port of `openai.py::_is_loopback_ws_origin`.
fn is_loopback_ws_origin(origin: &str) -> bool {
    let Ok(parsed) = url::Url::parse(origin.trim()) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return false;
    }
    crate::loopback_guard::is_loopback_host(parsed.host_str())
}

/// Origin policy. Port of `openai.py::_is_allowed_websocket_origin`:
/// missing Origin → allowed; present → loopback-only unless the env
/// allowlist matches (`*` allows all).
fn is_allowed_websocket_origin(origin: Option<&str>, allowed_env: Option<&[String]>) -> bool {
    let Some(origin) = origin.filter(|o| !o.is_empty()) else {
        return true;
    };
    let Some(allowed) = allowed_env else {
        return is_loopback_ws_origin(origin);
    };
    if allowed.iter().any(|a| a == "*") {
        return true;
    }
    let Some(normalized) = normalize_origin(origin) else {
        return false;
    };
    allowed
        .iter()
        .filter_map(|a| normalize_origin(a))
        .any(|a| a == normalized)
}

/// Read `HEADROOM_WS_ORIGINS` (fallback `HEADROOM_CORS_ORIGINS`) into a list.
fn allowed_ws_origins_from_env() -> Option<Vec<String>> {
    let raw = std::env::var(WS_ALLOWED_ORIGINS_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var(CORS_ALLOWED_ORIGINS_ENV)
                .ok()
                .filter(|s| !s.trim().is_empty())
        })?;
    Some(
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Identify the client harness. Port of `auth_mode.classify_client`:
/// explicit `x-client` header (trimmed, lowercased) wins; else UA
/// substring match; else `None`.
fn classify_client(headers: &HeaderMap) -> Option<String> {
    if let Some(explicit) = headers
        .get("x-client")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        return Some(explicit);
    }
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase())?;
    if ua.is_empty() {
        return None;
    }
    CLIENT_UA_MAP
        .iter()
        .find(|(needle, _)| ua.contains(needle))
        .map(|(_, name)| (*name).to_string())
}

/// Whether to stamp `X-Client: codex`. Port of
/// `auth_mode.should_stamp_codex_client`: only for an unidentified caller
/// on `/v1/responses` (or a subpath).
fn should_stamp_codex_client(path: &str, headers: &HeaderMap) -> bool {
    if path != "/v1/responses" && !path.starts_with("/v1/responses/") {
        return false;
    }
    classify_client(headers).is_none()
}

/// Best-effort decode of a Bearer JWT payload — NO signature verification.
/// This is only a routing hint extractor; upstream still authenticates.
/// Port of `openai.py::_decode_openai_bearer_payload`.
fn decode_bearer_jwt_payload(auth: &str) -> Option<Value> {
    let (scheme, token) = auth.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    if token.matches('.').count() < 2 {
        return None;
    }
    let payload = token.splitn(3, '.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value.is_object().then_some(value)
}

/// Resolve ChatGPT Codex routing: explicit `chatgpt-account-id` header, or
/// the `chatgpt_account_id` claim from the (unverified) Bearer JWT payload.
/// Inserts `ChatGPT-Account-ID` when derived from the JWT. Returns whether
/// the session routes to the ChatGPT subscription backend.
/// Port of `openai.py::_resolve_codex_routing_headers`.
fn resolve_codex_routing(headers: &mut HeaderMap) -> bool {
    if headers.contains_key("chatgpt-account-id") {
        return true;
    }
    let Some(auth) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(payload) = decode_bearer_jwt_payload(auth) else {
        return false;
    };
    let account_id = payload
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(id) = account_id {
        if let Ok(v) = HeaderValue::from_str(id) {
            headers.insert(HeaderName::from_static("chatgpt-account-id"), v);
            return true;
        }
    }
    false
}

/// Usage extracted from a `response.completed` event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ResponsesUsage {
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    uncached_tokens: i64,
}

/// Port of `openai.py::_extract_responses_usage` (L656-687): non-zero only
/// for `response.completed`; reads `response.usage` (falling back to a
/// top-level `usage`); cache-write inferred as `max(input - cached, 0)`.
fn extract_responses_usage(event: &Value) -> ResponsesUsage {
    if event.get("type").and_then(Value::as_str) != Some("response.completed") {
        return ResponsesUsage::default();
    }
    let response = event.get("response").filter(|v| v.is_object());
    let usage = response
        .and_then(|r| r.get("usage"))
        .filter(|v| v.is_object())
        .or_else(|| event.get("usage").filter(|v| v.is_object()));
    let Some(usage) = usage else {
        return ResponsesUsage::default();
    };
    let int = |v: Option<&Value>| -> i64 { v.and_then(Value::as_i64).unwrap_or(0).max(0) };
    let input_tokens = int(usage.get("input_tokens"));
    let output_tokens = int(usage.get("output_tokens"));
    let cached = int(usage
        .get("input_tokens_details")
        .filter(|v| v.is_object())
        .and_then(|d| d.get("cached_tokens")));
    ResponsesUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens: cached,
        cache_write_tokens: (input_tokens - cached).max(0),
        uncached_tokens: (input_tokens - cached).max(0),
    }
}

/// Result of running the compressor over one `response.create` frame.
#[derive(Debug)]
enum FrameCompression {
    /// Forward the original frame unchanged.
    Passthrough { reason: &'static str },
    /// Forward the rewritten frame.
    Compressed {
        text: String,
        tokens_before: usize,
        tokens_after: usize,
        strategies: Vec<&'static str>,
        bytes_before: usize,
        bytes_after: usize,
    },
}

/// Pure envelope + compression logic for a single client text frame.
/// Both envelope shapes are supported (openai.py 4657-4684):
/// `{"type":"response.create","response":{...}}` and the bare payload with
/// `"type":"response.create"` at top level. Only `response.create` is ever
/// intercepted — everything else passes through byte-equal.
fn compress_response_create_frame(
    raw: &str,
    mode: CompressionMode,
    auth_mode: AuthMode,
    request_id: &str,
) -> FrameCompression {
    let Ok(mut parsed) = serde_json::from_str::<Value>(raw) else {
        return FrameCompression::Passthrough { reason: "non_json" };
    };
    if parsed.get("type").and_then(Value::as_str) != Some("response.create") {
        return FrameCompression::Passthrough {
            reason: "not_response_create",
        };
    }
    // Wrapped iff a `response` key exists AND is an object; a present but
    // non-object `response` is an invalid inner payload (openai.py 4957-4966).
    let wrapped = match parsed.get("response") {
        Some(v) if v.is_object() => true,
        Some(_) => {
            return FrameCompression::Passthrough {
                reason: "invalid_inner_payload",
            }
        }
        None => false,
    };
    let inner = if wrapped {
        parsed.get_mut("response").expect("checked above")
    } else {
        &mut parsed
    };
    let additional_tools_restore_plan =
        crate::handlers::responses::lift_codex_additional_tools(inner, request_id);
    let inner_bytes = match serde_json::to_vec(inner) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return FrameCompression::Passthrough {
                reason: "serialize_failed",
            }
        }
    };
    let bytes_before = raw.len();
    match compression::compress_openai_responses_request(&inner_bytes, mode, auth_mode, request_id)
    {
        Outcome::Compressed {
            body,
            tokens_before,
            tokens_after,
            strategies_applied,
            ..
        } => {
            let body = crate::handlers::responses::restore_codex_additional_tools_body(
                body,
                additional_tools_restore_plan.as_ref(),
                request_id,
            );
            let text = if wrapped {
                let Ok(new_inner) = serde_json::from_slice::<Value>(&body) else {
                    return FrameCompression::Passthrough {
                        reason: "compressed_payload_not_json",
                    };
                };
                if !new_inner.is_object() {
                    return FrameCompression::Passthrough {
                        reason: "compressed_payload_not_dict",
                    };
                }
                parsed["response"] = new_inner;
                match serde_json::to_string(&parsed) {
                    Ok(s) => s,
                    Err(_) => {
                        return FrameCompression::Passthrough {
                            reason: "serialize_failed",
                        }
                    }
                }
            } else {
                match String::from_utf8(body.to_vec()) {
                    Ok(s) => s,
                    Err(_) => {
                        return FrameCompression::Passthrough {
                            reason: "compressed_payload_not_utf8",
                        }
                    }
                }
            };
            let bytes_after = text.len();
            FrameCompression::Compressed {
                text,
                tokens_before,
                tokens_after,
                strategies: strategies_applied,
                bytes_before,
                bytes_after,
            }
        }
        Outcome::NoCompression => FrameCompression::Passthrough {
            reason: "no_compression",
        },
        Outcome::Passthrough { reason } => FrameCompression::Passthrough {
            reason: match reason {
                PassthroughReason::NotJson => "not_json",
                PassthroughReason::NoMessages => "no_messages",
                PassthroughReason::ModeOff => "optimize_disabled",
            },
        },
    }
}

/// Which relay task finished first (drives termination classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstDone {
    Client { error: bool },
    Upstream { error: bool },
}

/// Termination-cause classification. Port of openai.py 5691-5754 including
/// the `client_cancel` override.
fn classify_termination(
    first: FirstDone,
    response_completed_seen: bool,
    cancel_frames: u64,
) -> TerminationCause {
    let mut cause = match first {
        FirstDone::Client { error: true } => TerminationCause::ClientError,
        FirstDone::Client { error: false } => TerminationCause::ClientDisconnect,
        FirstDone::Upstream { error: true } => TerminationCause::UpstreamError,
        FirstDone::Upstream { error: false } => {
            if response_completed_seen {
                TerminationCause::ResponseCompleted
            } else {
                TerminationCause::UpstreamDisconnect
            }
        }
    };
    if cancel_frames > 0
        && !response_completed_seen
        && matches!(
            cause,
            TerminationCause::UpstreamDisconnect
                | TerminationCause::ClientDisconnect
                | TerminationCause::Unknown
        )
    {
        cause = TerminationCause::ClientCancel;
    }
    cause
}

fn compression_timeout() -> Duration {
    let secs = std::env::var(COMPRESSION_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(COMPRESSION_TIMEOUT_SECONDS_DEFAULT);
    Duration::from_secs_f64(secs.max(0.001))
}

fn first_frame_timeout() -> Duration {
    let secs = std::env::var(WS_FIRST_FRAME_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(WS_FIRST_FRAME_TIMEOUT_SECONDS);
    Duration::from_secs_f64(secs.max(0.001))
}

// ─────────────────────────────────────────────────────────────────────────
// Session state
// ─────────────────────────────────────────────────────────────────────────

/// Per-session accumulators + `recorded_*` shadow totals for delta-based
/// per-turn outcome emission (openai.py 4302-4339, 5273-5416).
#[derive(Debug, Default)]
struct SessionTotals {
    tokens_saved: i64,
    attempted_input_tokens: i64,
    transforms_applied: Vec<String>,
    ws_frames_compressed: u64,
    ws_client_frames_total: u64,
    ws_upstream_frames_total: u64,
    ws_response_create_frames: u64,
    ws_cancel_frames: u64,
    ws_last_client_frame_type: Option<String>,
    ws_last_upstream_frame_type: Option<String>,
    ws_client_disconnect_seen: bool,
    response_completed_seen: bool,

    input_tokens_total: i64,
    output_tokens_total: i64,
    cache_read_total: i64,
    cache_write_total: i64,
    uncached_total: i64,

    recorded_input: i64,
    recorded_output: i64,
    recorded_cache_read: i64,
    recorded_cache_write: i64,
    recorded_uncached: i64,
    recorded_saved: i64,
    recorded_attempted: i64,
    recorded_overhead_ms: f64,

    overhead_ms_total: f64,
    ws_ttfb_ms: Option<f64>,
    ttfb_emitted: bool,
    response_created_at: Option<Instant>,
    upstream_first_event_ms: Option<f64>,

    model: String,
    num_messages: i64,
}

impl SessionTotals {
    fn add_compression(&mut self, tokens_before: usize, tokens_after: usize, strategies: &[&str]) {
        self.tokens_saved += tokens_before.saturating_sub(tokens_after) as i64;
        self.attempted_input_tokens += tokens_before as i64;
        for s in strategies {
            if !self.transforms_applied.iter().any(|t| t == s) {
                self.transforms_applied.push((*s).to_string());
            }
        }
        self.ws_frames_compressed += 1;
    }

    fn add_usage(&mut self, u: &ResponsesUsage) {
        self.input_tokens_total += u.input_tokens;
        self.output_tokens_total += u.output_tokens;
        self.cache_read_total += u.cache_read_tokens;
        self.cache_write_total += u.cache_write_tokens;
        self.uncached_total += u.uncached_tokens;
    }
}

/// Immutable per-session context shared by the relay tasks.
struct SessionCtx {
    state: AppState,
    request_id: String,
    session_id: String,
    path: String,
    upstream_url: String,
    upstream_headers: HeaderMap,
    is_chatgpt: bool,
    client: Option<String>,
    ws_tags: HashMap<String, String>,
    bypass: bool,
    auth_mode: AuthMode,
    mode: CompressionMode,
    client_addr: SocketAddr,
    handler_started: Instant,
    upstream_connect_ms: Option<f64>,
    /// Counter behind [`SessionCtx::next_request_id`]. Shared with the relay
    /// task's copy of the context so the per-turn and residual emissions draw
    /// from one sequence.
    emission_seq: Arc<std::sync::atomic::AtomicU64>,
}

/// The next id in `base`'s emission sequence: `base` itself first, then
/// `base-1`, `base-2`, … The first emission keeps the session's request id so
/// the request log still lines up with the session's trace lines.
fn next_emission_id(base: &str, seq: &std::sync::atomic::AtomicU64) -> String {
    let n = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n == 0 {
        base.to_string()
    } else {
        format!("{base}-{n}")
    }
}

impl SessionCtx {
    /// A distinct request id for each emitted outcome. One WS session emits an
    /// outcome per completed turn plus a residual at close; while they all
    /// carried the session's request id the request log keyed them alike, so
    /// the dashboard's recent-requests table dropped every turn but one — and
    /// dropped unrelated rows sharing the reused key with it.
    fn next_request_id(&self) -> String {
        next_emission_id(&self.request_id, &self.emission_seq)
    }
}

/// Local mirror of proxy.rs's private `ProxyOutcomeSink` — fans a
/// [`RequestOutcome`] out to the savings tracker, cost tracker, and
/// request logger. Kept in sync with that impl.
struct CodexWsOutcomeSink {
    cost_tracker: Arc<headroom_core::cost_tracker::CostTracker>,
    savings_tracker: Arc<headroom_core::savings_tracker::SavingsTracker>,
    request_logger: Arc<crate::request_logger::RequestLogger>,
}

impl OutcomeSink for CodexWsOutcomeSink {
    fn record_request(&self, outcome: &RequestOutcome) {
        let rec = headroom_core::savings_tracker::RequestRecord {
            model: &outcome.model,
            input_tokens: outcome.original_tokens,
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
            // Same read-only estimate as the HTTP sink.
            output_tokens_saved: headroom_core::output_savings::get_recorder()
                .estimate_request_savings(&outcome.transforms_applied, outcome.output_tokens),
            // Durable lifetime metrics, as on the HTTP sink.
            output_tokens: outcome.output_tokens,
            attempted_input_tokens: outcome.attempted_input_tokens,
            cache_write_5m_tokens: outcome.cache_write_5m_tokens,
            cache_write_1h_tokens: outcome.cache_write_1h_tokens,
            cached: outcome.cache_hit(),
            stack: outcome.client.as_deref(),
            waste_signals: outcome.waste_signals.clone(),
        };
        self.savings_tracker.record_request(&rec);
    }

    fn record_tokens(&self, outcome: &RequestOutcome) {
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

    fn log_request(&self, outcome: &RequestOutcome) {
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
            headroom_core::output_savings::get_recorder().record_from_labels(&transforms, output_tokens);
        });
    }

    fn record_failed(&self, outcome: &RequestOutcome) {
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

    fn record_savings_ledger(&self, outcome: &RequestOutcome) {
        // Mirrors `ProxyOutcomeSink`: forwarded count in, helper reconstructs
        // the original, flocked append pushed off the request path.
        let forwarded = outcome.optimized_tokens;
        let saved = outcome.tokens_saved;
        let model = outcome.model.clone();
        let client = outcome.client.clone();
        let priced_cost = outcome.compression_savings_cost_usd();
        let priced_basis = outcome.compression_savings_cost_basis().to_string();
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

fn emit_outcome(state: &AppState, outcome: &RequestOutcome) {
    let sink = CodexWsOutcomeSink {
        cost_tracker: state.cost_tracker.clone(),
        savings_tracker: state.savings_tracker.clone(),
        request_logger: state.request_logger.clone(),
    };
    emit_request_outcome(&sink, outcome);
}

/// Per-turn outcome on `response.completed`. Port of
/// `_record_ws_response_metrics` (openai.py 5273-5416): delta-based so a
/// multi-turn session emits one outcome per completed response, with the
/// session-end residual sweep catching anything left over.
fn emit_per_turn_outcome(ctx: &SessionCtx, totals: &mut SessionTotals) {
    let input_delta = totals.input_tokens_total - totals.recorded_input;
    let output_delta = totals.output_tokens_total - totals.recorded_output;
    let cache_read_delta = totals.cache_read_total - totals.recorded_cache_read;
    let cache_write_delta = totals.cache_write_total - totals.recorded_cache_write;
    let uncached_delta = totals.uncached_total - totals.recorded_uncached;
    let saved_delta = totals.tokens_saved - totals.recorded_saved;
    let attempted_delta = totals.attempted_input_tokens - totals.recorded_attempted;
    let overhead_delta = totals.overhead_ms_total - totals.recorded_overhead_ms;

    if input_delta <= 0
        && output_delta <= 0
        && cache_read_delta <= 0
        && cache_write_delta <= 0
        && uncached_delta <= 0
        && saved_delta <= 0
        && attempted_delta <= 0
    {
        return;
    }

    let total_latency_ms = totals
        .response_created_at
        .map(|t| t.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let ttfb_ms = if totals.ttfb_emitted {
        0.0
    } else {
        totals.ttfb_emitted = true;
        totals.ws_ttfb_ms.unwrap_or(0.0)
    };

    let outcome = RequestOutcome {
        request_id: ctx.next_request_id(),
        provider: "openai".to_string(),
        model: if totals.model.is_empty() {
            "unknown".to_string()
        } else {
            totals.model.clone()
        },
        original_tokens: input_delta.max(0) + saved_delta.max(0),
        optimized_tokens: input_delta.max(0),
        output_tokens: output_delta.max(0),
        tokens_saved: saved_delta.max(0),
        attempted_input_tokens: attempted_delta.max(0),
        cache_read_tokens: cache_read_delta.max(0),
        cache_write_tokens: cache_write_delta.max(0),
        uncached_input_tokens: uncached_delta.max(0),
        total_latency_ms,
        overhead_ms: overhead_delta.max(0.0),
        ttfb_ms,
        transforms_applied: totals.transforms_applied.clone(),
        num_messages: totals.num_messages,
        tags: ctx.ws_tags.clone(),
        client: ctx.client.clone(),
        ..Default::default()
    };
    emit_outcome(&ctx.state, &outcome);

    totals.recorded_input = totals.input_tokens_total;
    totals.recorded_output = totals.output_tokens_total;
    totals.recorded_cache_read = totals.cache_read_total;
    totals.recorded_cache_write = totals.cache_write_total;
    totals.recorded_uncached = totals.uncached_total;
    totals.recorded_saved = totals.tokens_saved;
    totals.recorded_attempted = totals.attempted_input_tokens;
    totals.recorded_overhead_ms = totals.overhead_ms_total;
}

/// Session tags for the residual outcome + close log. Port of
/// openai.py 5845-5865 (all string values).
fn session_tags(
    ctx: &SessionCtx,
    totals: &SessionTotals,
    cause_label: &str,
) -> HashMap<String, String> {
    let mut tags = ctx.ws_tags.clone();
    tags.insert("auth_mode".into(), ctx.auth_mode.as_str().to_string());
    tags.insert("endpoint".into(), "responses_ws".into());
    tags.insert("compression_scope".into(), "live_zone".into());
    tags.insert("cache_policy".into(), "prefix_safe".into());
    tags.insert("transport".into(), "websocket".into());
    tags.insert(
        "route".into(),
        if ctx.is_chatgpt {
            "chatgpt_subscription".into()
        } else {
            "openai_api".into()
        },
    );
    tags.insert(
        "ws_response_create_frames".into(),
        totals.ws_response_create_frames.to_string(),
    );
    tags.insert(
        "ws_frames_compressed".into(),
        totals.ws_frames_compressed.to_string(),
    );
    tags.insert(
        "ws_client_frames_total".into(),
        totals.ws_client_frames_total.to_string(),
    );
    tags.insert(
        "ws_upstream_frames_total".into(),
        totals.ws_upstream_frames_total.to_string(),
    );
    tags.insert(
        "ws_cancel_frames".into(),
        totals.ws_cancel_frames.to_string(),
    );
    if let Some(t) = &totals.ws_last_client_frame_type {
        tags.insert("ws_last_client_frame_type".into(), t.clone());
    }
    if let Some(t) = &totals.ws_last_upstream_frame_type {
        tags.insert("ws_last_upstream_frame_type".into(), t.clone());
    }
    tags.insert(
        "ws_client_disconnect_seen".into(),
        totals.ws_client_disconnect_seen.to_string(),
    );
    tags.insert("ws_termination_cause".into(), cause_label.to_string());
    tags.insert(
        "cache_read_tokens".into(),
        totals.cache_read_total.to_string(),
    );
    tags.insert(
        "cache_write_tokens".into(),
        totals.cache_write_total.to_string(),
    );
    tags.insert(
        "uncached_input_tokens".into(),
        totals.uncached_total.to_string(),
    );
    tags
}

// ─────────────────────────────────────────────────────────────────────────
// Handler
// ─────────────────────────────────────────────────────────────────────────

type UpstreamWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Entry point for the four Codex responses WS paths. Performs the
/// pre-upgrade origin check, header prep, and — critically — connects the
/// upstream BEFORE returning the 101 so the upstream's `x-codex-*`
/// handshake headers (Codex subscription rate-limit window) can be attached
/// to the client-facing accept (openai.py 4110-4115).
pub async fn ws_codex_handler(
    ws: WebSocketUpgrade,
    state: AppState,
    client_addr: SocketAddr,
    req: Request<Body>,
) -> Response<Body> {
    let handler_started = Instant::now();
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();

    // ── Origin check: refuse pre-upgrade (Python closes 1008 without
    // accepting; over HTTP the equivalent is refusing the upgrade). ──
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    let allowed = allowed_ws_origins_from_env();
    if !is_allowed_websocket_origin(origin, allowed.as_deref()) {
        tracing::warn!(
            event = "websocket_origin_not_allowed",
            request_id = %request_id,
            session_id = %session_id,
            path = %path,
            origin = ?origin,
            "codex ws origin refused"
        );
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("origin not allowed"))
            .expect("static response");
    }

    // ── Client classification + codex stamping (auth_mode.py:267-282) ──
    let stamped = should_stamp_codex_client(&path, &headers);
    let client = if stamped {
        Some("codex".to_string())
    } else {
        classify_client(&headers)
    };
    let ws_tags = crate::headers::extract_tags(&headers);
    let bypass = crate::headers::headroom_bypass_enabled(&headers);
    let auth_mode = headroom_core::auth_mode::classify(&headers);
    let mode = if state.config.compression {
        state.config.compression_mode
    } else {
        CompressionMode::Off
    };

    // ── Upstream header build (openai.py 3936-3981) ──
    let strip_internal = state.config.strip_internal_headers.is_enabled();
    let mut upstream_headers = HeaderMap::new();
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if WS_SKIP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if strip_internal && lower.starts_with(crate::headers::INTERNAL_HEADER_PREFIX) {
            continue;
        }
        upstream_headers.append(name.clone(), value.clone());
    }
    if stamped {
        upstream_headers.insert(
            HeaderName::from_static("x-client"),
            HeaderValue::from_static("codex"),
        );
    }
    let is_chatgpt = resolve_codex_routing(&mut upstream_headers);
    // OpenAI rejects newer Codex models when this client-only lite header
    // leaks upstream (openai.py 3976-3981).
    upstream_headers.remove(CODEX_LITE_HEADER);

    // Auth fallback: inject OPENAI_API_KEY when the client sent none.
    if !upstream_headers.contains_key(http::header::AUTHORIZATION) {
        match std::env::var("OPENAI_API_KEY") {
            Ok(key) if !key.trim().is_empty() => {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", key.trim())) {
                    upstream_headers.insert(http::header::AUTHORIZATION, v);
                }
            }
            _ => {
                tracing::warn!(
                    request_id = %request_id,
                    "codex ws: no authorization header and OPENAI_API_KEY unset"
                );
            }
        }
    }

    // OpenAI-Beta merge (openai.py 4034-4087). SessionBetaTracker skipped:
    // WS session ids are fresh uuids so stickiness is a per-connection no-op;
    // the deterministic merge is the effective behavior.
    let client_beta = upstream_headers
        .get("openai-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let merged =
        crate::headers::merge_beta_tokens(client_beta.as_deref(), &[RESPONSES_WS_REQUIRED_BETA]);
    if !merged.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&merged) {
            upstream_headers.insert(HeaderName::from_static("openai-beta"), v);
        }
    }

    // ── Upstream URL (openai.py 3984-3995) ──
    let upstream_url: url::Url = if is_chatgpt {
        CHATGPT_CODEX_WS_URL.parse().expect("static url")
    } else {
        let mut u = state.config.upstream.clone();
        let scheme = match u.scheme() {
            "https" | "wss" => "wss",
            _ => "ws",
        };
        let _ = u.set_scheme(scheme);
        u.set_path("/v1/responses");
        u.set_query(None);
        u
    };

    let subprotocols: Vec<String> = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // ── Connect upstream BEFORE client accept ──
    let connect_started = Instant::now();
    let connect_result =
        connect_upstream_with_retry(&state, &upstream_url, &upstream_headers, &subprotocols).await;
    let upstream_connect_ms = connect_started.elapsed().as_secs_f64() * 1000.0;

    let (upstream, codex_headers): (Option<UpstreamWs>, Vec<(HeaderName, HeaderValue)>) =
        match connect_result {
            Ok((stream, resp)) => {
                // Forward `x-codex-*` (subscription rate-limit window,
                // openai.py:614-640) AND `set-cookie` (session affinity —
                // asserted by e2e_ws_codex_usage_headers.py) from the
                // upstream handshake onto the client 101. NEVER
                // `authorization` or anything else. The codex rate-limit
                // state + usage poll subsystems have no Rust counterpart —
                // skipped.
                let codex_headers = resp
                    .headers()
                    .iter()
                    .filter(|(name, _)| {
                        name.as_str().starts_with("x-codex-") || *name == http::header::SET_COOKIE
                    })
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect();
                (Some(stream), codex_headers)
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    session_id = %session_id,
                    upstream = %upstream_url,
                    error = %e,
                    "codex ws upstream connect failed; will fall back to HTTP streaming"
                );
                (None, Vec::new())
            }
        };

    let ctx = SessionCtx {
        state,
        request_id,
        session_id,
        path,
        upstream_url: upstream_url.to_string(),
        upstream_headers,
        is_chatgpt,
        client,
        ws_tags,
        bypass,
        auth_mode,
        mode,
        client_addr,
        handler_started,
        upstream_connect_ms: Some(upstream_connect_ms),
        emission_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };

    let ws = if subprotocols.is_empty() {
        ws
    } else {
        ws.protocols(subprotocols)
    };
    let mut response =
        ws.on_upgrade(move |socket| async move { run_codex_session(socket, upstream, ctx).await });
    for (name, value) in codex_headers {
        response.headers_mut().append(name, value);
    }
    response
}

/// Upstream WS connect with retry + jitter (openai.py 4131-4183).
/// `max_message_size`/`max_frame_size` = None (Python `max_size=None`
/// parity: inline base64 images exceed the default 1 MiB frame cap).
/// tungstenite answers upstream pings automatically and never enforces a
/// pong deadline — matching Python's `ping_timeout=None` intent.
async fn connect_upstream_with_retry(
    state: &AppState,
    upstream_url: &url::Url,
    upstream_headers: &HeaderMap,
    subprotocols: &[String],
) -> Result<
    (
        UpstreamWs,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    let attempts = state.config.retry_max_attempts.max(1);
    let mut last_err = String::from("no attempts made");
    for attempt in 0..attempts {
        let mut req = upstream_url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("ws into_client_request: {e}"))?;
        {
            let h = req.headers_mut();
            for (name, value) in upstream_headers.iter() {
                h.append(name.clone(), value.clone());
            }
            if !subprotocols.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&subprotocols.join(", ")) {
                    h.insert(HeaderName::from_static("sec-websocket-protocol"), v);
                }
            }
        }
        let mut config = WebSocketConfig::default();
        config.max_message_size = None;
        config.max_frame_size = None;
        // Python parity (openai.py 4133-4159): open_timeout =
        // max(30, connect_timeout*3); close_timeout=10 and ping_interval=20
        // have no tokio-tungstenite client equivalents (no automatic
        // keepalive pings); tungstenite never enforces a pong deadline,
        // which matches the load-bearing `ping_timeout=None` (image-gen
        // turns go silent 20-60s — never tear down on a missing pong).
        let open_timeout = state
            .config
            .upstream_connect_timeout
            .saturating_mul(3)
            .max(Duration::from_secs(30));
        let connect = tokio_tungstenite::connect_async_with_config(req, Some(config), false);
        match tokio::time::timeout(open_timeout, connect).await {
            Ok(Ok((stream, resp))) => return Ok((stream, resp)),
            outcome => {
                last_err = match outcome {
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => format!("open timeout after {open_timeout:?}"),
                    Ok(Ok(_)) => unreachable!("handled above"),
                };
                if attempt + 1 < attempts {
                    let delay = crate::proxy::backoff_ms(&state, attempt);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// Compression outcome of the async wrapper.
enum CompressAttempt {
    Done(FrameCompression),
    Timeout,
    Panicked,
}

/// Run the (sync, CPU-bound) compressor off the async runtime with the
/// Python `COMPRESSION_TIMEOUT_SECONDS` bound, so the fail-closed decision
/// matrix is reachable (timeout / panic) even though the Rust compressor is
/// infallible by contract.
async fn compress_frame_bounded(
    raw: String,
    mode: CompressionMode,
    auth_mode: AuthMode,
    request_id: String,
) -> CompressAttempt {
    let handle = tokio::task::spawn_blocking(move || {
        compress_response_create_frame(&raw, mode, auth_mode, &request_id)
    });
    match tokio::time::timeout(compression_timeout(), handle).await {
        Ok(Ok(result)) => CompressAttempt::Done(result),
        Ok(Err(_join_err)) => CompressAttempt::Panicked,
        Err(_elapsed) => CompressAttempt::Timeout,
    }
}

/// Receive the first client TEXT frame, skipping ping/pong. Returns `None`
/// on close/disconnect. (Python `receive_text` semantics; client binary
/// frames before the first text frame are ignored.)
async fn recv_first_text(stream: &mut SplitStream<WebSocket>) -> Option<String> {
    loop {
        match stream.next().await? {
            Ok(AxMsg::Text(t)) => return Some(t.to_string()),
            Ok(AxMsg::Close(_)) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

struct SessionEnd {
    cause: TerminationCause,
    /// Log/tag label; differs from `cause` only for `compression_refused`
    /// (the registry enum has no such variant — registered as ClientError).
    cause_label: String,
}

impl SessionEnd {
    fn of(cause: TerminationCause) -> Self {
        let cause_label = cause.to_string();
        Self { cause, cause_label }
    }
}

/// Post-upgrade session driver: registry lifecycle, first frame, relay,
/// termination classification, session-end residual outcome + close log.
async fn run_codex_session(client_ws: WebSocket, upstream: Option<UpstreamWs>, ctx: SessionCtx) {
    let accept_ms = ctx.handler_started.elapsed().as_secs_f64() * 1000.0;
    let session_started = Instant::now();

    // Register session (openai.py 4222-4245).
    {
        let mut handle = WSSessionHandle::new(ctx.session_id.clone(), ctx.request_id.clone());
        handle.client_addr = Some(ctx.client_addr.to_string());
        handle.upstream_url = Some(ctx.upstream_url.clone());
        handle.relay_task_count = 2;
        handle.relay_task_names = vec![
            format!("codex-ws-c2u-{}", ctx.session_id),
            format!("codex-ws-u2c-{}", ctx.session_id),
        ];
        ctx.state
            .ws_sessions
            .lock()
            .expect("ws_sessions lock")
            .register(handle);
    }
    inc_active_ws_sessions();
    inc_active_relay_tasks(2);

    let totals = Arc::new(Mutex::new(SessionTotals::default()));
    let mut first_client_frame_ms: Option<f64> = None;
    let mut compression_ms: Option<f64> = None;

    let end = run_codex_session_inner(
        client_ws,
        upstream,
        &ctx,
        &totals,
        &mut first_client_frame_ms,
        &mut compression_ms,
    )
    .await;

    // ── Session end: residual outcome + close log (openai.py 5799-5951,
    // 5990-6050) ──
    let total_session_ms = session_started.elapsed().as_secs_f64() * 1000.0;
    {
        let t = totals.lock().expect("totals lock");
        let tags = session_tags(&ctx, &t, &end.cause_label);

        let input_delta = t.input_tokens_total - t.recorded_input;
        let output_delta = t.output_tokens_total - t.recorded_output;
        let saved_delta = t.tokens_saved - t.recorded_saved;
        let attempted_delta = t.attempted_input_tokens - t.recorded_attempted;
        let cache_read_delta = t.cache_read_total - t.recorded_cache_read;
        let cache_write_delta = t.cache_write_total - t.recorded_cache_write;
        let uncached_delta = t.uncached_total - t.recorded_uncached;
        let overhead_delta = t.overhead_ms_total - t.recorded_overhead_ms;

        if input_delta > 0
            || output_delta > 0
            || saved_delta > 0
            || attempted_delta > 0
            || cache_read_delta > 0
            || cache_write_delta > 0
            || uncached_delta > 0
        {
            let outcome = RequestOutcome {
                request_id: ctx.next_request_id(),
                provider: "openai".to_string(),
                model: if t.model.is_empty() {
                    "unknown".to_string()
                } else {
                    t.model.clone()
                },
                original_tokens: input_delta.max(0) + saved_delta.max(0),
                optimized_tokens: input_delta.max(0),
                output_tokens: output_delta.max(0),
                tokens_saved: saved_delta.max(0),
                attempted_input_tokens: attempted_delta.max(0),
                cache_read_tokens: cache_read_delta.max(0),
                cache_write_tokens: cache_write_delta.max(0),
                uncached_input_tokens: uncached_delta.max(0),
                total_latency_ms: total_session_ms,
                overhead_ms: overhead_delta.max(0.0),
                ttfb_ms: if t.ttfb_emitted {
                    0.0
                } else {
                    t.ws_ttfb_ms.unwrap_or(0.0)
                },
                transforms_applied: t.transforms_applied.clone(),
                num_messages: t.num_messages,
                tags: tags.clone(),
                client: ctx.client.clone(),
                ..Default::default()
            };
            emit_outcome(&ctx.state, &outcome);
        }

        // Registry teardown + gauges (openai.py 5997-6025).
        let released = ctx
            .state
            .ws_sessions
            .lock()
            .expect("ws_sessions lock")
            .deregister(&ctx.session_id, end.cause.clone())
            .map(|(_, released)| released)
            .unwrap_or(0);
        dec_active_ws_sessions();
        dec_active_relay_tasks(released as i64);
        record_ws_session_duration(total_session_ms, &end.cause_label);

        // Stage-timings parity: one structured line with the Python
        // `emit_stage_timings_log` stage fields (judgment #12).
        tracing::info!(
            event = "codex_ws_session_closed",
            request_id = %ctx.request_id,
            session_id = %ctx.session_id,
            path = %ctx.path,
            termination_cause = %end.cause_label,
            ws_client_frames_total = t.ws_client_frames_total,
            ws_upstream_frames_total = t.ws_upstream_frames_total,
            ws_response_create_frames = t.ws_response_create_frames,
            ws_frames_compressed = t.ws_frames_compressed,
            ws_cancel_frames = t.ws_cancel_frames,
            tokens_saved = t.tokens_saved,
            stage_accept_ms = accept_ms,
            stage_first_client_frame_ms = first_client_frame_ms,
            stage_upstream_connect_ms = ctx.upstream_connect_ms,
            stage_upstream_first_event_ms = t.upstream_first_event_ms,
            stage_compression_ms = compression_ms,
            stage_total_session_ms = total_session_ms,
            "codex ws session closed"
        );
    }
}

async fn run_codex_session_inner(
    client_ws: WebSocket,
    upstream: Option<UpstreamWs>,
    ctx: &SessionCtx,
    totals: &Arc<Mutex<SessionTotals>>,
    first_client_frame_ms: &mut Option<f64>,
    compression_ms: &mut Option<f64>,
) -> SessionEnd {
    let (mut client_sink, mut client_stream) = client_ws.split();
    let session_started = Instant::now();

    // ── First frame (openai.py 4246-4331) ──
    let first_msg = match tokio::time::timeout(
        first_frame_timeout(),
        recv_first_text(&mut client_stream),
    )
    .await
    {
        Err(_elapsed) => {
            let _ = client_sink
                .send(AxMsg::Close(Some(CloseFrame {
                    code: 1001,
                    reason: "first-frame timeout".into(),
                })))
                .await;
            // Idempotent upstream close backstop (openai.py 5990-5992):
            // the upstream connected but the client never spoke.
            if let Some(mut up) = upstream {
                let _ = up.close(None).await;
            }
            return SessionEnd::of(TerminationCause::ClientTimeout);
        }
        Ok(None) => {
            if let Some(mut up) = upstream {
                let _ = up.close(None).await;
            }
            let mut t = totals.lock().expect("totals lock");
            t.ws_client_disconnect_seen = true;
            return SessionEnd::of(TerminationCause::ClientDisconnect);
        }
        Ok(Some(text)) => text,
    };
    *first_client_frame_ms = Some(session_started.elapsed().as_secs_f64() * 1000.0);

    // Best-effort parse for model + num_messages (openai.py 4310-4314).
    {
        let mut t = totals.lock().expect("totals lock");
        if let Ok(parsed) = serde_json::from_str::<Value>(&first_msg) {
            let inner = parsed
                .get("response")
                .filter(|v| v.is_object())
                .unwrap_or(&parsed);
            t.model = inner
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            t.num_messages = inner
                .get("messages")
                .or_else(|| inner.get("input"))
                .and_then(Value::as_array)
                .map(|a| a.len() as i64)
                .unwrap_or(0);
        }
        t.ws_client_frames_total += 1;
        if let Ok(parsed) = serde_json::from_str::<Value>(&first_msg) {
            if let Some(ty) = parsed.get("type").and_then(Value::as_str) {
                t.ws_last_client_frame_type = Some(ty.to_string());
                if ty == "response.create" {
                    t.ws_response_create_frames += 1;
                }
            }
        }
    }

    // ── First-frame compression: fail-CLOSED via the decision matrix
    // (openai.py 4647-4859) ──
    let mut first_msg_raw = first_msg;
    if ctx.bypass {
        tracing::info!(
            request_id = %ctx.request_id,
            reason = "bypass_header",
            "codex ws first-frame compression skipped"
        );
    } else {
        let comp_started = Instant::now();
        let attempt = compress_frame_bounded(
            first_msg_raw.clone(),
            ctx.mode,
            ctx.auth_mode,
            ctx.request_id.clone(),
        )
        .await;
        let elapsed_ms = comp_started.elapsed().as_secs_f64() * 1000.0;
        *compression_ms = Some(elapsed_ms);
        match attempt {
            CompressAttempt::Done(FrameCompression::Compressed {
                text,
                tokens_before,
                tokens_after,
                strategies,
                bytes_before,
                bytes_after,
            }) => {
                let mut t = totals.lock().expect("totals lock");
                t.add_compression(tokens_before, tokens_after, &strategies);
                t.overhead_ms_total += elapsed_ms;
                tracing::info!(
                    request_id = %ctx.request_id,
                    bytes_before,
                    bytes_after,
                    tokens_before,
                    tokens_after,
                    "codex ws first frame compressed"
                );
                first_msg_raw = text;
            }
            CompressAttempt::Done(FrameCompression::Passthrough { reason }) => {
                let mut t = totals.lock().expect("totals lock");
                t.overhead_ms_total += elapsed_ms;
                tracing::debug!(
                    request_id = %ctx.request_id,
                    reason,
                    "codex ws first frame passthrough"
                );
            }
            failure @ (CompressAttempt::Timeout | CompressAttempt::Panicked) => {
                let is_timeout = matches!(failure, CompressAttempt::Timeout);
                // The fail-open env override was resolved once at startup
                // into `AppState.compression_failure_action` (proxy.rs) —
                // its reason is `env_override:fail_open` iff the operator
                // opted back into legacy fail-open. Per-request inputs
                // (codex client, timeout, frame size) are resolved here.
                // The threshold is not carried by the startup struct, so it
                // comes from the same module's env parser (Python also
                // reads it at decision time).
                let fail_open =
                    ctx.state.compression_failure_action.reason == "env_override:fail_open";
                let threshold = oversize_threshold_bytes(
                    std::env::var(WS_COMPRESSION_OVERSIZE_BYTES_ENV)
                        .ok()
                        .as_deref(),
                );
                let is_codex = ctx.client.as_deref() == Some("codex");
                let action = decide_compression_failure_action(
                    fail_open,
                    is_codex,
                    is_timeout,
                    first_msg_raw.len(),
                    threshold,
                );
                tracing::warn!(
                    request_id = %ctx.request_id,
                    refuse = action.refuse,
                    reason = %action.reason,
                    frame_bytes = action.frame_bytes,
                    "codex ws first-frame compression failed"
                );
                crate::observability::proxy_counters::record_compression_failed(
                    compression_failure_metric_reason(&action.reason),
                );
                if action.refuse {
                    let reason = format!(
                        "headroom: compression {} — please compact context and retry",
                        action.reason
                    );
                    let _ = client_sink
                        .send(AxMsg::Close(Some(CloseFrame {
                            code: 1009,
                            reason: reason.into(),
                        })))
                        .await;
                    if let Some(mut up) = upstream {
                        let _ = up.close(None).await;
                    }
                    // The registry TerminationCause enum has no
                    // `compression_refused` variant; register as ClientError
                    // and keep the Python label for tags/logs.
                    return SessionEnd {
                        cause: TerminationCause::ClientError,
                        cause_label: "compression_refused".to_string(),
                    };
                }
                // forward original (fail-open branch of the matrix)
            }
        }
    }

    // ── HTTP fallback when upstream WS never connected (openai.py
    // 5779-5797, 6052+) ──
    let Some(upstream) = upstream else {
        ws_http_fallback(&mut client_sink, &first_msg_raw, ctx).await;
        let _ = client_sink.close().await;
        // Python leaves termination_cause unset on the fallback arm.
        return SessionEnd::of(TerminationCause::Unknown);
    };

    // ── Relay (openai.py 4861-5778) ──
    let (mut up_sink, mut up_stream) = upstream.split();
    if let Err(e) = up_sink.send(TgMsg::Text(first_msg_raw.into())).await {
        tracing::warn!(request_id = %ctx.request_id, error = %e, "codex ws first-frame upstream send failed");
        let _ = client_sink
            .send(AxMsg::Close(Some(CloseFrame {
                code: 1011,
                reason: "upstream send failed".into(),
            })))
            .await;
        return SessionEnd::of(TerminationCause::UpstreamError);
    }

    let cancel = tokio_util::sync::CancellationToken::new();

    // Client → upstream (openai.py 5105-5219). Per-frame compression is
    // fail-OPEN (contrast with the first frame's fail-closed matrix —
    // intentional Python asymmetry, judgment #10).
    let c2u = {
        let cancel = cancel.clone();
        let totals = Arc::clone(&totals);
        let ctx_state = ctx.state.clone();
        let request_id = ctx.request_id.clone();
        let session_id = ctx.session_id.clone();
        let bypass = ctx.bypass;
        let mode = ctx.mode;
        let auth_mode = ctx.auth_mode;
        tokio::spawn(async move {
            let mut had_error = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = client_stream.next() => {
                        let Some(msg) = msg else {
                            totals.lock().expect("totals lock").ws_client_disconnect_seen = true;
                            break;
                        };
                        let m = match msg {
                            Ok(m) => m,
                            Err(_) => {
                                totals.lock().expect("totals lock").ws_client_disconnect_seen = true;
                                break;
                            }
                        };
                        match m {
                            AxMsg::Text(text) => {
                                let text = text.to_string();
                                let frame_type = serde_json::from_str::<Value>(&text)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("type").and_then(Value::as_str).map(str::to_string)
                                    });
                                let is_create = frame_type.as_deref() == Some("response.create");
                                {
                                    let mut t = totals.lock().expect("totals lock");
                                    t.ws_client_frames_total += 1;
                                    if let Some(ty) = &frame_type {
                                        t.ws_last_client_frame_type = Some(ty.clone());
                                        if ty == "response.cancel" {
                                            t.ws_cancel_frames += 1;
                                        }
                                        if is_create {
                                            t.ws_response_create_frames += 1;
                                        }
                                    }
                                }
                                if let Ok(mut reg) = ctx_state.ws_sessions.lock() {
                                    reg.mark_activity(&session_id);
                                }
                                let out = if is_create && !bypass {
                                    let comp_started = Instant::now();
                                    match compress_frame_bounded(
                                        text.clone(),
                                        mode,
                                        auth_mode,
                                        request_id.clone(),
                                    )
                                    .await
                                    {
                                        CompressAttempt::Done(FrameCompression::Compressed {
                                            text: rewritten,
                                            tokens_before,
                                            tokens_after,
                                            strategies,
                                            ..
                                        }) => {
                                            let mut t = totals.lock().expect("totals lock");
                                            t.add_compression(
                                                tokens_before,
                                                tokens_after,
                                                &strategies,
                                            );
                                            t.overhead_ms_total +=
                                                comp_started.elapsed().as_secs_f64() * 1000.0;
                                            rewritten
                                        }
                                        // Fail-open: forward the original frame.
                                        _ => text,
                                    }
                                } else {
                                    text
                                };
                                if up_sink.send(TgMsg::Text(out.into())).await.is_err() {
                                    had_error = true;
                                    break;
                                }
                            }
                            AxMsg::Close(_) => {
                                totals.lock().expect("totals lock").ws_client_disconnect_seen =
                                    true;
                                let _ = up_sink.send(TgMsg::Close(None)).await;
                                break;
                            }
                            // Binary/ping/pong: forward untouched (superset of
                            // Python, which only reads text frames).
                            other => {
                                if let Some(tg) = ax_to_tg(other) {
                                    if up_sink.send(tg).await.is_err() {
                                        had_error = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = up_sink.close().await;
            cancel.cancel();
            had_error
        })
    };

    // Upstream → client (openai.py 5221-5645): byte-equal passthrough plus
    // observation (usage extraction + per-turn outcome). No memory-tool
    // suppression is ported — that path is dead code in Python (an
    // unconditional flush+continue makes Phases 2a/2b unreachable).
    let u2c = {
        let cancel = cancel.clone();
        let totals = Arc::clone(&totals);
        let emit_ctx = SessionCtx {
            state: ctx.state.clone(),
            request_id: ctx.request_id.clone(),
            session_id: ctx.session_id.clone(),
            path: ctx.path.clone(),
            upstream_url: ctx.upstream_url.clone(),
            upstream_headers: HeaderMap::new(),
            is_chatgpt: ctx.is_chatgpt,
            client: ctx.client.clone(),
            ws_tags: ctx.ws_tags.clone(),
            bypass: ctx.bypass,
            auth_mode: ctx.auth_mode,
            mode: ctx.mode,
            client_addr: ctx.client_addr,
            handler_started: ctx.handler_started,
            upstream_connect_ms: ctx.upstream_connect_ms,
            emission_seq: Arc::clone(&ctx.emission_seq),
        };
        tokio::spawn(async move {
            let mut had_error = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = up_stream.next() => {
                        let Some(msg) = msg else { break };
                        let m = match msg {
                            Ok(m) => m,
                            Err(_) => {
                                had_error = true;
                                break;
                            }
                        };
                        match m {
                            TgMsg::Text(text) => {
                                {
                                    let mut t = totals.lock().expect("totals lock");
                                    t.ws_upstream_frames_total += 1;
                                    if t.ws_ttfb_ms.is_none() {
                                        let ttfb =
                                            session_started.elapsed().as_secs_f64() * 1000.0;
                                        t.ws_ttfb_ms = Some(ttfb);
                                        t.upstream_first_event_ms = Some(ttfb);
                                    }
                                }
                                if let Ok(event) = serde_json::from_str::<Value>(&text) {
                                    let ty = event
                                        .get("type")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    let usage = extract_responses_usage(&event);
                                    let mut t = totals.lock().expect("totals lock");
                                    t.ws_last_upstream_frame_type = Some(ty.clone());
                                    if ty == "response.created" {
                                        t.response_created_at = Some(Instant::now());
                                    }
                                    t.add_usage(&usage);
                                    if ty == "response.completed" {
                                        t.response_completed_seen = true;
                                        emit_per_turn_outcome(&emit_ctx, &mut t);
                                    }
                                }
                                // Forward the ORIGINAL string — never a
                                // re-serialization.
                                if client_sink.send(AxMsg::Text(text.as_str().into())).await.is_err() {
                                    break;
                                }
                            }
                            TgMsg::Close(cf) => {
                                {
                                    let mut t = totals.lock().expect("totals lock");
                                    t.ws_upstream_frames_total += 1;
                                }
                                if let Some(ax) = tg_to_ax(TgMsg::Close(cf)) {
                                    let _ = client_sink.send(ax).await;
                                }
                                break;
                            }
                            other => {
                                {
                                    let mut t = totals.lock().expect("totals lock");
                                    t.ws_upstream_frames_total += 1;
                                }
                                if let Some(ax) = tg_to_ax(other) {
                                    if client_sink.send(ax).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = client_sink.close().await;
            cancel.cancel();
            had_error
        })
    };

    // FIRST_COMPLETED orchestration + termination classification
    // (openai.py 5647-5754).
    tokio::pin!(c2u);
    tokio::pin!(u2c);
    let first = tokio::select! {
        r = &mut c2u => {
            let error = r.unwrap_or(true);
            cancel.cancel();
            let _ = (&mut u2c).await;
            FirstDone::Client { error }
        }
        r = &mut u2c => {
            let error = r.unwrap_or(true);
            cancel.cancel();
            let _ = (&mut c2u).await;
            FirstDone::Upstream { error }
        }
    };

    let (response_completed_seen, cancel_frames) = {
        let t = totals.lock().expect("totals lock");
        (t.response_completed_seen, t.ws_cancel_frames)
    };
    SessionEnd::of(classify_termination(
        first,
        response_completed_seen,
        cancel_frames,
    ))
}

/// HTTP POST + SSE→WS relay when the upstream WS connect failed entirely.
/// Port of `_ws_http_fallback` (openai.py 6052+). The body is the
/// already-compressed first frame with the envelope unwrapped and
/// `stream: true` forced.
async fn ws_http_fallback(
    client_sink: &mut SplitSink<WebSocket, AxMsg>,
    first_msg_raw: &str,
    ctx: &SessionCtx,
) {
    let http_url = if ctx.is_chatgpt {
        CHATGPT_CODEX_HTTP_URL.to_string()
    } else {
        let mut u = ctx.state.config.upstream.clone();
        u.set_path("/v1/responses");
        u.set_query(None);
        u.to_string()
    };

    // Normalize the WS response.create payload into the HTTP request body.
    let mut http_body: Value = match serde_json::from_str::<Value>(first_msg_raw) {
        Ok(parsed) => {
            if let Some(inner) = parsed.get("response").filter(|v| v.is_object()) {
                inner.clone()
            } else if parsed.is_object() {
                let mut obj = parsed;
                if let Some(map) = obj.as_object_mut() {
                    if matches!(
                        map.get("type").and_then(Value::as_str),
                        Some("response.create") | Some("response")
                    ) {
                        map.remove("type");
                    }
                }
                obj
            } else {
                Value::Object(serde_json::Map::new())
            }
        }
        Err(_) => Value::Object(serde_json::Map::new()),
    };
    if let Some(map) = http_body.as_object_mut() {
        map.insert("stream".to_string(), Value::Bool(true));
    }
    let body_bytes = match serde_json::to_vec(&http_body) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(request_id = %ctx.request_id, error = %e, "ws http fallback: body serialize failed");
            return;
        }
    };

    let mut headers = ctx.upstream_headers.clone();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    tracing::info!(
        request_id = %ctx.request_id,
        url = %http_url,
        "codex ws → HTTP fallback POST"
    );

    let attempts = ctx.state.config.retry_max_attempts.max(1);
    let mut response = None;
    for attempt in 0..attempts {
        match ctx
            .state
            .client
            .post(&http_url)
            .headers(headers.clone())
            .body(body_bytes.clone())
            .timeout(Duration::from_secs(120))
            .send()
            .await
        {
            Ok(resp) => {
                response = Some(resp);
                break;
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %ctx.request_id,
                    attempt = attempt + 1,
                    error = %e,
                    "ws http fallback request failed"
                );
                if attempt + 1 < attempts {
                    let delay = crate::proxy::backoff_ms(&ctx.state, attempt);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }
    let Some(response) = response else {
        let error_event = serde_json::json!({
            "type": "error",
            "error": {"type": "server_error", "message": "Upstream unreachable"},
        });
        let _ = client_sink.send(AxMsg::Text(error_event.to_string().into())).await;
        return;
    };

    if response.status() != reqwest::StatusCode::OK {
        let status = response.status().as_u16();
        tracing::warn!(
            request_id = %ctx.request_id,
            status,
            "ws http fallback got non-200"
        );
        let error_event = serde_json::json!({
            "type": "error",
            "error": {
                "type": "server_error",
                "message": format!("Upstream returned {status}"),
            },
        });
        let _ = client_sink.send(AxMsg::Text(error_event.to_string().into())).await;
        return;
    }

    // Relay SSE `data:` lines as WS text frames.
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buffer.find('\n') {
            let line: String = buffer.drain(..=idx).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    continue;
                }
                if client_sink
                    .send(AxMsg::Text(data.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // `event:` lines are skipped — the data line carries the type.
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Unit tests (pure helpers)
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    // ── path routing ─────────────────────────────────────────────

    #[test]
    fn codex_paths_recognized() {
        for p in [
            "/v1/responses",
            "/v1/codex/responses",
            "/backend-api/responses",
            "/backend-api/codex/responses",
        ] {
            assert!(is_codex_responses_path(p), "{p}");
        }
        assert!(!is_codex_responses_path("/v1/responses/xyz"));
        assert!(!is_codex_responses_path("/v1/chat/completions"));
        assert!(!is_codex_responses_path("/ws"));
    }

    // ── origin policy ────────────────────────────────────────────

    #[test]
    fn origin_missing_is_allowed() {
        assert!(is_allowed_websocket_origin(None, None));
        assert!(is_allowed_websocket_origin(Some(""), None));
    }

    #[test]
    fn origin_loopback_allowed_without_env() {
        assert!(is_allowed_websocket_origin(
            Some("http://localhost:3000"),
            None
        ));
        assert!(is_allowed_websocket_origin(Some("http://127.0.0.1"), None));
        assert!(!is_allowed_websocket_origin(
            Some("https://evil.example.com"),
            None
        ));
    }

    #[test]
    fn origin_env_allowlist_and_wildcard() {
        let allowed = vec!["https://app.example.com".to_string()];
        assert!(is_allowed_websocket_origin(
            Some("https://app.example.com"),
            Some(&allowed)
        ));
        assert!(is_allowed_websocket_origin(
            // default port elided on both sides
            Some("https://app.example.com:443"),
            Some(&allowed)
        ));
        assert!(!is_allowed_websocket_origin(
            Some("https://other.example.com"),
            Some(&allowed)
        ));
        let wild = vec!["*".to_string()];
        assert!(is_allowed_websocket_origin(
            Some("https://anything.example.com"),
            Some(&wild)
        ));
    }

    #[test]
    fn normalize_origin_cases() {
        assert_eq!(
            normalize_origin("HTTPS://App.Example.COM:443").as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(
            normalize_origin("http://h:8080").as_deref(),
            Some("http://h:8080")
        );
        assert_eq!(normalize_origin("ftp://x"), None);
        assert_eq!(normalize_origin("not a url"), None);
    }

    // ── client classification / stamping ─────────────────────────

    #[test]
    fn classify_client_explicit_header_wins() {
        let h = hm(&[("x-client", " Codex "), ("user-agent", "aider/1.0")]);
        assert_eq!(classify_client(&h).as_deref(), Some("codex"));
    }

    #[test]
    fn classify_client_ua_substring() {
        let h = hm(&[("user-agent", "corp-wrapper codex-cli/2.1")]);
        assert_eq!(classify_client(&h).as_deref(), Some("codex"));
        let h = hm(&[("user-agent", "Mozilla/5.0")]);
        assert_eq!(classify_client(&h), None);
    }

    #[test]
    fn stamp_codex_only_for_unidentified_on_responses_path() {
        let unknown = hm(&[("user-agent", "Codex Desktop/1.0")]);
        assert!(should_stamp_codex_client("/v1/responses", &unknown));
        assert!(should_stamp_codex_client("/v1/responses/sub", &unknown));
        assert!(!should_stamp_codex_client(
            "/backend-api/responses",
            &unknown
        ));
        let known = hm(&[("user-agent", "codex-cli/1.0")]);
        assert!(!should_stamp_codex_client("/v1/responses", &known));
    }

    // ── merge_openai_beta ────────────────────────────────────────

    #[test]
    fn openai_beta_merge_appends_required_token() {
        let merged =
            crate::headers::merge_beta_tokens(Some("foo=1,Bar=2"), &[RESPONSES_WS_REQUIRED_BETA]);
        assert_eq!(merged, format!("foo=1,Bar=2,{RESPONSES_WS_REQUIRED_BETA}"));
    }

    #[test]
    fn openai_beta_merge_dedupes_case_insensitively() {
        let upper = RESPONSES_WS_REQUIRED_BETA.to_ascii_uppercase();
        let merged =
            crate::headers::merge_beta_tokens(Some(upper.as_str()), &[RESPONSES_WS_REQUIRED_BETA]);
        // First occurrence's casing wins; no duplicate appended.
        assert_eq!(merged, upper);
    }

    // ── JWT routing hint ─────────────────────────────────────────

    fn make_jwt(payload: &Value) -> String {
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.{}",
            enc(b"{\"alg\":\"none\"}"),
            enc(payload.to_string().as_bytes()),
            enc(b"sig")
        )
    }

    #[test]
    fn jwt_payload_decodes_without_verification() {
        let token = make_jwt(&json!({"sub": "u1"}));
        let payload = decode_bearer_jwt_payload(&format!("Bearer {token}")).unwrap();
        assert_eq!(payload["sub"], "u1");
    }

    #[test]
    fn jwt_decode_rejects_non_bearer_and_non_jwt() {
        assert!(decode_bearer_jwt_payload("Basic dXNlcjpwYXNz").is_none());
        assert!(decode_bearer_jwt_payload("Bearer sk-not-a-jwt").is_none());
        assert!(decode_bearer_jwt_payload("Bearer a.b").is_none());
        assert!(decode_bearer_jwt_payload("Bearer a.!!!.c").is_none());
    }

    #[test]
    fn routing_explicit_header_short_circuits() {
        let mut h = hm(&[("chatgpt-account-id", "acct_1")]);
        assert!(resolve_codex_routing(&mut h));
    }

    #[test]
    fn routing_derives_account_id_from_jwt() {
        let token = make_jwt(&json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": " acct_42 "}
        }));
        let mut h = hm(&[("authorization", &format!("Bearer {token}"))]);
        assert!(resolve_codex_routing(&mut h));
        assert_eq!(
            h.get("chatgpt-account-id").unwrap().to_str().unwrap(),
            "acct_42"
        );
    }

    #[test]
    fn routing_no_hint_is_api_route() {
        let mut h = hm(&[("authorization", "Bearer sk-test-123")]);
        assert!(!resolve_codex_routing(&mut h));
        assert!(!h.contains_key("chatgpt-account-id"));
    }

    // ── usage extraction ─────────────────────────────────────────

    #[test]
    fn usage_only_from_response_completed() {
        let ev = json!({"type": "response.output_text.delta", "usage": {"input_tokens": 5}});
        assert_eq!(extract_responses_usage(&ev), ResponsesUsage::default());
    }

    #[test]
    fn usage_extracted_with_cache_details() {
        let ev = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 25,
                    "input_tokens_details": {"cached_tokens": 60}
                }
            }
        });
        let u = extract_responses_usage(&ev);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 25);
        assert_eq!(u.cache_read_tokens, 60);
        assert_eq!(u.cache_write_tokens, 40);
        assert_eq!(u.uncached_tokens, 40);
    }

    #[test]
    fn usage_tolerates_top_level_usage_and_garbage() {
        let ev =
            json!({"type": "response.completed", "usage": {"input_tokens": 7, "output_tokens": 3}});
        let u = extract_responses_usage(&ev);
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
        let ev = json!({"type": "response.completed"});
        assert_eq!(extract_responses_usage(&ev), ResponsesUsage::default());
        let ev =
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": "x"}}});
        assert_eq!(extract_responses_usage(&ev).input_tokens, 0);
    }

    // ── frame compression envelope logic ─────────────────────────

    const RID: &str = "req-test";

    #[test]
    fn frame_non_json_passthrough() {
        let out = compress_response_create_frame(
            "not json",
            CompressionMode::LiveZone,
            AuthMode::Payg,
            RID,
        );
        assert!(matches!(
            out,
            FrameCompression::Passthrough { reason: "non_json" }
        ));
    }

    #[test]
    fn frame_not_response_create_passthrough() {
        let raw = json!({"type": "session.update", "x": 1}).to_string();
        let out =
            compress_response_create_frame(&raw, CompressionMode::LiveZone, AuthMode::Payg, RID);
        assert!(matches!(
            out,
            FrameCompression::Passthrough {
                reason: "not_response_create"
            }
        ));
    }

    #[test]
    fn frame_invalid_inner_payload_passthrough() {
        let raw = json!({"type": "response.create", "response": "nope"}).to_string();
        let out =
            compress_response_create_frame(&raw, CompressionMode::LiveZone, AuthMode::Payg, RID);
        assert!(matches!(
            out,
            FrameCompression::Passthrough {
                reason: "invalid_inner_payload"
            }
        ));
    }

    #[test]
    fn frame_mode_off_passthrough() {
        let raw = json!({
            "type": "response.create",
            "response": {"model": "gpt-x", "input": [{"type": "message", "role": "user", "content": "hi"}]}
        })
        .to_string();
        let out = compress_response_create_frame(&raw, CompressionMode::Off, AuthMode::Payg, RID);
        assert!(matches!(
            out,
            FrameCompression::Passthrough {
                reason: "optimize_disabled"
            }
        ));
    }

    fn big_compressible_inner() -> Value {
        // Repetitive structured log output — reliably compressible by the
        // live-zone content router at >2 KiB.
        let blob = "{\"level\": \"info\", \"msg\": \"request handled ok\", \"latency_ms\": 12}\n"
            .repeat(400);
        json!({
            "model": "gpt-5.4-codex",
            "instructions": "You are Codex.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run tests"}]},
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"},
                {"type": "function_call_output", "call_id": "call_1", "output": blob}
            ],
            "stream": true
        })
    }

    #[test]
    fn frame_wrapped_envelope_compresses_and_rewraps() {
        let raw =
            json!({"type": "response.create", "response": big_compressible_inner()}).to_string();
        let out =
            compress_response_create_frame(&raw, CompressionMode::LiveZone, AuthMode::Payg, RID);
        match out {
            FrameCompression::Compressed {
                text,
                tokens_before,
                tokens_after,
                ..
            } => {
                assert!(tokens_after < tokens_before);
                let parsed: Value = serde_json::from_str(&text).unwrap();
                // Envelope shape preserved.
                assert_eq!(parsed["type"], "response.create");
                assert!(parsed["response"].is_object());
                assert_eq!(parsed["response"]["model"], "gpt-5.4-codex");
                assert!(text.len() < raw.len());
            }
            other => panic!("expected Compressed, got {other:?}"),
        }
    }

    #[test]
    fn frame_additional_tools_are_restored_for_stateful_sessions() {
        let mut inner = big_compressible_inner();
        inner["input"].as_array_mut().unwrap().insert(
            1,
            json!({
                "type": "additional_tools",
                "id": "tool-transcript-item",
                "tools": [{
                    "type": "function",
                    "name": "shell",
                    "description": "Run a shell command",
                    "parameters": {"type": "object", "properties": {}}
                }]
            }),
        );
        let raw = json!({"type": "response.create", "response": inner}).to_string();
        let out =
            compress_response_create_frame(&raw, CompressionMode::LiveZone, AuthMode::Payg, RID);
        let FrameCompression::Compressed { text, .. } = out else {
            panic!("expected compressed frame, got {out:?}");
        };
        let forwarded: Value = serde_json::from_str(&text).unwrap();
        let response = &forwarded["response"];
        assert!(response.get("tools").is_none());
        let carrier = response["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "additional_tools")
            .expect("tool carrier restored into the transcript");
        assert_eq!(carrier["id"], "tool-transcript-item");
        assert_eq!(carrier["tools"][0]["name"], "shell");
    }

    #[test]
    fn frame_bare_envelope_compresses_in_place() {
        let mut bare = big_compressible_inner();
        bare.as_object_mut()
            .unwrap()
            .insert("type".to_string(), json!("response.create"));
        let raw = bare.to_string();
        let out =
            compress_response_create_frame(&raw, CompressionMode::LiveZone, AuthMode::Payg, RID);
        match out {
            FrameCompression::Compressed { text, .. } => {
                let parsed: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["type"], "response.create");
                assert!(parsed.get("response").is_none());
                assert!(text.len() < raw.len());
            }
            other => panic!("expected Compressed, got {other:?}"),
        }
    }

    // ── termination classification ───────────────────────────────

    #[test]
    fn termination_matrix() {
        use TerminationCause::*;
        assert_eq!(
            classify_termination(FirstDone::Client { error: true }, false, 0),
            ClientError
        );
        assert_eq!(
            classify_termination(FirstDone::Client { error: false }, false, 0),
            ClientDisconnect
        );
        assert_eq!(
            classify_termination(FirstDone::Upstream { error: true }, false, 0),
            UpstreamError
        );
        assert_eq!(
            classify_termination(FirstDone::Upstream { error: false }, true, 0),
            ResponseCompleted
        );
        assert_eq!(
            classify_termination(FirstDone::Upstream { error: false }, false, 0),
            UpstreamDisconnect
        );
    }

    #[test]
    fn termination_client_cancel_override() {
        use TerminationCause::*;
        // Cancel frames + no completed response + disconnect-ish cause.
        assert_eq!(
            classify_termination(FirstDone::Upstream { error: false }, false, 1),
            ClientCancel
        );
        assert_eq!(
            classify_termination(FirstDone::Client { error: false }, false, 2),
            ClientCancel
        );
        // Completed response suppresses the override.
        assert_eq!(
            classify_termination(FirstDone::Upstream { error: false }, true, 1),
            ResponseCompleted
        );
        // Error causes are not overridden.
        assert_eq!(
            classify_termination(FirstDone::Upstream { error: true }, false, 1),
            UpstreamError
        );
    }
}

/// Reduce a compression-failure reason to a bounded Prometheus label.
///
/// Python's label set is only `{timeout, error}`, but Rust's reasons carry more
/// diagnostic value and are worth keeping. The one thing that cannot survive is
/// an embedded measurement: `decide_compression_failure_action` produces
/// `oversize:bytes=123>threshold=456`, and using that verbatim would mint a new
/// Prometheus series per distinct frame size. Anything with a `:` payload is
/// truncated to its stable prefix, so the label space stays bounded.
fn compression_failure_metric_reason(reason: &str) -> &str {
    match reason.split_once(':') {
        // `env_override:fail_open` and `client_override:codex` have a fixed,
        // finite tail, so they keep it.
        Some(("env_override" | "client_override", _)) => reason,
        Some((prefix, _)) => prefix,
        None => reason,
    }
}

#[cfg(test)]
mod compression_failure_metric_tests {
    use super::*;

    /// The measurement-bearing reason must collapse, or every distinct frame
    /// size would create its own Prometheus series.
    #[test]
    fn a_reason_carrying_a_measurement_collapses_to_its_prefix() {
        assert_eq!(
            compression_failure_metric_reason("oversize:bytes=123>threshold=456"),
            "oversize"
        );
        assert_eq!(
            compression_failure_metric_reason("oversize:bytes=999999>threshold=456"),
            "oversize"
        );
    }

    /// Reasons with a fixed, finite tail keep their full text — they carry real
    /// information and cannot explode cardinality.
    #[test]
    fn fixed_tail_reasons_are_preserved() {
        for reason in ["env_override:fail_open", "client_override:codex"] {
            assert_eq!(compression_failure_metric_reason(reason), reason);
        }
    }

    #[test]
    fn plain_reasons_pass_through() {
        assert_eq!(compression_failure_metric_reason("timeout"), "timeout");
        assert_eq!(
            compression_failure_metric_reason("small_frame_transient"),
            "small_frame_transient"
        );
    }

    // ── per-emission request ids ─────────────────────────────────

    /// A multi-turn session emits one outcome per completed turn plus a
    /// residual at close, all off one counter. Each needs its own request-log
    /// key.
    #[test]
    fn emission_ids_are_unique_per_turn() {
        let seq = std::sync::atomic::AtomicU64::new(0);
        let ids: Vec<String> = (0..3).map(|_| next_emission_id("req-1", &seq)).collect();
        assert_eq!(ids, vec!["req-1", "req-1-1", "req-1-2"]);
    }
}
