//! POST handler for Vertex `:rawPredict` (non-streaming).
//!
//! Path:
//! ```text
//! POST /v1beta1/projects/{project}/locations/{location}/publishers/anthropic/models/{model}:rawPredict
//! ```
//!
//! See [`super`] for the module-level rationale (envelope shape, ADC,
//! routing strategy). This handler:
//!
//! 1. Buffers the request body.
//! 2. Confirms the Vertex envelope shape (`anthropic_version` present,
//!    `model` absent). On envelope mismatch, logs
//!    `event = "vertex_envelope_invalid"` and returns 400.
//! 3. Runs live-zone Anthropic compression — the body is the same
//!    Anthropic Messages shape `/v1/messages` accepts, just with
//!    `anthropic_version` instead of `model`. The dispatcher
//!    preserves `anthropic_version` byte-equal because the
//!    `RawValue`-based surgery only rewrites `messages[*]` entries.
//! 4. Resolves the ADC bearer token (cached, refreshed ahead of
//!    expiry) and attaches `Authorization: Bearer <token>`. On ADC
//!    failure, logs `event = "vertex_adc_fetch_failed"` and returns
//!    502 — never silently forwards unauthenticated.
//! 5. Forwards to the configured Vertex endpoint
//!    (`https://{region}-aiplatform.googleapis.com/<path>`).
//! 6. Streams the response body back unchanged.
//!
//! All decision points emit a structured `tracing::info!` /
//! `tracing::warn!` event so operators can confirm the pipeline is
//! engaged in dashboards.

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use std::net::SocketAddr;

use crate::compression;
use crate::headers::{build_forward_request_headers, filter_response_headers};
use crate::proxy::AppState;
use crate::vertex::{adc::TokenSourceError, envelope, VertexVerb};

/// Carrier struct for the bits parsed out of the URL path; passed
/// down so logs and error paths share a consistent set of fields.
#[derive(Debug, Clone)]
pub(crate) struct VertexCallContext {
    pub project: String,
    pub location: String,
    pub model_id: String,
    pub verb: VertexVerb,
}

