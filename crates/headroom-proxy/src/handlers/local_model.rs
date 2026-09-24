//! Local model routing handler.
//!
//! Thin dispatch over the shared machinery in [`crate::routed`]: the
//! orchestrator below answers the sidecar, resolves the route, prepares,
//! translates, sends and fans out — every stage lives in [`crate::routed`]
//! (`routing`, `prepare`, `translation`, `retry`, `response_arms`,
//! `sidecar`, `auth`, `ccr`, `redaction`).

use crate::proxy::{forward_http, AppState};
use crate::routed::ccr::RoutedCcr;
use crate::routed::outcome::{build_routed_outcome_context, RerouteOrigin};
use crate::routed::prepare::prepare_turn;
use crate::routed::quirks::{classify_upstream, UpstreamKind};
use crate::routed::response_arms::{
    fold_buffered, handle_passthrough, handle_routed_error_response, handle_streaming_response,
};
use crate::routed::retry::send_with_retry;
use crate::routed::routing::{apply_model_routing, dispatch_route_fallback, RouteTarget};
use crate::routed::sidecar::handle_sidecar;
use crate::routed::translation::translate_routed_request;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{json, Value};
use std::net::SocketAddr;

/// Handle GET `/v1/models` for Claude Code's gateway model-discovery feature
/// (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`).
///
/// Claude Code reads `id`/`display_name` from `data[]` and drops any entry
/// whose `id` doesn't start with `claude` or `anthropic`, so only exact-match
/// routes (never `*`-suffixed prefix routes, which aren't a single
/// selectable model) with a qualifying `model_prefix` are listed. Operators
/// who want a routed model (Codex, Grok, ...) to show up in `/model` name
/// its route with a `claude-`/`anthropic-` prefix, e.g.
/// `--extra-model-route "claude-grok-4.6=cursor:cursor-grok-4.6-high"`.
pub async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    fn discoverable(id: &str) -> bool {
        id.starts_with("claude") || id.starts_with("anthropic")
    }

    let mut data: Vec<Value> = Vec::new();

    if let Some(local_model) = &state.config.local_model {
        if discoverable(local_model) {
            data.push(json!({
                "id": local_model,
                "display_name": format!("{local_model} (headroom local model)"),
            }));
        }
    }

    for route in &state.config.model_routes {
        if route.prefix_match || !discoverable(&route.model_prefix) {
            continue;
        }
        let display_name = if let Some(cursor_model) = &route.cursor_agent {
            format!("{cursor_model} (via cursor-agent)")
        } else if let Some(upstream) = &route.upstream {
            format!("{} (via {})", route.model_prefix, upstream.authority())
        } else {
            route.model_prefix.clone()
        };
        data.push(json!({
            "id": route.model_prefix,
            "display_name": display_name,
        }));
    }

    axum::Json(json!({ "data": data }))
}

