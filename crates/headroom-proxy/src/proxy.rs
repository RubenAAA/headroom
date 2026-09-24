//! Core reverse-proxy router and HTTP forwarding handler.

mod app;
mod ccr_expansion;
mod ccr_response;
mod continuation;
mod egress;
mod forward;
mod hooks;
mod inflight;
mod memory_continuation;
mod outcome;
mod reasoning;
mod replay;
mod request_transforms;
mod sse;
mod sse_anthropic;
mod sse_openai;
mod state;
mod tool_pairing;
mod upstream;

// Globs cap each item at its own visibility: `pub` items stay public
// API (`proxy` is a `pub mod`), the rest stay in-crate. Modules with
// no `pub` item would otherwise warn that they re-export nothing.
#[allow(unused_imports)]
pub use self::{
    app::*, ccr_expansion::*, continuation::*, egress::*, hooks::*, inflight::*, outcome::*,
    reasoning::*, replay::*, request_transforms::*, sse::*, state::*, tool_pairing::*, upstream::*,
};

pub(crate) use memory_continuation::{
    MemoryToolContext, handle_memory_response, memory_tool_context,
};

use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use url::Url;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, Request, Response, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
#[cfg(test)]
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt};

use crate::cache_stabilization;
use crate::cache_stabilization::beta_sticky::BetaProvider;
use crate::cache_stabilization::drift_detector::{
    ApiKind, DriftState, compute_structural_hash, derive_session_key, observe_drift_with_birth,
    stream_lane_key,
};
use crate::cache_stabilization::prefix_replay::{REPLAY_STORE_CAPACITY, SessionReplayStore};
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
use headroom_core::auth_mode::{AuthMode, classify as classify_auth_mode};
use headroom_core::compression_policy::CompressionPolicy;

/// Maximum number of messages allowed in a request body.
/// Mirrors Python's `MAX_MESSAGE_ARRAY_LENGTH = 10_000`.
const MAX_MESSAGE_ARRAY_LENGTH: usize = 10_000;

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
        if let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(&buffered)
            && let Some(shed) = forward::analyze_buffered_session(
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
            )
        {
            return Ok(shed);
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
mod tests;

#[cfg(test)]
mod inbound_metrics_tests;

#[cfg(test)]
mod timing_field_tests;