/// Shared forwarder used by both `:rawPredict` and `:streamRawPredict`
/// handlers. Streaming-specific behaviour lives in the response-side
/// SSE tee in [`crate::proxy::forward_http`] (which we don't reuse
/// here — Vertex's path is not in `is_compressible_path` and we have
/// our own envelope handling). For PR-D4 the streaming handler just
/// passes through with the same auth + envelope + log surface; the
/// upstream SSE bytes flow back to the client unchanged.
///
/// Note: this function takes 9 arguments. Grouping them into a
/// struct (an obvious clippy fix) would obscure that each argument
/// is a distinct axum extractor / handler-supplied value. The
/// argument list mirrors the catch-all `forward_http` in
/// [`crate::proxy`]; consistency wins over the pedantic lint.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_vertex_request(
    state: AppState,
    client_addr: SocketAddr,
    request_id: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
    ctx: VertexCallContext,
    attach_sse_tee: bool,
) -> Response {
    let path_for_log = uri.path().to_string();

    // ─── 1. BUFFER BODY ────────────────────────────────────────────────
    let buffered = match buffer_vertex_body(&state, body, &request_id, &path_for_log).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    // ─── 2. ENVELOPE PARSE ─────────────────────────────────────────────
    if let Err(resp) = check_vertex_envelope(&buffered, &ctx, &request_id, &path_for_log) {
        return resp;
    }

    // ─── 3. LIVE-ZONE COMPRESSION (when enabled) ───────────────────────
    //
    // Vertex bodies are Anthropic-shape; we feed the same
    // `compress_anthropic_request` dispatcher that runs on /v1/messages.
    // The dispatcher uses RawValue-based surgery so `anthropic_version`
    // (and any other non-`messages` top-level field) round-trips
    // byte-equal. Compression off → buffered bytes used unchanged.
    let body_to_send = compress_vertex_body(&state, buffered, &request_id, &path_for_log);

    // ─── 4. RESOLVE BEARER TOKEN ───────────────────────────────────────
    let bearer = match fetch_vertex_bearer(&state, &ctx, &request_id, &path_for_log).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // ─── 5. BUILD UPSTREAM URL ─────────────────────────────────────────
    //
    // The Vertex endpoint pattern is
    // `https://{region}-aiplatform.googleapis.com/<path-and-query>`.
    // We honour the same `Config::upstream` override pattern the
    // rest of the proxy uses: when an operator sets `upstream` to the
    // mock server (typical in tests), we forward there and the
    // request still carries the canonical Vertex path.
    //
    // For production, the operator should set `upstream` to the
    // regional Vertex host. We do NOT auto-construct the regional
    // URL from `vertex_region` — that would be a hardcoded provider
    // routing decision. The region setting is exposed for
    // observability only.
    let upstream_url = match build_vertex_upstream_url(&state, &uri, &request_id) {
        Ok(u) => u,
        Err(resp) => return resp,
    };

    // ─── 6. BUILD HEADERS ──────────────────────────────────────────────
    let outgoing_headers =
        match build_vertex_headers(&state, &headers, client_addr, &bearer, &request_id) {
            Ok(h) => h,
            Err(resp) => return resp,
        };

    // ─── 6b. REDACTION SEAM ──────────────────────────────────────────
    // Request-id keyed: Vertex has no ApiKind variant for conversation
    // keys (same rule as the gemini/batch seam). The response restores
    // through the seam at the end, streaming or not.
    let gate = crate::redact::RedactGate::new(
        state.config.redact_sensitive,
        &state.redact_store,
        &request_id,
    );
    let (body_to_send, seam) = gate.seam_bytes(body_to_send);

    // ─── 7. FORWARD ────────────────────────────────────────────────────
    let upstream_resp = match send_vertex_request(
        &state,
        method,
        upstream_url,
        outgoing_headers,
        body_to_send,
        &request_id,
        &path_for_log,
    )
    .await
    {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // ─── 8. STREAM RESPONSE ────────────────────────────────────────────
    stream_vertex_response(
        upstream_resp,
        seam,
        request_id,
        path_for_log,
        &ctx,
        attach_sse_tee,
    )
}

/// Buffer the inbound body within the compression limit.
/// Extracted from `forward_vertex_request` without behavior change.
///
// The `Response` error keeps the convention of every sibling arm on the
// routed paths (cf. `routed::translation`); boxing it would save nothing
// measurable and diverge from all of them.
#[allow(clippy::result_large_err)]
async fn buffer_vertex_body(
    state: &AppState,
    body: Body,
    request_id: &str,
    path_for_log: &str,
) -> Result<bytes::Bytes, Response> {
    let max = state.config.compression_max_body_bytes as usize;
    match to_bytes(body, max).await {
        Ok(b) => Ok(b),
        Err(e) => {
            tracing::warn!(
                event = "vertex_body_too_large",
                request_id = %request_id,
                path = %path_for_log,
                limit_bytes = max,
                error = %e,
                "vertex request body exceeds compression buffer limit; failing loudly"
            );
            Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds buffer limit",
            ))
        }
    }
}

/// Reject bodies whose envelope does not match the expected Vertex shape.
/// Extracted from `forward_vertex_request` without behavior change.
///
// See `buffer_vertex_body` for why the `Response` error is not boxed.
#[allow(clippy::result_large_err)]
fn check_vertex_envelope(
    buffered: &[u8],
    ctx: &VertexCallContext,
    request_id: &str,
    path_for_log: &str,
) -> Result<(), Response> {
    match envelope::parse(buffered) {
        Ok(env) => {
            tracing::info!(
                event = "vertex_envelope_parsed",
                request_id = %request_id,
                path = %path_for_log,
                project = %ctx.project,
                location = %ctx.location,
                model = %ctx.model_id,
                verb = ctx.verb.as_str(),
                anthropic_version = %env.anthropic_version,
                has_messages = env.has_messages,
                "vertex envelope detected"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                event = "vertex_envelope_invalid",
                request_id = %request_id,
                path = %path_for_log,
                model = %ctx.model_id,
                verb = ctx.verb.as_str(),
                error = %e,
                "vertex envelope did not match expected shape; rejecting with 400"
            );
            Err(error_response(
                StatusCode::BAD_REQUEST,
                "vertex envelope invalid",
            ))
        }
    }
}