/// Handle POST `/v1/messages` with local model routing.
///
/// 1. Buffer the body
/// 2. Check if `model` matches the configured local model
/// 3. If yes: translate request -> forward to local upstream -> translate response
/// 4. If no: delegate to `forward_http()` (transparent passthrough)
pub async fn handle_messages(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Clock for the request outcome, started before any work so the recorded
    // latency covers what the client actually waited for.
    let request_started = std::time::Instant::now();
    // Held until the response is dispatched (all exits drop it): routed turns
    // count toward the process in-flight total (`GET /debug/inflight`)
    // exactly like `forward_http` turns, so rotation drain sees them. This
    // covers headers-wait, buffered bodies, CCR continuations, and fallback
    // re-dispatch. Streamed body bytes are covered separately: every SSE body
    // is `track_streaming`-wrapped before `Body::from_stream`, so the drain
    // stays nonzero until the last byte. Overcounts briefly when delegating to
    // `forward_http` (both guards held) — conservative is the safe direction
    // for a drain check.
    let _inflight = crate::proxy::InflightGuard::enter();
    // One id for this turn, shared by the request outcome and the prefix-replay
    // store — `begin_request` parks under it and the response side hands it
    // back to `complete`, so the two must be the same value.
    let request_id = uuid::Uuid::new_v4().to_string();
    // Parse body to extract model name.
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON — can't be a model-routed request, delegate to forward_http.
            return forward_raw_to_forwarder(state, client_addr, method, uri, headers, body).await;
        }
    };

    // Claude Code's spinner-text sidecar is answered here and goes no further.
    if let Some(resp) =
        maybe_answer_sidecar(&state, &headers, &client_addr, &uri, &parsed, &request_id).await
    {
        return resp;
    }

    // Cost-aware model routing (#1706): the same helper the passthrough path
    // uses, applied here so a rewritten id still meets the route table below.
    // Disabled by default; when no rule matches the bytes come back untouched.
    //
    // Runs after the sidecar block on purpose: a sidecar is answered and gone
    // before this line, so routing can never claim one.
    let routing = apply_model_routing(&state, parsed, body, &request_id);
    let parsed = routing.parsed;
    let body = routing.body;
    let identity_model = routing.identity_model;

    // Owned (not borrowed): `parsed` is moved into `prepare_turn` below
    // while `body_model` is still needed after, so a borrow would dangle.
    // One tiny alloc; the serialize-once below is the real win here.
    let body_model: String = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body_model = body_model.as_str();

    // Route resolution (C2): cursor beats URL, first-match wins. The
    // cost-router rewrite above already ran, so this sees the post-rewrite id.
    let target =
        match crate::handlers::route_resolve::resolve_route(&state.config, &parsed, body_model) {
            crate::handlers::route_resolve::RouteDecision::Cursor { cursor_model } => {
                return dispatch_cursor_route(
                    state,
                    &headers,
                    &client_addr,
                    &parsed,
                    body_model,
                    &cursor_model,
                    &request_id,
                )
                .await;
            }
            crate::handlers::route_resolve::RouteDecision::Route { target } => target,
            crate::handlers::route_resolve::RouteDecision::NoMatch => {
                // No route matched — delegate to standard forwarder.
                return delegate_unmatched_to_forwarder(
                    state,
                    client_addr,
                    method,
                    uri,
                    headers,
                    body,
                )
                .await;
            }
        };
    let RouteTarget {
        upstream,
        translate,
        target_model,
        auth_env,
    } = target;

    let anthropic_target = !translate && target_model.is_some();
    let auth = resolve_upstream_auth(
        &state,
        auth_env.as_deref(),
        &headers,
        &upstream,
        &request_id,
        anthropic_target,
    );
    let (upstream_headers, is_chatgpt_auth) = match auth {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    if !translate {
        return serve_untranslated_target(
            &state,
            parsed,
            body,
            target_model,
            &upstream,
            &uri,
            upstream_headers,
            body_model,
            &request_id,
            anthropic_target,
        )
        .await;
    }

    // Zen is free, so offload saves no money there and costs a hidden
    // retrieval round trip each time the model wants a digest back.
    let offload = state.config.ctx_offload_zen
        || classify_upstream(&upstream, is_chatgpt_auth) != UpstreamKind::OpenCodeZen;
    let prepared = match prepare_turn(
        &state,
        parsed,
        &headers,
        &client_addr,
        &request_id,
        identity_model.as_deref(),
        offload,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(resp) => return resp,
    };
    let parsed = prepared.parsed;
    let egress_lane_key = prepared.ctx_report.lane_key.clone();

    let translated = match translate_routed_request(
        &parsed,
        &headers,
        target_model.as_deref(),
        &upstream,
        is_chatgpt_auth,
        body_model,
        &request_id,
    ) {
        Ok(translated) => translated,
        Err(resp) => return resp,
    };
    let openai_body = translated.openai_body;
    let upstream_url = translated.upstream_url;
    let downstream_is_stream = translated.downstream_is_stream;
    // The wire shape, decided once in translation. Arm pick and CCR shape
    // below match on this, never on `target_model`.
    let is_responses = translated.is_responses;

    // Book this turn through the same outcome funnel `forward_http` uses, so
    // routed spend shows up in /stats, /stats-history, and the dashboard
    // alongside Claude traffic.
    //
    // Serialize once: these bytes go upstream below AND feed the token
    // estimate. `to_vec` emits identical compact bytes to the old
    // `to_string` here, and `from_utf8` on them is free, so the estimate
    // is unchanged while a full second serialization is gone.
    let openai_body_vec = match serde_json::to_vec(&openai_body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize OpenAI request"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };
    let forwarded_tokens_estimate = std::str::from_utf8(&openai_body_vec)
        .ok()
        .map(|body| {
            headroom_core::tokenizer::get_tokenizer(target_model.as_deref().unwrap_or(body_model))
                .count_text(body) as i64
        })
        .unwrap_or(0);
    let mut outcome_ctx = build_routed_outcome_context(
        &state,
        &parsed,
        &headers,
        target_model.as_deref(),
        is_responses,
        body_model,
        prepared.ctx_report,
        prepared.overhead_ms,
        request_started,
        request_id.clone(),
        // Only hand the response side a store when this turn was actually
        // parked; `complete` on an unparked id is a no-op, but passing `None`
        // keeps the flag-off path from cloning a handle it will never use.
        prepared.replay_parked.then(|| state.replay_store.clone()),
        forwarded_tokens_estimate,
        openai_body_vec.len() as u64,
    );
    note_routing_attribution(
        &mut outcome_ctx,
        identity_model.as_deref(),
        &headers,
        &openai_body,
        body_model,
        state.config.redact_sensitive,
        &state.redact_store,
    );

    let openai_body_bytes = Bytes::from(openai_body_vec);

    let mut upstream_headers = upstream_headers;

    // Session correlation headers and turn-state echo, mirroring the real
    // Codex client (codex-api/src/requests/headers.rs, client.rs). The
    // provider quirk (P6): only the ChatGPT subscription mutates headers.
    let quirks = classify_upstream(&upstream, is_chatgpt_auth);
    let session_key = quirks.apply_session_headers(&mut upstream_headers, &parsed);

    // Send with retry: refresh the OAuth token once on 401, back off on
    // 429/5xx/transport errors (honoring Retry-After), like the Codex CLI —
    // plus the Zen rate-limit hold, so a 429 outlives the VPN rotation
    // instead of killing the turn.
    let is_zen = matches!(quirks, UpstreamKind::OpenCodeZen);
    let send = send_with_retry(
        &state,
        &upstream_url,
        upstream_headers,
        openai_body_bytes.clone(),
        &request_id,
        session_key.as_deref(),
        Some(&egress_lane_key),
        is_chatgpt_auth,
        is_zen,
    )
    .await;
    let (upstream_resp, upstream_headers, attempt, replay_stripped_bytes, slow_probe) = match send {
        Ok(send) => (
            send.resp,
            send.headers,
            send.attempts,
            send.retried_without_replay,
            send.slow_probe,
        ),
        Err(resp) => return resp,
    };
    note_send_outcome(&mut outcome_ctx, replay_stripped_bytes, attempt);

    quirks.capture_turn_state(&upstream_resp, session_key.as_deref());

    let upstream_status = upstream_resp.status();

    let ccr = RoutedCcr::assemble(
        &state,
        &headers,
        &parsed,
        upstream_url.clone(),
        upstream_headers,
        openai_body_bytes.clone(),
        &request_id,
        is_responses,
        &prepared.redact_session_key,
    )
    .await;

    dispatch_upstream_answer(
        state,
        client_addr,
        method,
        uri,
        headers,
        parsed,
        prepared.redacted,
        &prepared.redact_session_key,
        body_model,
        identity_model,
        upstream_resp,
        upstream_status,
        downstream_is_stream,
        is_responses,
        outcome_ctx,
        ccr,
        slow_probe,
        &request_id,
    )
    .await
}

/// Non-JSON bodies can't be model-routed: rebuild the raw request and
/// delegate to the standard forwarder.
/// Extracted from `handle_messages` without behavior change.
async fn forward_raw_to_forwarder(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let req = builder.body(Body::from(body)).expect("valid request");
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// No route matched: rebuild the raw request and delegate to the standard
/// forwarder. A rebuild failure (not a send failure) is a 500.
/// Extracted from `handle_messages` without behavior change.
async fn delegate_unmatched_to_forwarder(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(hs) = builder.headers_mut() {
        *hs = headers;
    }
    let req = match builder.body(Body::from(body)) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                event = "handler_error",
                handler = "messages_local_model",
                error = %e,
                "failed to reconstruct request"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal handler error"))
                .expect("static response");
        }
    };
    forward_http(state, client_addr, req)
        .await
        .unwrap_or_else(|e| e.into_response())
}