/// Run live-zone compression when enabled, else pass the bytes through.
/// Vertex uses GCP ADC bearer-token auth downstream, not Anthropic
/// credentials, so the PAYG/OAuth/subscription classification doesn't apply:
/// hard-code `AuthMode::OAuth` (PR-E3).
/// Extracted from `forward_vertex_request` without behavior change.
fn compress_vertex_body(
    state: &AppState,
    buffered: bytes::Bytes,
    request_id: &str,
    path_for_log: &str,
) -> bytes::Bytes {
    if !state.config.compression {
        tracing::info!(
            event = "vertex_compression_skipped",
            request_id = %request_id,
            path = %path_for_log,
            reason = "compression_off",
            "compression master switch off; vertex body forwarded unchanged"
        );
        return buffered;
    }
    // PR-E3: Vertex uses GCP ADC bearer-token auth downstream, not
    // Anthropic credentials, so the PAYG/OAuth/subscription
    // classification doesn't apply. Hard-code `AuthMode::OAuth` to
    // skip E3 cache_control auto-placement (and any other PAYG-only
    // mutation). Live-zone compression itself continues to run.
    let outcome = compression::compress_anthropic_request(
        &buffered,
        state.config.compression_mode,
        state.config.cache_control_auto_frozen,
        headroom_core::auth_mode::AuthMode::OAuth,
        request_id,
        &state.config.exclude_tools,
        // No CCR store: this path never injects `headroom_retrieve`, so a
        // marker here would advertise a recovery route the model cannot
        // take.
        None,
    );
    // Cross-turn verbatim de-dup post-pass (no-op unless
    // `--enable-cross-turn-dedup` is set).
    let outcome = compression::apply_cross_turn_dedup(
        outcome,
        &buffered,
        &state.config,
        "/vertex/rawPredict",
        request_id,
    );
    report_compression_outcome(outcome, buffered, state, request_id, path_for_log)
}

/// Log the live-zone outcome and select the bytes to forward.
/// Extracted from `compress_vertex_body` without behavior change.
fn report_compression_outcome(
    outcome: compression::Outcome,
    buffered: bytes::Bytes,
    state: &AppState,
    request_id: &str,
    path_for_log: &str,
) -> bytes::Bytes {
    match outcome {
        compression::Outcome::NoCompression => {
            tracing::info!(
                event = "vertex_compression_skipped",
                request_id = %request_id,
                path = %path_for_log,
                compression_mode = state.config.compression_mode.as_str(),
                reason = "no_compression",
                "vertex live-zone dispatcher returned NoCompression"
            );
            buffered
        }
        compression::Outcome::Compressed {
            body,
            tokens_before,
            tokens_after,
            strategies_applied,
            markers_inserted,
            ..
        } => {
            tracing::info!(
                event = "vertex_compression_applied",
                request_id = %request_id,
                path = %path_for_log,
                tokens_before = tokens_before,
                tokens_after = tokens_after,
                tokens_freed = tokens_before.saturating_sub(tokens_after),
                strategies = ?strategies_applied,
                markers = markers_inserted.len(),
                "vertex live-zone compression applied"
            );
            body
        }
        compression::Outcome::Passthrough { reason } => {
            tracing::warn!(
                event = "vertex_compression_passthrough",
                request_id = %request_id,
                path = %path_for_log,
                reason = ?reason,
                "vertex live-zone dispatcher passthrough on parse/serialize"
            );
            buffered
        }
    }
}

/// Fetch the GCP ADC bearer token. Per project rule "no silent fallbacks":
/// never forward unauthenticated — surface the failure as a structured 502.
/// Extracted from `forward_vertex_request` without behavior change.
///
// See `buffer_vertex_body` for why the `Response` error is not boxed.
#[allow(clippy::result_large_err)]
async fn fetch_vertex_bearer(
    state: &AppState,
    ctx: &VertexCallContext,
    request_id: &str,
    path_for_log: &str,
) -> Result<String, Response> {
    match state.vertex_token_source.bearer().await {
        Ok(t) => Ok(t),
        Err(e) => {
            // Per project rule "no silent fallbacks": never forward
            // unauthenticated. Surface the failure as a structured
            // 502 so operators see the cause clearly in logs.
            tracing::error!(
                event = "vertex_adc_fetch_failed",
                request_id = %request_id,
                path = %path_for_log,
                model = %ctx.model_id,
                verb = ctx.verb.as_str(),
                error = %e,
                "vertex ADC bearer token fetch failed; refusing to forward unauthenticated"
            );
            let status = match e {
                TokenSourceError::ProviderInit(_) => StatusCode::BAD_GATEWAY,
                TokenSourceError::Fetch(_) => StatusCode::BAD_GATEWAY,
            };
            Err(error_response(status, "vertex ADC token fetch failed"))
        }
    }
}

/// Build the upstream URL, honouring the `Config::upstream` override pattern
/// (a mock server in tests still carries the canonical Vertex path).
/// Extracted from `forward_vertex_request` without behavior change.
///
// See `buffer_vertex_body` for why the `Response` error is not boxed.
#[allow(clippy::result_large_err)]
fn build_vertex_upstream_url(
    state: &AppState,
    uri: &Uri,
    request_id: &str,
) -> Result<url::Url, Response> {
    match crate::proxy::build_upstream_url(&state.config.upstream, uri) {
        Ok(u) => Ok(u),
        Err(e) => {
            tracing::error!(
                event = "vertex_upstream_url_failed",
                request_id = %request_id,
                error = %e,
                "could not construct vertex upstream URL"
            );
            Err(error_response(
                StatusCode::BAD_GATEWAY,
                "vertex upstream URL build failed",
            ))
        }
    }
}

/// Build the outbound headers: forwarded host/proto handling plus the ADC
/// bearer (which replaces any client-sent Authorization — Vertex rejects
/// the wrong Auth flavour, so keeping it would silently break the call).
/// Extracted from `forward_vertex_request` without behavior change.
///
// See `buffer_vertex_body` for why the `Response` error is not boxed.
#[allow(clippy::result_large_err)]
fn build_vertex_headers(
    state: &AppState,
    headers: &HeaderMap,
    client_addr: SocketAddr,
    bearer: &str,
    request_id: &str,
) -> Result<HeaderMap, Response> {
    let strip_internal = state.config.strip_internal_headers.is_enabled();
    let forwarded_host = headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // Honour the scheme set by a TLS-terminating upstream (e.g. a load
    // balancer that sets X-Forwarded-Proto: https). Fall back to "http"
    // for plain connections that carry no such header.
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    // PR-F4 (P5-53): Vertex is always ADC/OAuth downstream (see the
    // compression call above), and OAuth keeps X-Forwarded-* per spec.
    let mut outgoing_headers = build_forward_request_headers(
        headers,
        client_addr.ip(),
        forwarded_proto,
        forwarded_host.as_deref(),
        request_id,
        strip_internal,
        headroom_core::auth_mode::AuthMode::OAuth,
    );
    if !state.config.rewrite_host {
        if let Some(h) = headers.get(http::header::HOST) {
            outgoing_headers.insert(http::header::HOST, h.clone());
        }
    }
    // Attach the bearer; if the client already sent an Authorization
    // header we replace it (Vertex rejects the wrong Auth flavour
    // anyway, so keeping the client-provided value would silently
    // break the call).
    match http::HeaderValue::from_str(&format!("Bearer {bearer}")) {
        Ok(v) => {
            outgoing_headers.insert(http::header::AUTHORIZATION, v);
            Ok(outgoing_headers)
        }
        Err(e) => {
            tracing::error!(
                event = "vertex_authorization_invalid",
                request_id = %request_id,
                error = %e,
                "ADC bearer token contained invalid header bytes; refusing to forward"
            );
            Err(error_response(
                StatusCode::BAD_GATEWAY,
                "vertex auth header build failed",
            ))
        }
    }
}