/// Claude Code's spinner-text sidecar is answered here and goes no further:
/// it must not reach route matching, the replay store, or the cache tracker,
/// because the whole point is that it leaves no per-conversation state for
/// the next real turn to be measured against. See `crate::sidecar`.
/// Extracted from `handle_messages` without behavior change.
async fn maybe_answer_sidecar(
    state: &AppState,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    uri: &Uri,
    parsed: &Value,
    request_id: &str,
) -> Option<Response> {
    if crate::sidecar::is_describe_action_sidecar(parsed) {
        return handle_sidecar(state, headers, client_addr, uri, parsed, request_id).await;
    }
    None
}

/// Cursor route: derive the session key and run the Cursor agent CLI.
/// Extracted from `handle_messages` without behavior change.
async fn dispatch_cursor_route(
    state: AppState,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    parsed: &Value,
    body_model: &str,
    cursor_model: &str,
    request_id: &str,
) -> Response {
    let session_key = crate::cache_stabilization::drift_detector::derive_session_key(
        headers,
        client_addr,
        parsed,
        crate::cache_stabilization::drift_detector::ApiKind::Anthropic,
    );
    // TEMPORARY: source port distinguishes a pipelined follow-up
    // (same connection) from a concurrent sender (new
    // connection). Drop once the poll source is identified.
    tracing::info!(
        event = "cursor_route",
        from = %client_addr,
        body_model,
        cursor_model = %cursor_model,
        "routed to cursor agent"
    );
    crate::cursor::handler::handle(state, parsed, &session_key, cursor_model, request_id).await
}