/// Send the prepared request upstream.
/// Extracted from `forward_vertex_request` without behavior change.
///
// See `buffer_vertex_body` for why the `Response` error is not boxed.
#[allow(clippy::result_large_err)]
async fn send_vertex_request(
    state: &AppState,
    method: Method,
    upstream_url: url::Url,
    headers: HeaderMap,
    body: bytes::Bytes,
    request_id: &str,
    path_for_log: &str,
) -> Result<reqwest::Response, Response> {
    let reqwest_method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(
                event = "vertex_method_invalid",
                request_id = %request_id,
                method = %method,
                error = %e,
                "could not convert axum method to reqwest method"
            );
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "vertex method invalid",
            ));
        }
    };
    match state
        .client
        .request(reqwest_method, upstream_url.clone())
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(r) => Ok(r),
        Err(e) => {
            tracing::warn!(
                event = "vertex_upstream_error",
                request_id = %request_id,
                path = %path_for_log,
                error = %e,
                "vertex upstream call failed"
            );
            Err(error_response(
                StatusCode::BAD_GATEWAY,
                "vertex upstream error",
            ))
        }
    }
}

/// Stream the upstream response back, teeing SSE bytes for telemetry when
/// asked, then restoring redaction placeholders on the way out.
/// Extracted from `forward_vertex_request` without behavior change.
fn stream_vertex_response(
    upstream_resp: reqwest::Response,
    seam: Option<crate::redact::Seam>,
    request_id: String,
    path_for_log: String,
    ctx: &VertexCallContext,
    attach_sse_tee: bool,
) -> Response {
    let upstream_status = upstream_resp.status();
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = filter_response_headers(upstream_resp.headers());

    // PR-C1 reuse: when `attach_sse_tee` is set AND the upstream
    // response is `text/event-stream`, tee bytes into a bounded mpsc
    // and drive `AnthropicStreamState` in a spawned task — same shape
    // as the `/v1/messages` SSE telemetry tee in
    // `crate::proxy::forward_http`. The byte-passthrough path is
    // unaffected by the tee (best-effort `try_send`, bounded channel).
    let parser_tx = engage_sse_tee(attach_sse_tee, &upstream_resp, &request_id);

    use futures_util::StreamExt as _;
    let rid_for_stream = request_id.clone();
    let resp_stream = upstream_resp.bytes_stream().map(move |r| match r {
        Ok(b) => {
            if let Some(tx) = &parser_tx {
                if let Err(e) = tx.try_send(b.clone()) {
                    tracing::debug!(
                        request_id = %rid_for_stream,
                        error = %e,
                        "vertex sse parser queue full or closed; skipping telemetry chunk"
                    );
                }
            }
            Ok(b)
        }
        Err(e) => {
            tracing::warn!(
                request_id = %rid_for_stream,
                error = %e,
                cause = ?e,
                "vertex upstream stream error mid-response"
            );
            Err(e)
        }
    });
    let body = Body::from_stream(crate::proxy::track_streaming(resp_stream));

    let mut response = Response::builder().status(status);
    if let Some(h) = response.headers_mut() {
        h.extend(resp_headers);
        if let Ok(v) = http::HeaderValue::from_str(&request_id) {
            h.insert(http::HeaderName::from_static("x-request-id"), v);
        }
    }
    let response = match response.body(body) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                event = "vertex_response_build_failed",
                request_id = %request_id,
                error = %e,
                "could not build vertex response"
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "response build failed");
        }
    };

    tracing::info!(
        event = "vertex_forwarded",
        request_id = %request_id,
        path = %path_for_log,
        project = %ctx.project,
        location = %ctx.location,
        model = %ctx.model_id,
        verb = ctx.verb.as_str(),
        upstream_status = upstream_status.as_u16(),
        "vertex request forwarded"
    );

    crate::redact::restore_response(seam, response)
}