/// Upstream auth for the routed turn: Anthropic-key headers for untranslated
/// Anthropic targets, the shared auth-header builder otherwise. `Err` is the
/// response to return directly.
/// Extracted from `handle_messages` without behavior change.
#[allow(clippy::result_large_err)]
fn resolve_upstream_auth(
    state: &AppState,
    auth_env: Option<&str>,
    headers: &HeaderMap,
    upstream: &url::Url,
    request_id: &str,
    anthropic_target: bool,
) -> Result<(HeaderMap, bool), Response> {
    if anthropic_target {
        crate::routed::auth::anthropic_auth_headers(auth_env, headers, Some(upstream), request_id)
            .map(|headers| (headers, false))
    } else {
        crate::routed::auth::auth_headers(
            auth_env,
            headers,
            state.config.codex_auth_file.as_deref(),
            upstream,
            request_id,
            None,
        )
    }
}

/// Untranslated route (`translate == false`): optionally rewrite the model id
/// in place, then serve straight through the passthrough arm.
/// Extracted from `handle_messages` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn serve_untranslated_target(
    state: &AppState,
    parsed: Value,
    body: Bytes,
    target_model: Option<String>,
    upstream: &url::Url,
    uri: &Uri,
    upstream_headers: HeaderMap,
    body_model: &str,
    request_id: &str,
    anthropic_target: bool,
) -> Response {
    let body = if let Some(target_model) = target_model {
        let mut parsed = parsed;
        parsed["model"] = Value::String(target_model);
        match serde_json::to_vec(&parsed) {
            Ok(body) => Bytes::from(body),
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "serialization error").into_response();
            }
        }
    } else {
        body
    };
    handle_passthrough(
        state,
        upstream,
        uri,
        upstream_headers,
        body,
        body_model,
        request_id,
        anthropic_target,
    )
    .await
}