fn error_response(status: StatusCode, msg: &'static str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(msg))
        .expect("static error response")
}

/// Bound on the in-flight queue between the byte-passthrough and the
/// SSE state-machine task. Mirrors the
/// `crate::proxy::SSE_PARSER_QUEUE_DEPTH` rationale (256 events ≈ 5
/// seconds of typical Anthropic streaming under the per-100ms event
/// rate; keeps memory bounded even if the parser stalls).
const VERTEX_SSE_QUEUE_DEPTH: usize = 256;

/// Engage the SSE telemetry tee when asked and the upstream response is
/// `text/event-stream`: bounded channel, spawned state machine, best-effort
/// sends that never block the byte path.
/// Extracted from `stream_vertex_response` without behavior change.
fn engage_sse_tee(
    attach_sse_tee: bool,
    upstream_resp: &reqwest::Response,
    request_id: &str,
) -> Option<tokio::sync::mpsc::Sender<bytes::Bytes>> {
    let is_sse = upstream_resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let media = s.split(';').next().unwrap_or("").trim();
            media.eq_ignore_ascii_case("text/event-stream")
        })
        .unwrap_or(false);
    if !(attach_sse_tee && is_sse) {
        return None;
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(VERTEX_SSE_QUEUE_DEPTH);
    let rid = request_id.to_string();
    tokio::spawn(run_anthropic_sse_state_machine(rx, rid));
    tracing::info!(
        event = "vertex_sse_tee_engaged",
        request_id = %request_id,
        "vertex stream_raw_predict SSE telemetry tee engaged"
    );
    Some(tx)
}

/// Drive the Anthropic SSE state machine over a stream of byte
/// chunks. Lives in its own spawned task; the byte path is fed via a
/// best-effort tee from [`forward_vertex_request`] and never blocks
/// on this loop.
async fn run_anthropic_sse_state_machine(
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    request_id: String,
) {
    use crate::sse::framing::SseFramer;
    let mut framer = SseFramer::new();
    let mut state = crate::sse::anthropic::AnthropicStreamState::new();
    while let Some(chunk) = rx.recv().await {
        framer.push(&chunk);
        while let Some(ev_result) = framer.next_event() {
            apply_framed_event(&mut state, ev_result, &request_id);
        }
    }
    tracing::info!(
        event = "vertex_sse_stream_closed",
        request_id = %request_id,
        provider = "vertex_anthropic",
        input_tokens = state.usage.input_tokens,
        output_tokens = state.usage.output_tokens,
        cache_creation_input_tokens = state.usage.cache_creation_input_tokens,
        cache_read_input_tokens = state.usage.cache_read_input_tokens,
        stop_reason = state.stop_reason.as_deref().unwrap_or(""),
        blocks = state.blocks.len(),
        "vertex sse stream closed"
    );
}

/// Apply one framed SSE event to the telemetry state machine.
/// Extracted from `run_anthropic_sse_state_machine` without behavior change.
fn apply_framed_event(
    state: &mut crate::sse::anthropic::AnthropicStreamState,
    ev_result: Result<crate::sse::framing::SseEvent, crate::sse::framing::FramingError>,
    request_id: &str,
) {
    match ev_result {
        Ok(ev) => {
            if let Err(e) = state.apply(ev) {
                tracing::warn!(
                    request_id = %request_id,
                    error = %e,
                    "vertex sse anthropic state-machine apply error"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                request_id = %request_id,
                error = %e,
                "vertex sse framer error"
            );
        }
    }
}