/// Routing attribution on the outcome context: the reroute origin (so the
/// savings ledger can price what the reroute avoided), the conversation
/// savings key (Responses shape only), and the redaction memory for
/// continuations whenever the flag is on.
/// Extracted from `handle_messages` without behavior change.
#[allow(clippy::too_many_arguments)]
fn note_routing_attribution(
    outcome_ctx: &mut Option<crate::routed::outcome::RoutedOutcomeContext>,
    identity_model: Option<&str>,
    headers: &HeaderMap,
    openai_body: &Value,
    body_model: &str,
    redact_sensitive: bool,
    redact_store: &crate::redact::RedactStore,
) {
    // `build_routed_outcome_context` leaves this `None` because it cannot see
    // the routing decision; this is the handler that made it. Without it the
    // `model_route_served` line never fires and the savings ledger has no way
    // to price what the reroute avoided — the turn books at the free model's
    // rate and the offload looks like it saved nothing.
    if let (Some(ctx), Some(from_model)) = (outcome_ctx.as_mut(), identity_model) {
        ctx.reroute = Some(RerouteOrigin {
            from_model: from_model.to_string(),
            to_model: body_model.to_string(),
        });
    }
    // Novel-vs-repeat savings attribution (upstream `427fa76f`): on a
    // Responses-shape translation the booked per-turn diff is the
    // conversation's running removed-total, so carry the ledger key. Derived
    // from the translated body, whose `input` shape the key rules inspect;
    // identity itself (explicit body ids, session headers) is
    // transform-stable, so deriving post-translation cannot move the key
    // mid-conversation the way content-derived identity could. Chat-shape
    // translations yield `None` by the shape rules and keep per-request
    // accounting, which is already novel-only there.
    if let Some(ctx) = outcome_ctx.as_mut() {
        let session_id = headers
            .get("conversation_id")
            .or_else(|| headers.get("session_id"))
            .or_else(|| headers.get("x-headroom-session-id"))
            .and_then(|v| v.to_str().ok());
        ctx.conversation_key =
            headroom_core::conversation_savings::savings_conversation_key(openai_body, session_id);
    }
    // Hand the response arms the redaction memory whenever the flag is on —
    // not only when the outbound body had spans. A clean prompt can still
    // pull secrets mid-turn (memory answers, cold-tier blocks), and the
    // continuations must redact those too. Empty map snapshots are a
    // passthrough, so clean turns keep the zero-overhead path.
    if redact_sensitive {
        if let Some(ctx) = outcome_ctx.as_mut() {
            ctx.redact_store = Some(redact_store.clone());
        }
    }
}

/// Error-path fallback: the router chose this upstream and the upstream will
/// not serve the turn. The client asked for its own model and is owed an
/// answer on it, so park the target and re-dispatch rather than passing the
/// failure down. Only a turn the router moved can come back this way: when
/// the client named the alias itself there is nothing to fall back to and
/// the error is the honest answer.
/// Extracted from `handle_messages` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn dispatch_model_fallback(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    mut parsed: Value,
    redacted: bool,
    redact_session_key: &str,
    body_model: &str,
    client_model: &str,
    upstream_status: StatusCode,
    request_id: &str,
    outcome_ctx: Option<crate::routed::outcome::RoutedOutcomeContext>,
    upstream_resp: reqwest::Response,
) -> Response {
    let window = state.config.model_router.cooldown();
    let parked = state.model_route_cooldowns.start(body_model, window);
    tracing::warn!(
        event = "model_route_fallback",
        request_id = %request_id,
        routed_model = %body_model,
        client_model = %client_model,
        status = upstream_status.as_u16(),
        cooldown_secs = window.as_secs(),
        parked,
        "routed upstream refused the turn; re-dispatching on the client's model"
    );
    // The failed attempt is deliberately not booked: it never
    // produced a turn, and the fallback dispatch books this request
    // once, under the model the client actually got served on.
    drop(outcome_ctx);
    drop(upstream_resp);
    if redacted {
        // The fallback serves the client's own model, whose client
        // must see real paths — unredact first, or every tool call
        // lands on a placeholder file that does not exist.
        crate::redact::unredact_body(&state.redact_store, redact_session_key, &mut parsed);
        tracing::info!(
            event = "routed_redact_fallback_unredacted",
            request_id = %request_id,
            "fallback serves the client's model on restored text"
        );
    }
    dispatch_route_fallback(
        state,
        client_addr,
        method,
        uri,
        headers,
        parsed,
        client_model,
        request_id,
    )
    .await
}

/// Post-send bookkeeping: the refused byte count on a 413 replay-strip
/// retry, and the attempt count (at least one — the send happened).
/// Extracted from `handle_messages` without behavior change.
fn note_send_outcome(
    outcome_ctx: &mut Option<crate::routed::outcome::RoutedOutcomeContext>,
    replay_stripped_bytes: Option<u64>,
    attempt: u32,
) {
    if let (Some(ctx), Some(stripped)) = (outcome_ctx.as_mut(), replay_stripped_bytes) {
        // The 413 retry below re-sent without the replay prefix: the refused
        // byte count is this smaller body, not the first attempt's.
        ctx.outbound_bytes = stripped;
    }
    if let Some(ctx) = outcome_ctx.as_mut() {
        ctx.upstream_attempts = i64::from(attempt.max(1));
    }
}

/// Fan out on the upstream answer: fallback re-dispatch on a refused routed
/// turn, streaming fold, or buffered fold.
/// Extracted from `handle_messages` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn dispatch_upstream_answer(
    state: AppState,
    client_addr: SocketAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    parsed: Value,
    prepared_redacted: bool,
    prepared_redact_session_key: &str,
    body_model: &str,
    identity_model: Option<String>,
    upstream_resp: reqwest::Response,
    upstream_status: StatusCode,
    downstream_is_stream: bool,
    is_responses: bool,
    outcome_ctx: Option<crate::routed::outcome::RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
    slow_probe: Option<crate::upstream_route_probe::SlowUpstreamProbe>,
    request_id: &str,
) -> Response {
    if upstream_status != StatusCode::OK {
        // The router chose this upstream and the upstream will not serve the
        // turn. The client asked for its own model and is owed an answer on
        // it, so park the target and re-dispatch rather than passing the
        // failure down. Only a turn the router moved can come back this way:
        // when the client named the alias itself there is nothing to fall
        // back to and the error is the honest answer.
        if let Some(client_model) = identity_model.clone() {
            return dispatch_model_fallback(
                state,
                client_addr,
                method,
                uri,
                headers,
                parsed,
                prepared_redacted,
                prepared_redact_session_key,
                body_model,
                &client_model,
                upstream_status,
                request_id,
                outcome_ctx,
                upstream_resp,
            )
            .await;
        }
        handle_routed_error_response(upstream_resp, upstream_status, outcome_ctx).await
    } else if downstream_is_stream {
        handle_streaming_response(
            upstream_resp,
            &parsed,
            state.codex_rate_limits.clone(),
            outcome_ctx,
            ccr,
            slow_probe,
        )
        .await
    } else {
        fold_buffered(
            upstream_resp,
            &parsed,
            upstream_status,
            is_responses,
            outcome_ctx,
            ccr,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{codex_user_agent, derive_session_uuid, generate_traceparent};

    #[test]
    fn codex_user_agent_matches_cli_format() {
        let ua = codex_user_agent(None);
        assert!(ua.starts_with("codex_cli_rs/"));
        assert!(ua.contains('('));
        let tp = generate_traceparent();
        assert_eq!(tp.len(), 55);
        assert!(tp.starts_with("00-"));
        assert!(tp.ends_with("-01"));
    }

    #[test]
    fn derive_session_uuid_is_stable_and_uuid_shaped() {
        let a = derive_session_uuid("user_abc_session_123");
        let b = derive_session_uuid("user_abc_session_123");
        let c = derive_session_uuid("user_abc_session_456");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|&ch| ch == '-').count(), 4);
    }

    /// Upstream `427fa76f` wiring: the translated Responses body carries the
    /// whole transcript under `input`, so the conversation key derives from
    /// it plus the session header; a Chat-shape translation yields no key
    /// (novel-only accounting there, per-request as today).
    #[test]
    fn translated_body_feeds_conversation_key_on_responses_shape_only() {
        use crate::routed::translation::translate_routed_request;

        let parsed = serde_json::json!({
            "model": "claude-muse-spark-1.3",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-headroom-session-id",
            axum::http::HeaderValue::from_static("sess-conv-7"),
        );
        let upstream: url::Url = "https://example.invalid/v1".parse().unwrap();

        let translated = translate_routed_request(
            &parsed,
            &headers,
            Some("muse-spark-1.3"),
            &upstream,
            false,
            "claude-muse-spark-1.3",
            "req-conv-key",
        )
        .expect("translation succeeds");
        assert!(
            translated.openai_body.get("input").is_some(),
            "Responses translation carries the transcript under `input`"
        );
        let session = headers
            .get("x-headroom-session-id")
            .and_then(|v| v.to_str().ok());
        assert!(
            headroom_core::conversation_savings::savings_conversation_key(
                &translated.openai_body,
                session
            )
            .is_some(),
            "session header identifies the conversation"
        );

        // Chat-shape translation: no `input`, no key.
        let translated = translate_routed_request(
            &parsed,
            &headers,
            None,
            &upstream,
            false,
            "claude-muse-spark-1.3",
            "req-conv-key",
        )
        .expect("translation succeeds");
        assert_eq!(
            headroom_core::conversation_savings::savings_conversation_key(
                &translated.openai_body,
                session
            ),
            None
        );
    }

    #[tokio::test]
    async fn fallback_forwards_what_the_client_sent_with_redaction_on() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let Ok(home) = std::env::var("HOME") else {
            eprintln!("SKIP: no HOME in test env");
            return;
        };
        let inner = "read ".to_string() + &home + "/src/main.rs" + " for the deploy key";
        let probe_body = serde_json::to_vec(&serde_json::json!({
            "messages": [{"role": "user", "content": inner.clone()}],
        }))
        .unwrap();
        // Pin the fixture: without a sensitive body this test proves nothing.
        {
            let probe_store = crate::redact::RedactStore::with_key([0xA5; 32]);
            let probe_gate = crate::redact::RedactGate::new(true, &probe_store, "probe");
            let (_, seam) = probe_gate.seam_bytes(bytes::Bytes::from(probe_body));
            assert!(
                seam.is_some(),
                "fixture must be sensitive under ambient HOME"
            );
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "ok"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            })))
            .mount(&server)
            .await;

        let mock_uri: url::Url = server.uri().parse().unwrap();
        let state = crate::test_support::test_state(|c| {
            c.redact_sensitive = true;
            c.upstream = mock_uri.clone();
        });
        let parsed = serde_json::json!({
            "model": "routed-model",
            "messages": [{"role": "user", "content": inner}],
        });
        let uri: axum::http::Uri = server.uri().parse().unwrap();
        let response = dispatch_route_fallback(
            state,
            "127.0.0.1:0".parse().unwrap(),
            axum::http::Method::POST,
            uri,
            axum::http::HeaderMap::new(),
            parsed,
            "client-model",
            "req-fallback-original",
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let received = server.received_requests().await.unwrap_or_default();
        assert!(!received.is_empty(), "fallback must reach upstream");
        let sent: String = received
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect();
        assert!(
            sent.contains(&inner),
            "fallback must forward what the client sent"
        );
        assert!(
            !sent.contains("__HR_"),
            "fallback must not forward placeholders"
        );
    }
}
