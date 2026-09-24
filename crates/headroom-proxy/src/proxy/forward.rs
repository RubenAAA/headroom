//! Buffered-send phase of [`super::forward_http`].
//!
//! Pure module move of the intercept arm's upstream-send tail: retry-budget
//! setup plus the attempt loop (transient-status retry with `Retry-After`
//! cap, leading in-band SSE-error retry, transport-error retry). No logic
//! changes — every block is verbatim motion.
//!
//! `use super::*` keeps the parent's private items (`OutcomeContext`,
//! `backoff_ms`, `peek_leading_sse_error`, `is_retryable_transport_error`,
//! …) reachable with zero visibility churn elsewhere in the crate.

use super::*;

/// Reads the full request body into memory, capped at `max` bytes.
///
/// Buffer up to the compression limit. Rejects oversize bodies loudly with
/// `PayloadTooLarge`: once the body is partially consumed streaming cannot
/// resume. Extracted from `forward_http` without behavior change.
pub(crate) async fn read_buffered_body(
    req: Request<Body>,
    max: usize,
    body_bytes_hint: Option<u64>,
    request_id: &str,
    path_for_log: &str,
) -> Result<bytes::Bytes, ProxyError> {
    if let Some(len) = body_bytes_hint {
        if len as usize > max {
            tracing::warn!(
                event = "forward_body_too_large",
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
    // Manual frame loop instead of `to_bytes`: the Content-Length
    // hint above sizes the accumulator up front (no regrows on
    // large bodies) and `freeze()` hands the bytes over with no
    // final memcpy (`Collected::to_bytes` copies once). Byte
    // contract matches `to_bytes(req.into_body(), max)` exactly:
    // data frames concatenated in order, trailers ignored, and any
    // error (including the `max` limit) falls into the same
    // `PayloadTooLarge` arm below.
    let buffered = {
        let mut acc = bytes::BytesMut::with_capacity(
            body_bytes_hint.map(|n| (n as usize).min(max)).unwrap_or(0),
        );
        let mut limited = http_body_util::Limited::new(req.into_body(), max);
        let result: Result<bytes::Bytes, String> = async {
            while let Some(frame) = http_body_util::BodyExt::frame(&mut limited)
                .await
                .transpose()
                .map_err(|e| e.to_string())?
            {
                if let Ok(data) = frame.into_data() {
                    acc.extend_from_slice(&data);
                }
            }
            Ok(acc.freeze())
        }
        .await;
        match result {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    event = "forward_body_over_limit_fatal",
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
        }
    };
    Ok(buffered)
}

/// Sends the buffered request body upstream, retrying transient failures.
///
/// Covers 429/529/5xx with `Retry-After` cap and backoff, a leading in-band
/// SSE error inside a 200 body on its own longer budget, and retryable
/// transport errors. Records the attempts made on `outcome_ctx`.
/// Extracted from `forward_http` without behavior change.
pub(crate) async fn send_buffered_with_retry(
    state: &AppState,
    request_id: &str,
    request_session_key: &str,
    reqwest_method: &reqwest::Method,
    upstream: UpstreamCall<'_>,
    body_to_send: &bytes::Bytes,
    outcome_ctx: &mut Option<OutcomeContext>,
) -> Result<(reqwest::Response, bytes::Bytes), ProxyError> {
    let UpstreamCall {
        client: upstream_client,
        url: upstream_url,
        headers: send_headers,
    } = upstream;
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
            let resp = upstream_client
                .request(reqwest_method.clone(), upstream_url.clone())
                .headers(send_headers.clone())
                .body(body_to_send.clone())
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    if maybe_retry_status(
                        state,
                        request_id,
                        request_session_key,
                        &r,
                        status,
                        attempt,
                        max_attempts,
                    )
                    .await
                    {
                        continue;
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
                        if maybe_retry_leading_error(
                            state,
                            request_id,
                            kind,
                            attempt,
                            overload_max_attempts,
                        )
                        .await
                        {
                            continue;
                        }
                    }
                    result = Some((r, prefix));
                    break;
                }
                Err(e) => {
                    let (retry, err) =
                        handle_transport_error(state, request_id, e, attempt, max_attempts).await;
                    if retry {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        let resolved = match result {
            Some(res) => res,
            None => {
                let err = last_err.unwrap_or_else(|| {
                    ProxyError::InvalidUpstream("retry loop exhausted".to_string())
                });
                // Terminal line for the retry loop: the per-attempt
                // "retrying" lines carry this id, but the `proxy error`
                // line at the response boundary does not, which left a
                // connection flap unlinkable to its requests.
                tracing::warn!(
                    event = "upstream_retry_exhausted",
                    request_id = %request_id,
                    attempts = attempts_made.max(1),
                    error = %err,
                    "upstream retry loop exhausted; failing the turn"
                );
                return Err(err);
            }
        };
        if let Some(ctx) = outcome_ctx.as_mut() {
            ctx.upstream_attempts = attempts_made.max(1);
        }
        Ok(resolved)
    }
}

/// Retryable-status arm of the send loop: sleep and report `true` when the
/// caller should `continue` with another attempt, report `false` when it
/// should fall through to the leading-error peek (not retryable, budget
/// exhausted, or `Retry-After` over the cap — the cap case still warns).
/// Extracted from `send_buffered_with_retry` without behavior change.
async fn maybe_retry_status(
    state: &AppState,
    request_id: &str,
    request_session_key: &str,
    r: &reqwest::Response,
    status: u16,
    attempt: u32,
    max_attempts: u32,
) -> bool {
    // 429 = rate-limited, 529 = Anthropic overloaded, 5xx = server error
    let is_retryable = status == 429 || status == 529 || (500..600).contains(&status);
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
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(request_session_key),
                "upstream Retry-After exceeds the internal wait cap; returning the response without an early retry"
            );
        } else {
            let retry_after =
                retry_after_uncapped.map(|delay| delay.ceil().min(u64::MAX as f64) as u64);
            let delay_source = if retry_after.is_some() {
                "header"
            } else {
                "backoff"
            };
            // Shared selection (C5): header wins, else the
            // backoff below. No outer clamp here — unlike
            // the routed loop, this path sleeps the raw
            // value, so it stays as written.
            let delay_ms = headroom_core::retry::next_delay_ms(
                retry_after_uncapped,
                backoff_ms(state, attempt),
            );
            tracing::warn!(
                event = "upstream_retryable_status",
                request_id = %request_id,
                status = status,
                attempt = attempt + 1,
                max_attempts = max_attempts,
                delay_ms = delay_ms,
                retry_after_header,
                delay_source,
                retry_after_clamped = false,
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(request_session_key),
                "upstream returned retryable status; retrying"
            );
            crate::observability::record_upstream_retry(
                "anthropic",
                crate::observability::retry_reason::from_status(status),
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            return true;
        }
    }
    false
}

/// Leading in-band SSE-error arm of the send loop: sleep and report `true`
/// when the caller should `continue`, report `false` when the budget is
/// spent (caller breaks with the errored response after the spent warn).
/// Extracted from `send_buffered_with_retry` without behavior change.
async fn maybe_retry_leading_error(
    state: &AppState,
    request_id: &str,
    kind: &str,
    attempt: u32,
    overload_max_attempts: u32,
) -> bool {
    if attempt + 1 < overload_max_attempts {
        let delay_ms = backoff_ms(state, attempt);
        tracing::warn!(
            event = "upstream_200_stream_error",
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
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        return true;
    }
    tracing::warn!(
        event = "upstream_200_error_exhausted",
        request_id = %request_id,
        error_type = %kind,
        attempts = overload_max_attempts,
        "upstream error inside a 200 stream survived every retry"
    );
    crate::observability::record_upstream_retry_exhausted(
        "anthropic",
        crate::observability::retry_reason::IN_BAND_SSE,
    );
    false
}

/// Transport-error arm of the send loop: returns `(retry, err)` where
/// `retry` tells the caller to store `err` as `last_err` and `continue`
/// versus returning `Err(err)` immediately.
/// Extracted from `send_buffered_with_retry` without behavior change.
async fn handle_transport_error(
    state: &AppState,
    request_id: &str,
    e: reqwest::Error,
    attempt: u32,
    max_attempts: u32,
) -> (bool, ProxyError) {
    let is_retryable = is_retryable_transport_error(&e);
    if is_retryable && attempt + 1 < max_attempts {
        let delay_ms = backoff_ms(state, attempt);
        tracing::warn!(
            event = "upstream_transport_retrying",
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
        return (true, ProxyError::Upstream(e));
    }
    if is_retryable {
        crate::observability::record_upstream_retry_exhausted(
            "anthropic",
            crate::observability::retry_reason::TRANSPORT,
        );
    }
    (false, ProxyError::Upstream(e))
}

/// Applies the endpoint-specific buffered body transforms.
///
/// Sanitizes the Anthropic model id, pins the billing header, and flips a
/// streaming `/v1/responses` turn with `headroom_retrieve` to a buffered
/// upstream call. Sets `buffered_responses_ccr` on a flip. Extracted from
/// `forward_http` without behavior change.
pub(crate) fn apply_buffered_body_transforms(
    buffered: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    headers_snapshot: &Option<HeaderMap>,
    ccr_handle_responses: bool,
    request_id: &str,
    buffered_responses_ccr: &mut bool,
    upstream_base: &url::Url,
) -> bytes::Bytes {
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
    let mut buffered = match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => {
            cache_stabilization::billing_header::pin_billing_header_in_body(buffered)
        }
        compression::CompressibleEndpoint::OpenAiChatCompletions
        | compression::CompressibleEndpoint::OpenAiResponses => buffered,
    };

    // Buffered CCR: a streaming `/v1/responses` turn offering
    // `headroom_retrieve` goes upstream buffered (`stream: false`)
    // so the buffered arm downstream can resolve retrieval
    // server-side and resynthesize SSE for the client.
    // ChatGPT-OAuth sessions stay streaming — their server-side
    // session owns the transcript. Runs before compression so the
    // flipped body is what every downstream stage (fingerprint,
    // replay store, compressor, wire bytes) sees.
    if matches!(endpoint, compression::CompressibleEndpoint::OpenAiResponses)
        && ccr_handle_responses
    {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&buffered) {
            let stream = parsed
                .get("stream")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let is_chatgpt = headers_snapshot
                .as_ref()
                .is_some_and(crate::openai_buffered_ccr::caller_is_chatgpt_auth);
            if crate::openai_buffered_ccr::should_buffer_openai_responses_stream_ccr(
                stream,
                true,
                parsed.get("tools"),
                is_chatgpt,
                crate::openai_buffered_ccr::is_opencode_zen_base(upstream_base),
            ) {
                let mut flipped = parsed.clone();
                flipped["stream"] = serde_json::Value::Bool(false);
                if let Ok(rewritten) = serde_json::to_vec(&flipped) {
                    buffered = bytes::Bytes::from(rewritten);
                    *buffered_responses_ccr = true;
                    tracing::info!(
                        request_id = %request_id,
                        event = "buffered_responses_ccr_flip",
                        "streaming /v1/responses with headroom_retrieve flipped to buffered upstream call",
                    );
                }
            }
        }
    }
    buffered
}

/// Builds the headers for the buffered upstream send and runs the stampede gate.
///
/// Strips any surviving client `content-encoding`, asks for JSON on a flipped
/// buffered-CCR turn, and admits same-head followers through the stampede
/// gate. Records `stampede_wait`. Extracted from `forward_http` without
/// behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_buffered_send(
    state: &AppState,
    request_id: &str,
    endpoint: compression::CompressibleEndpoint,
    outgoing_headers: &HeaderMap,
    body_to_send: &bytes::Bytes,
    buffered_responses_ccr: bool,
    stage_timer: &mut crate::stage_timer::StageTimer,
    stampede: &mut Option<(
        String,
        Option<cache_stabilization::prefix_stampede::LeaderToken>,
    )>,
) -> HeaderMap {
    // A surviving client `content-encoding` with re-serialized JSON
    // makes the upstream gunzip plaintext. Strip it here (buffered
    // branch only — the streaming passthrough below keeps headers
    // byte-faithful, and opaque non-JSON bytes keep the header inside
    // the helper).
    let mut send_headers = crate::headers::headers_for_json_body(outgoing_headers, body_to_send);
    // Buffered CCR flipped this `/v1/responses` turn to a non-streaming
    // upstream call, but the client's `Accept: text/event-stream`
    // survives the clone above. Ask for JSON: the buffered arm below
    // resynthesizes SSE for the client from the JSON body.
    if buffered_responses_ccr {
        send_headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
    }

    // Same-head stampede gate. Runs last, on the bytes that go out, so the
    // key matches what the provider will cache. Its wait is recorded as
    // its own stage: it is proxy time, but chosen, not spent.
    if state.config.cache_stampede_gate
        && endpoint == compression::CompressibleEndpoint::AnthropicMessages
    {
        let key = serde_json::from_slice::<serde_json::Value>(body_to_send)
            .ok()
            .as_ref()
            .and_then(cache_stabilization::prefix_stampede::PrefixStampedeGate::head_key);
        if let Some(key) = key {
            use cache_stabilization::prefix_stampede::Admission;
            let gate_start = Instant::now();
            let token = match state.stampede_gate.admit(&key).await {
                Admission::Leader(token) => Some(token),
                Admission::Warm => None,
                Admission::Follower { waited, release } => {
                    tracing::info!(
                        event = "stampede_follower_released",
                        request_id = %request_id,
                        head_key = %key,
                        waited_ms = waited.as_millis() as u64,
                        release = release.as_str(),
                        "held behind a same-head leader"
                    );
                    None
                }
            };
            stage_timer.record("stampede_wait", gate_start.elapsed().as_secs_f64() * 1000.0);
            *stampede = Some((key, token));
        }
    }
    send_headers
}

/// Observability side effects at first upstream byte.
///
/// Settles the stampede gate (leader `first_byte` / follower `touch`), records
/// the `upstream` stage timing, captures upstream request ids, classifies the
/// SSE kind, filters response headers, and records rate-limit observations.
/// Returns `(status, resp_headers, is_sse, sse_kind, request ids)`.
/// Extracted from `forward_http` without behavior change.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
pub(crate) fn observe_upstream_head(
    upstream_resp: &reqwest::Response,
    stampede: &mut Option<(
        String,
        Option<cache_stabilization::prefix_stampede::LeaderToken>,
    )>,
    stage_timer: &mut crate::stage_timer::StageTimer,
    start: &Instant,
    state: &AppState,
    path_for_log: &str,
    request_id: &str,
    enable_responses_streaming: bool,
    configured_http_proxy: bool,
) -> (
    StatusCode,
    HeaderMap,
    bool,
    SseStreamKind,
    (Option<String>, Option<String>, Option<String>),
) {
    if let Some((key, token)) = stampede.take() {
        if upstream_resp.status().is_success() {
            match token {
                Some(token) => token.first_byte(),
                None => state.stampede_gate.touch(&key),
            }
        }
        // A failed leader drops its token here and releases its followers.
    }
    // Response headers are in hand. Whatever is left after `pre_forward` is
    // the provider's own time, including retries and backoff.
    let pre_forward = stage_timer
        .summary()
        .get("pre_forward")
        .copied()
        .unwrap_or(0.0);
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let upstream_wait_ms = (elapsed - pre_forward).max(0.0);
    stage_timer.record("upstream", upstream_wait_ms);

    let upstream_status = upstream_resp.status();

    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    tracing::info!(
        target: "headroom.proxy",
        event = "upstream_response_ready",
        request_id = %request_id,
        path = %path_for_log,
        upstream_wait_ms,
        configured_http_proxy,
        upstream_peer = ?upstream_resp.remote_addr(),
        upstream_http_version = ?upstream_resp.version(),
        upstream_status = upstream_status.as_u16(),
        "upstream response became available"
    );

    let (upstream_request_id_anthropic, upstream_request_id_openai, upstream_request_id) =
        capture_upstream_request_ids(upstream_resp.headers());

    let is_sse = is_sse_response(upstream_resp.headers());
    let sse_kind = classify_sse_kind(is_sse, path_for_log, enable_responses_streaming, request_id);

    let resp_headers = filter_response_headers(upstream_resp.headers());

    record_upstream_rate_limits(
        upstream_resp.headers(),
        upstream_request_id_anthropic.is_some(),
        upstream_request_id_openai.is_some(),
        path_for_log,
        request_id,
    );
    (
        status,
        resp_headers,
        is_sse,
        sse_kind,
        (
            upstream_request_id_anthropic,
            upstream_request_id_openai,
            upstream_request_id,
        ),
    )
}

/// Anthropic-arm rewrite: model routing, id sanitize, prune, deferral.
///
/// Runs after the endpoint dispatch inside `run_rewrite_pipeline`'s Anthropic
/// branch. Reads the routed model off `outcome_ctx`, writes tool-search
/// attribution back. Extracted from `forward_http` without behavior change.
pub(crate) fn rewrite_anthropic_body(
    body_to_send: bytes::Bytes,
    state: &AppState,
    request_id: &str,
    selected_upstream_base: &str,
    skip_model_routing: bool,
    outcome_ctx: &mut Option<OutcomeContext>,
) -> bytes::Bytes {
    // Cost-aware model routing (#1706). Runs before sanitisation so
    // a routed id is cleaned too, and before the upstream is chosen
    // so `config.model_routes` sees the model actually being sent.
    // No-op unless the operator configured routes.
    //
    // Skipped entirely for a turn re-dispatched after its routed
    // upstream failed, and rules whose target is in cooldown are
    // passed over — see [`SkipModelRouting`].
    let body_to_send = if skip_model_routing {
        body_to_send
    } else {
        crate::model_router::apply_to_anthropic_body_with_cooldowns(
            body_to_send,
            &crate::model_router::ModelRouter::new(Some(state.config.model_router.clone())),
            request_id,
            Some(&state.model_route_cooldowns),
        )
    };
    // Strip terminal styling artifacts (e.g. a dangling
    // `[1m]` suffix) from `body["model"]` before forwarding;
    // Anthropic-compatible upstreams reject the decorated id.
    let body_to_send = crate::model_sanitize::sanitize_anthropic_model_in_body(body_to_send);
    let body_to_send = if state.config.tool_prune_policy.is_noop() {
        body_to_send
    } else {
        maybe_prune_tools(body_to_send, &state.config.tool_prune_policy, request_id)
    };
    // Server-side tool-search deferral (+ third-party strip).
    // After pruning (settles the tool set); compaction and the
    // cache-control stages below then see the final array.
    let body_to_send = {
        let model = outcome_ctx
            .as_ref()
            .map(|ctx| ctx.model.as_str())
            .unwrap_or("");
        let (bytes, attribution) = maybe_inject_tool_search(
            body_to_send,
            selected_upstream_base,
            model,
            request_id,
            crate::tool_search_deferral::tool_search_enabled(),
        );
        if let Some(attr) = attribution {
            if let Some(ctx) = outcome_ctx.as_mut() {
                // Always tag the mode: a client stand-down must not read as
                // the feature being off, and "none" must not read as "headroom".
                ctx.tags
                    .insert("tool_search_mode".to_string(), attr.mode.to_string());
                if attr.deferred_tools > 0 {
                    ctx.tags.insert(
                        "tool_search_deferred_tools".to_string(),
                        attr.deferred_tools.to_string(),
                    );
                    ctx.tags.insert(
                        "tool_search_deferred_tokens".to_string(),
                        attr.deferred_tokens.to_string(),
                    );
                    // Disjoint slice of `tool_search_deferred_tokens`, tagged
                    // only when nonzero; the experiment dashboard divides the
                    // two instead of summing them.
                    if attr.core_deferred_tokens > 0 {
                        ctx.tags.insert(
                            "core_deferred_tokens".to_string(),
                            attr.core_deferred_tokens.to_string(),
                        );
                    }
                    ctx.transforms_applied.push(format!(
                        "router:tool_search_deferral:{}tools:{}tok",
                        attr.deferred_tools, attr.deferred_tokens
                    ));
                }
                if attr.stripped_third_party > 0 {
                    ctx.tags.insert(
                        "third_party_tool_search_stripped".to_string(),
                        attr.stripped_third_party.to_string(),
                    );
                }
            }
        }
        bytes
    };
    let body_to_send = if state.config.image_optimize {
        maybe_optimize_images(body_to_send, request_id)
    } else {
        body_to_send
    };
    if state.config.context_edit {
        maybe_inject_context_management(body_to_send, &state.config, request_id)
    } else {
        body_to_send
    }
}

/// Wire-footprint accounting plus outbound capture.
///
/// Last measurement before the body leaves: spawns the footprint task on the
/// bytes that go out, records the `footprint` stage, and captures the
/// outbound body. Anthropic-messages only; other endpoints pass through.
/// Callers keep the `post` gap timing. Extracted from `forward_http`
/// without behavior change.
pub(crate) fn record_wire_ledger(
    body_to_send: &bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    original_buffered: &bytes::Bytes,
    stage_timer: &mut crate::stage_timer::StageTimer,
) {
    // Footprint accounting. Last, so `body_to_send` is what actually goes
    // on the wire. `tokens_saved` is measured after the injection stages
    // have already run, so without this the bytes the proxy adds are baked
    // into its own baseline and never appear as a cost.
    if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        //
        // Off the request path: two parses of a ~200 kB body and, once a
        // second, a temp-file + fsync + rename in the tracker, for a number
        // nothing downstream reads. Measured 14.4 ms p50, 92 ms p99 and a
        // 63 s max on this stage (2026-09-03, n=2,014). The tracker's mutex
        // orders the writes; the `Bytes` clones are refcount bumps.
        let footprint_start = Instant::now();
        spawn_request_footprint(
            state.savings_tracker.clone(),
            request_id.to_owned(),
            original_buffered.clone(),
            body_to_send.clone(),
        );
        stage_timer.record(
            "footprint",
            footprint_start.elapsed().as_secs_f64() * 1000.0,
        );
    }

    cache_stabilization::capture::maybe_capture_outbound(body_to_send, request_id);
}

/// Pre-send seam: context-edit beta, memory tool, request hooks.
///
/// Mutates `outgoing_headers` for the context-edit beta and `outcome_ctx`
/// for hook savings attribution; returns the (possibly hooked) body.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn run_presend_seam(
    body_to_send: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    outgoing_headers: &mut HeaderMap,
    outcome_ctx: &mut Option<OutcomeContext>,
) -> bytes::Bytes {
    if state.config.context_edit
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        )
    {
        append_anthropic_beta(
            outgoing_headers,
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
            if handler.is_initialized() {
                for (name, value) in handler.get_beta_headers() {
                    if name.eq_ignore_ascii_case("anthropic-beta") {
                        append_anthropic_beta(outgoing_headers, &value);
                    }
                }
            }
        }
    }

    // Turn hooks: pre-send `on_request` seam. Inert (byte-identical)
    // unless a hook is registered — the empty-registry check avoids
    // touching the body at all, matching Python's "inert unless a hook is
    // registered" contract. A hook that shrinks the tool array is
    // deferral-shaped (removes schemas counting never saw), so its
    // saving lands in tags, additive to `tokens_saved`.
    let body_to_send = if crate::turn_hooks::registered_turn_hooks().is_empty() {
        body_to_send
    } else {
        let (bytes, tools_saved) = apply_request_hooks(body_to_send, endpoint, request_id);
        if tools_saved > 0 {
            if let Some(ctx) = outcome_ctx.as_mut() {
                let entry = ctx
                    .tags
                    .entry("turn_hook_tools_saved_tokens".to_string())
                    .or_insert_with(|| "0".to_string());
                *entry = entry
                    .parse::<i64>()
                    .unwrap_or(0)
                    .saturating_add(tools_saved)
                    .to_string();
                ctx.transforms_applied
                    .push(format!("turn_hook:tools:{tools_saved}tok"));
            }
        }
        bytes
    };
    body_to_send
}

/// Post-rewrite finalizers: reasoning restore, TTL order, tool-search
/// repair, codex tool restore.
///
/// Pure body transforms (plus repair attribution on `outcome_ctx`).
/// Extracted from `forward_http` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_finalize_pipeline(
    body_to_send: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    original_buffered: &bytes::Bytes,
    auth_mode: AuthMode,
    outcome_ctx: &mut Option<OutcomeContext>,
    additional_tools_restore_plan: &Option<crate::handlers::responses::AdditionalToolsRestorePlan>,
) -> bytes::Bytes {
    // Signed reasoning blocks, checked after every stage that can rewrite
    // the message array has run.
    let body_to_send = if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        restore_client_reasoning_blocks(body_to_send, original_buffered, request_id)
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
            original_buffered,
            // B1 pins every marker to 1h on the operator's orders, so a 1h
            // marker on such a turn is theirs, not a leak from an earlier
            // one.
            state.config.force_1h_cache_ttl && auth_mode != AuthMode::Payg,
            request_id,
        )
    } else {
        body_to_send
    };
    // Tool-search history repair. Last tools/messages mutator: runs
    // after turn hooks and the TTL ordering, validating message history
    // against the final tools array.
    let body_to_send = if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        let (bytes, neutralized) = maybe_repair_tool_search_history(body_to_send, request_id);
        if neutralized > 0 {
            if let Some(ctx) = outcome_ctx.as_mut() {
                ctx.transforms_applied
                    .push(format!("router:tool_search_repair:{neutralized}blocks"));
            }
        }
        bytes
    } else {
        body_to_send
    };

    // CCR retrieve history repair (upstream #2814). Beside the
    // tool-search repair, after both normal CCR injection and turn
    // hooks: a side-request that does not declare `headroom_retrieve`
    // cannot retain historical references the upstream would reject.
    let body_to_send = if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        let (bytes, neutralized) = maybe_repair_ccr_retrieve_history(body_to_send, request_id);
        if neutralized > 0 {
            if let Some(ctx) = outcome_ctx.as_mut() {
                ctx.transforms_applied
                    .push(format!("router:ccr_retrieve_repair:{neutralized}blocks"));
            }
        }
        bytes
    } else {
        body_to_send
    };

    // Orphan tool_result repair. Last history validator: the siblings
    // only remove calls, which can strand results, and Anthropic checks
    // result/call pairing independently of the tools array.
    let body_to_send = if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        let (bytes, neutralized) = maybe_repair_orphan_tool_results(body_to_send, request_id);
        if neutralized > 0 {
            if let Some(ctx) = outcome_ctx.as_mut() {
                ctx.transforms_applied
                    .push(format!("router:orphan_tool_repair:{neutralized}blocks"));
            }
        }
        bytes
    } else {
        body_to_send
    };
    crate::handlers::responses::restore_codex_additional_tools_body(
        body_to_send,
        additional_tools_restore_plan.as_ref(),
        request_id,
    )
}

/// The proxy handle and what every log line about a request carries.
///
/// One value in place of the `state` / `endpoint` / `request_id` /
/// `headers_snapshot` quadruple the forward path repeats at nearly every
/// stage. Copy, so a stage can pass it on after destructuring it.
#[derive(Clone, Copy)]
pub(crate) struct RequestScope<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) endpoint: compression::CompressibleEndpoint,
    pub(crate) request_id: &'a str,
    pub(crate) headers_snapshot: &'a Option<HeaderMap>,
}

/// The keys a turn is filed under.
///
/// Lane and session are not interchangeable: sibling lanes share a session
/// but not a cached prefix, so a ledger keyed on the wrong one reads every
/// alternation as churn.
#[derive(Clone, Copy)]
pub(crate) struct RequestKeys<'a> {
    pub(crate) lane_key: &'a str,
    pub(crate) session_key: &'a str,
    pub(crate) conversation_key: &'a str,
    pub(crate) api_kind: Option<ApiKind>,
}

/// The bytes a pre-send observation is about: what leaves, against what
/// arrived. `original_buffered_len` is measured before any stage runs, so it
/// is not `original_buffered.len()` once a transform has rewritten it.
#[derive(Clone, Copy)]
pub(crate) struct PresendBodies<'a> {
    pub(crate) body_to_send: &'a bytes::Bytes,
    pub(crate) original_buffered: &'a bytes::Bytes,
    pub(crate) original_buffered_len: usize,
}

/// Pre-send wire ledger: sizes, pairing, outcome booking.
///
/// First half of `observe_presend`. Extracted to keep both halves under the
/// complexity threshold; no behavior change.
pub(crate) fn observe_wire_ledger(
    scope: RequestScope<'_>,
    keys: RequestKeys<'_>,
    path_for_log: &str,
    bodies: PresendBodies<'_>,
    outcome_ctx: &mut Option<OutcomeContext>,
) {
    let RequestScope {
        state,
        endpoint,
        request_id,
        ..
    } = scope;
    let RequestKeys {
        lane_key: request_lane_key,
        session_key: request_session_key,
        ..
    } = keys;
    let PresendBodies {
        body_to_send,
        original_buffered,
        original_buffered_len,
    } = bodies;
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
        // Lane, not session: the roster baseline below is per prefix
        // lineage, and sibling lanes carry different tool sets —
        // sharing one baseline reads every alternation as churn.
        log_prefix_composition(request_id, request_lane_key, body_to_send);
        // Tool-use pairing on the exact wire bytes, attributed against
        // the client's own: a turn the provider is about to refuse for
        // an unpaired tool_use gets named here first, with who broke it.
        if matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
            check_outbound_tool_pairing(
                request_id,
                request_session_key,
                original_buffered,
                body_to_send,
            );
        }
        // Feed the ground-truth ledger. Sizes come off the wire, and the
        // arm label makes a compression-on vs compression-off comparison a
        // query instead of an argument.
        state.usage_observer.note_wire_bytes(
            request_id,
            received.max(0) as u64,
            sent.max(0) as u64,
            state.config.compression_mode.as_str(),
        );
        // Hand the pair to the outcome, which books it against the
        // provider's usage once the response reports one.
        if let Some(ctx) = outcome_ctx.as_mut() {
            ctx.wire_bytes = Some((received, sent));
            ctx.forwarded_tokens_estimate = headroom_core::tokenizer::get_tokenizer(&ctx.model)
                .count_text(&String::from_utf8_lossy(body_to_send))
                as i64;
        }
    }
}

/// Pre-send fingerprint: cache-key inputs of the forwarded request.
///
/// Second half of `observe_presend`'s drift/fingerprint section. Logs the
/// per-turn cache-key fingerprint and parks forward witnesses. Extracted to
/// keep both halves under the complexity threshold; no behavior change.
pub(crate) fn observe_forward_fingerprint(
    body_to_send: &bytes::Bytes,
    scope: RequestScope<'_>,
    keys: RequestKeys<'_>,
) {
    let RequestScope {
        state,
        request_id,
        headers_snapshot,
        ..
    } = scope;
    let RequestKeys {
        session_key: request_session_key,
        conversation_key: request_conversation_key,
        api_kind: request_api_kind,
        ..
    } = keys;
    if request_api_kind.is_some() {
        if let (Some((model, markers, breakpoints)), Some((sys, tools))) = (
            cache_key_fingerprint(body_to_send),
            preamble_digests(body_to_send),
        ) {
            let beta = headers_snapshot
                .as_ref()
                .and_then(|h| h.get("anthropic-beta"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            tracing::info!(
                event = "turn_cache_fingerprint",
                request_id = %request_id,
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(
                    request_session_key
                ),
                conversation_key = %request_conversation_key,
                model = %model,
                // Both are cache-key inputs that sit ahead of message 0, so
                // a change in either voids the whole prefix.
                system_digest = %format!("{sys:016x}"),
                tools_digest = %format!("{tools:016x}"),
                markers = %markers,
                breakpoints = breakpoints,
                // Cross-turn stability of the forwarded messages. Within-turn
                // rewrites are expected and harmless if deterministic; only a
                // ladder checkpoint that moves between turns costs cache.
                prefix_ladder = %prefix_digest_ladder(body_to_send).unwrap_or_default(),
                // The head ladder stops doubling at 32, so on a long turn
                // the whole disputed tail sits past its last checkpoint.
                // These windows cover the last 1/2/4 messages instead; the
                // smallest one that moved bounds the churn.
                tail_ladder = %tail_digest_ladder(body_to_send).unwrap_or_default(),
                beta_digest = %short_hash(beta),
                // Which account sent the turn. The provider's cache is per
                // credential, so a `/login` account switch recaches every
                // live conversation — legitimate, but indistinguishable
                // from waste unless it is recorded. Hashed, never the key.
                auth_digest = %headers_snapshot
                    .as_ref()
                    .and_then(|h| h.get("authorization").or_else(|| h.get("x-api-key")))
                    .and_then(|v| v.to_str().ok())
                    .map(short_hash)
                    .unwrap_or_default(),
                msgs = serde_json::from_slice::<serde_json::Value>(body_to_send)
                    .ok()
                    .and_then(|v| v.get("messages").and_then(|m| m.as_array()).map(|a| a.len()))
                    .unwrap_or(0),
                "cache-key inputs of the request as forwarded"
            );
            // Park the cache-key inputs neither drift lane sees (beta
            // header, marker layout, post-router model) on the pending
            // turn, so a later recache event can say whether they moved.
            // Short digests only — the same strings logged one line up —
            // except the model, which is small-cardinality and logged raw.
            state.usage_observer.note_forward_witnesses(
                request_id,
                short_hash(beta),
                markers.clone(),
                model.clone(),
            );
        }
    }
}

/// The two clocks the pre-send stage timings are measured against: `start`
/// is the whole request, `post_start` the footprint-to-forward gap.
#[derive(Clone, Copy)]
pub(crate) struct PresendTiming<'a> {
    pub(crate) start: &'a Instant,
    pub(crate) post_start: &'a Instant,
}

/// What the pre-send observation writes back: the outcome record, the stage
/// timings, and the two copies of the forwarded body a retry needs.
pub(crate) struct PresendSinks<'a> {
    pub(crate) outcome_ctx: &'a mut Option<OutcomeContext>,
    pub(crate) stage_timer: &'a mut crate::stage_timer::StageTimer,
    pub(crate) retry_body: &'a mut Option<bytes::Bytes>,
    pub(crate) forwarded_body: &'a mut Option<bytes::Bytes>,
}

/// Pre-send observability: wire bytes, pairing, drift, fingerprint, watchdog.
///
/// Logs outbound sizes, checks tool pairing, books wire bytes on the
/// outcome, takes the second drift reading, fingerprints the forwarded
/// prefix, and runs the watchdog/timeout guards (recording `post` /
/// `pre_forward`). Returns the post-replay digests for the watchdog.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn observe_presend(
    bodies: PresendBodies<'_>,
    scope: RequestScope<'_>,
    keys: RequestKeys<'_>,
    path_for_log: &str,
    post_replay_digests: &Option<Vec<u64>>,
    timing: PresendTiming<'_>,
    sinks: PresendSinks<'_>,
) {
    let RequestScope {
        state, request_id, ..
    } = scope;
    let RequestKeys {
        lane_key: request_lane_key,
        api_kind: request_api_kind,
        ..
    } = keys;
    let PresendBodies { body_to_send, .. } = bodies;
    let PresendTiming { start, post_start } = timing;
    let PresendSinks {
        outcome_ctx,
        stage_timer,
        retry_body,
        forwarded_body,
    } = sinks;
    observe_wire_ledger(scope, keys, path_for_log, bodies, outcome_ctx);
    // Second drift reading, on the bytes that leave. Every mutation stage
    // has run by here, so a hot-zone change the client did not make is
    // ours: memory injection, context injection, prefix replay, the
    // breakpoint move. The classifier prefers the inbound dims when both
    // moved, so this only ever speaks for a turn the client held still on.
    //
    // Costs one more parse of the outbound body. The pipeline above
    // already parses and re-serializes it several times, so this is a
    // small addition to a cost that is already paid.
    if let Some(kind) = request_api_kind {
        // Tri-state, and the middle one is the point: `Some("")` says the
        // lane was compared and the forwarded body held still in all three
        // dimensions, which is how a stabilizer absorbing a client edit
        // shows up. `None` stays reserved for "no comparison" — a birth
        // turn, or a body that never parsed — so a positive finding is
        // never confused with an absent one.
        let outbound_dims = serde_json::from_slice::<serde_json::Value>(body_to_send)
            .ok()
            .and_then(|sent| {
                let (dims, birth) =
                    cache_stabilization::drift_detector::observe_outbound_drift_with_birth(
                        &state.outbound_drift_state,
                        request_lane_key,
                        compute_structural_hash(&sent, kind),
                    );
                (!birth).then(|| dims.unwrap_or_default())
            });
        state
            .usage_observer
            .note_outbound_drift(request_id, outbound_dims);
    }

    // The invariant, checked rather than assumed: nothing between the
    // replay stage and here may disturb a message the provider has already
    // cached. Measured over the 2026-08-20/22 logs, 58 turns lost 1.29M
    // tokens with the replay reported applied and the cache dead anyway,
    // and no logged event told them apart from healthy turns — because the
    // one fact that would have is whether the bytes still matched.
    //
    // Only the settled prefix is checked. The last two messages are this
    // turn's own live tail and are expected to move.
    // One fingerprint per turn of everything the provider's cache key
    // depends on. The 58 residue turns had replay applied and the cache
    // dead with nothing to tell them apart from healthy turns; rather than
    // one instrument per guess, log the facts and diff turn-against-turn
    // offline, which also catches causes not yet guessed at.
    observe_forward_fingerprint(body_to_send, scope, keys);
    if let (Some(before), Some(after)) = (
        post_replay_digests.as_ref(),
        request_api_kind.and_then(|_| message_digests(body_to_send)),
    ) {
        // A change in message *count* is still worth a warning: it shifts
        // every later message regardless of what any stage computed.
        if before.len() != after.len() {
            tracing::warn!(
                event = "forwarded_prefix_length_changed_after_replay",
                request_id = %request_id,
                messages_before = before.len(),
                messages_after = after.len(),
                "a stage after prefix replay added or removed messages"
            );
        }
    }

    // Everything headroom did before the bytes leave. This is the number
    // that has to be reconstructed from log-line gaps when it is missing,
    // and the one a latency complaint is about.
    // Plan 2 step 1: `post` gap ends here (footprint end to pre_forward).
    stage_timer.record("post", post_start.elapsed().as_secs_f64() * 1000.0);
    stage_timer.record("pre_forward", start.elapsed().as_secs_f64() * 1000.0);

    *retry_body = Some(body_to_send.clone());
    *forwarded_body = retry_body.clone();
}

/// Request preamble, part 2: forwarded host, outgoing headers, strip log.
///
/// Builds `outgoing_headers` off the incoming ones, logs the strip outcome,
/// and restores Host when `rewrite_host` is off. Returns
/// `(outgoing_headers, strip_internal, pre_strip_internal_count)`.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn build_outgoing_headers(
    req: &Request<Body>,
    state: &AppState,
    client_addr: &std::net::SocketAddr,
    forwarded_host: Option<&str>,
    request_id: &str,
    auth_mode: AuthMode,
) -> (HeaderMap, bool, usize) {
    // PR-F4 (P5-53): synthetic X-Forwarded-* / X-Request-Id injection is
    // skipped for Subscription traffic (fingerprint risk). Staged behind
    // the same enforcement flag as CompressionPolicy (c3/6 pattern): with
    // enforcement disabled every request keeps the PAYG behavior.
    let header_auth_mode = if state.config.auth_mode_policy_enforcement.is_enabled() {
        auth_mode
    } else {
        AuthMode::Payg
    };
    let strip_internal = state.config.strip_internal_headers.is_enabled();
    let pre_strip_internal_count = req
        .headers()
        .iter()
        .filter(|(name, _)| crate::headers::is_internal_header(name))
        .count();
    let mut outgoing_headers = build_forward_request_headers(
        req.headers(),
        client_addr.ip(),
        "http",
        forwarded_host,
        request_id,
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
            "[redacted:secret]; \
             internal x-headroom-* headers forwarded to upstream"
        );
    }
    if !state.config.rewrite_host {
        if let Some(h) = req.headers().get(http::header::HOST) {
            outgoing_headers.insert(http::header::HOST, h.clone());
        }
    }
    (outgoing_headers, strip_internal, pre_strip_internal_count)
}

/// Return bundle for [`derive_session_identity`]: volatile findings plus the
/// derived session identity `(api-kind, session key, conversation key, lane key)`.
#[allow(clippy::type_complexity)]
pub(crate) type SessionIdentityOut = (
    Vec<cache_stabilization::volatile_detector::VolatileFinding>,
    Option<(ApiKind, String, String, String)>,
);

/// Derives volatile findings and the session identity for a buffered body.
///
/// First half of `analyze_buffered_session`: volatile detection plus the
/// shared session-identity derivation. Returns `(findings, identity)`.
/// Extracted to keep both halves under the complexity threshold; no behavior
/// change.
pub(crate) fn derive_session_identity(
    parsed: &serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    headers_snapshot: &Option<HeaderMap>,
    client_addr: &std::net::SocketAddr,
) -> SessionIdentityOut {
    // PR-E5: volatile-content detector. Emits one WARN per
    // finding (capped at 10) for content that busts cache
    // (timestamps, UUIDs, ID-named fields).
    let volatile_kind = cache_stabilization::volatile_detector::ApiKind::from_endpoint(endpoint);
    let findings =
        cache_stabilization::volatile_detector::detect_volatile_content(parsed, volatile_kind);
    // PR-E6: cache-bust drift detector. SHA-256 fingerprints
    // the cache hot zone (system / tools / first 3 messages);
    // a mismatch between consecutive turns of the same session
    // emits a `cache_drift_observed` event so operators see
    // invisible cache busts.
    let drift_kind = match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => Some(ApiKind::Anthropic),
        compression::CompressibleEndpoint::OpenAiChatCompletions => Some(ApiKind::OpenAiChat),
        compression::CompressibleEndpoint::OpenAiResponses => Some(ApiKind::OpenAiResponses),
    };

    // Derived once and shared by the volatile warnings below and the
    // drift detector after them. `derive_session_key` costs up to six
    // SHA-256 digests over canonicalized subtree clones, so deriving it
    // per consumer would put that on the hot path of every request
    // twice over — for a log field.
    let session_identity = match (drift_kind, headers_snapshot.as_ref()) {
        (Some(kind), Some(headers)) => {
            let key = derive_session_key(headers, client_addr, parsed, kind);
            // Lane before the previews: the pins key by it, and the
            // drift baseline, replay store, and observer below take it
            // too. One extra structural hash on the unmutated body;
            // the previews restore byte-identical, so this is the same
            // lane the post-preview hash below would derive.
            let lane = stream_lane_key(&key, &compute_structural_hash(parsed, kind));
            // Lane, not session: the usage observer files this turn
            // under the lane (begin_request below), and the volatile
            // warnings and turn fingerprint join on this key — a
            // session key would silently split one lane into two
            // conversations offline.
            let conversation = cache_stabilization::usage_observer::conversation_key(parsed, &lane);
            Some((kind, key, conversation, lane))
        }
        _ => None,
    };
    (findings, session_identity)
}

/// Parks session observations: usage identity, TTL, shed, ctx, betas.
///
/// Second half of `analyze_buffered_session`'s session arm. Returns
/// `Some(response)` on a concurrency-cap shed; `None` otherwise.
/// Extracted to keep both halves under the complexity threshold; no behavior
/// change.
pub(crate) fn park_session_observations(
    parsed: &serde_json::Value,
    scope: RequestScope<'_>,
    keys: RequestKeys<'_>,
    drift_dims: Option<String>,
    outgoing_headers: &mut HeaderMap,
) -> Option<Response<Body>> {
    let RequestScope {
        state,
        endpoint,
        request_id,
        headers_snapshot,
    } = scope;
    let RequestKeys {
        lane_key: request_lane_key,
        session_key,
        conversation_key: conversation,
        ..
    } = keys;
    // CTX-7: park conversation identity + drift dims under
    // the request id so the response-side usage observer
    // can classify this turn's billed usage against the
    // conversation's previous turn. Keyed by lane, not session:
    // same-opener streams must not share usage baselines.
    state.usage_observer.begin_request(
        request_id,
        cache_stabilization::usage_observer::conversation_key(parsed, request_lane_key),
        Some(session_key),
        drift_dims,
        Some(cache_stabilization::usage_observer::prefix_fingerprint(
            parsed,
        )),
    );
    // Presence for /debug/active-conversations: the canonical
    // project dir parks alongside the usage identity, before the
    // shed check below can pop the entry.
    state.usage_observer.note_project(
        request_id,
        resolve_ctx_project(
            headers_snapshot.as_ref(),
            parsed,
            state.config.memory_project_root.as_deref(),
        ),
    );
    // Price the stock arm at the tier the client actually bought:
    // `parsed` still carries its own markers here, before the
    // pipeline adds or rewrites any. Main-loop traffic arrives
    // on 1h, subagent traffic on the 5-minute default.
    state.usage_observer.note_client_cache_ttl(
        request_id,
        cache_stabilization::cache_ttl::client_ttl_shape(parsed),
    );
    // The stock arm is priced in Anthropic input-equivalents.
    // Turns forwarded to another cache universe (OpenAI chat /
    // Responses) bill without a creation counter, TTL split, or
    // Anthropic horizons, so comparing them at 1.25x/2.0x would
    // invent a premium the provider never charged.
    if !matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        state.usage_observer.note_stock_ineligible(request_id);
    }
    // Conversation-concurrency cap (`--max-conversation-concurrency`):
    // shed fan-out overlap with the client's own retry instead of
    // racing the provider's cache commit on every turn. The pending
    // entry just parked is popped by the shed call, so the turn
    // neither flags later turns concurrent nor lingers as
    // abandoned. Disabled at 0. Ordinary interactive overlap never
    // reaches a cap worth setting; only storms trip it.
    if let Some(in_flight) = state.usage_observer.shed_if_over_conversation_cap(
        request_id,
        conversation,
        state.config.max_conversation_concurrency,
    ) {
        crate::observability::proxy_counters::record_concurrency_shed();
        tracing::warn!(
            event = "conversation_concurrency_shed",
            request_id = %request_id,
            conversation_key = %conversation,
            in_flight,
            cap = state.config.max_conversation_concurrency,
            "conversation over its concurrency cap; shed with 429 so the client retries against a committed prefix"
        );
        return Some(conversation_concurrency_shed_response(
            in_flight,
            state.config.max_conversation_concurrency,
        ));
    }
    // Read off the client's body here, once: the observer has no
    // messages by the time usage comes back, and a first turn
    // that writes cache needs them to say why.
    state.usage_observer.note_first_turn_context(
        request_id,
        cache_stabilization::usage_observer::first_turn_context(parsed),
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
    cache_stabilization::capture::maybe_capture(parsed, endpoint_label, session_key, request_id);

    // CTX-2: passive session capture. Same spot + inputs as
    // maybe_capture; same never-block rule — `observe` clones the
    // body once and hands it to a detached worker. No-op unless
    // `ctx_capture` is enabled (then `ctx_observer` is `Some`).
    if let Some(observer) = state.ctx_observer.as_ref() {
        let project_dir = resolve_ctx_project(
            headers_snapshot.as_ref(),
            parsed,
            state.config.memory_project_root.as_deref(),
        );
        observer.observe(parsed, session_key, &project_dir);
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
            compression::CompressibleEndpoint::AnthropicMessages => BetaProvider::Anthropic,
            compression::CompressibleEndpoint::OpenAiChatCompletions
            | compression::CompressibleEndpoint::OpenAiResponses => BetaProvider::OpenAi,
        };
        cache_stabilization::beta_sticky::apply_sticky_betas(
            &state.beta_sticky,
            provider,
            request_lane_key,
            outgoing_headers,
            request_id,
        );
    }
    None
}
/// What the session analysis writes back into the caller's locals.
///
/// Every one of these is an output: the request arrives without a lane or a
/// session, and the analysis is what decides them, along with whether the
/// prefix boundary has to be rebuilt and which headers go out.
pub(crate) struct SessionAnalysisOut<'a> {
    pub(crate) session_key: &'a mut String,
    pub(crate) lane_key: &'a mut String,
    pub(crate) conversation_key: &'a mut String,
    pub(crate) api_kind: &'a mut Option<ApiKind>,
    pub(crate) rebuild_boundary: &'a mut bool,
    pub(crate) pre_boundary_agreement: &'a mut Option<usize>,
    pub(crate) outgoing_headers: &'a mut HeaderMap,
}

/// Session/volatile/drift analysis over the parsed buffered body.
///
/// Runs the volatile detector, derives the session identity, emits drift
/// events, previews working-dir/role-sentence pins, runs the replay-store
/// boundary logic, and parks usage-observer entries. Writes the derived
/// session/lane/conversation/api-kind keys plus boundary flags through
/// `&mut` out-params. Returns `Some(response)` on a concurrency-cap shed
/// (caller returns it directly); `None` otherwise.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn analyze_buffered_session(
    parsed: &mut serde_json::Value,
    scope: RequestScope<'_>,
    client_addr: &std::net::SocketAddr,
    out: SessionAnalysisOut<'_>,
) -> Option<Response<Body>> {
    let RequestScope {
        state,
        endpoint,
        request_id,
        headers_snapshot,
    } = scope;
    let SessionAnalysisOut {
        session_key: request_session_key,
        lane_key: request_lane_key,
        conversation_key: request_conversation_key,
        api_kind: request_api_kind,
        rebuild_boundary,
        pre_boundary_agreement,
        outgoing_headers,
    } = out;
    let (findings, session_identity) =
        derive_session_identity(parsed, endpoint, headers_snapshot, client_addr);
    if !findings.is_empty() {
        // Same identity the drift and recache events carry, so a
        // volatile finding can be joined to the bust it is suspected of
        // causing. Item 4 cannot be settled without it: the warning
        // fires on static sample text as readily as on real per-request
        // churn, and only a per-conversation join tells the two apart.
        let session_hash = session_identity
            .as_ref()
            .map(|(_, key, _, _)| cache_stabilization::drift_detector::session_key_log_prefix(key));
        cache_stabilization::volatile_detector::emit_volatile_warnings(
            &findings,
            request_id,
            session_hash.as_deref(),
            session_identity
                .as_ref()
                .map(|(_, _, conv, _)| conv.as_str()),
        );
    }

    if let Some((kind, session_key, conversation, lane)) = session_identity {
        *request_session_key = session_key.clone();
        *request_conversation_key = conversation.clone();
        *request_api_kind = Some(kind);
        *request_lane_key = lane;
        // A lane switch that continues another lane's message lineage
        // inherits its hold pins before the previews below read them;
        // without this the fresh lane latches the live form and the
        // `cd` the holds exist to mask costs a full rewrite.
        if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
            inherit_lane_pins(state, request_lane_key, messages, request_id);
        }
        // Hash the body the way it will be forwarded. The
        // working-directory hold rewrites `system` further down, so
        // hashing the client's own form calls a `cd` a hot-zone change
        // and drops the stored prefix — the re-cache the hold exists to
        // stop. Put the client's `system` back straight after: nothing
        // below here may see the pinned view.
        let previewed = if state.config.hold_working_directory
            && state.config.prefix_replay
            && matches!(kind, ApiKind::Anthropic)
        {
            state.working_dir_pins.preview(parsed, request_lane_key)
        } else {
            None
        };
        // Same again for the opening sentence: hash the held form so a
        // client-side flip does not read as a hot-zone change.
        let sentence_previewed = if state.config.hold_role_sentence
            && state.config.prefix_replay
            && matches!(kind, ApiKind::Anthropic)
        {
            state.role_sentence_pins.preview(parsed, request_lane_key)
        } else {
            None
        };
        let hash = compute_structural_hash(parsed, kind);
        // Restore in reverse order: the sentence preview saw the
        // directory-held view, so its copy goes back first.
        if let (Some(original), Some(slot)) = (sentence_previewed, parsed.get_mut("system")) {
            *slot = original;
        }
        if let (Some(original), Some(slot)) = (previewed, parsed.get_mut("system")) {
            *slot = original;
        }
        let (drift_dims, lane_birth) =
            observe_drift_with_birth(&state.drift_state, request_lane_key, hash);
        *rebuild_boundary = drift_dims.is_some();

        // Cross-session gate seeding: a newborn lane whose SESSION the
        // gate never saw (model switch, resume) inherits the same
        // conversation's conversions, so known blocks convert on
        // first sight instead of stalling Deferred. Known-session new
        // lanes (same session, new system) hit the shared gate and
        // refuse inside `seed_if_absent` — benign. Runs before the
        // offload policy below reads the gate (S1a ordering), on the
        // client's restored body, and only when flagged on.
        if lane_birth {
            if let (Some(runtime), Some(headers)) =
                (state.ctx_offload.as_ref(), headers_snapshot.as_ref())
            {
                if runtime.config.cross_session_seed {
                    crate::compression::ctx_offload::seed_newborn_session(
                        &runtime.gate,
                        headers,
                        client_addr,
                        parsed,
                        kind,
                        request_session_key,
                        request_id,
                    );
                }
            }
        }

        // The hot zone changed, so every prefix this lane had
        // cached shares a preamble the provider no longer holds —
        // including the alternates, which are prefixes for the same
        // dead cache. Drop them before `apply_prefix_replay` runs
        // below, so the next turn opens a fresh chain instead of
        // splicing bytes against a cache that is gone. A lane switch
        // (same session, new system) does NOT land here — it mints a
        // fresh baseline with no warn — so sibling streams stop
        // invalidating each other.
        if *rebuild_boundary {
            *pre_boundary_agreement = parsed
                .get("messages")
                .and_then(serde_json::Value::as_array)
                .and_then(|messages| {
                    state
                        .replay_store
                        .forwarded_agreement_len(request_lane_key, messages)
                });
            state.replay_store.invalidate(request_lane_key);
            tracing::info!(
                event = "prefix_replay_invalidated_on_rebuild",
                request_id = %request_id,
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(&session_key),
                "dropped stored prefix at a drift/rebuild boundary"
            );
        }

        return park_session_observations(
            parsed,
            scope,
            RequestKeys {
                lane_key: request_lane_key,
                session_key: &session_key,
                conversation_key: &conversation,
                api_kind: Some(kind),
            },
            drift_dims,
            outgoing_headers,
        );
    }
    None
}

/// Return bundle for [`run_ctx_inject_offload`]: offload records, CCR workspace, latest query, turn number.
#[allow(clippy::type_complexity)]
pub(crate) type CtxInjectOffloadOut<'a> = (
    bool,
    Option<(
        &'a CtxOffloadRuntime,
        Vec<crate::compression::ctx_offload::OffloadRecord>,
    )>,
    Option<(String, Option<String>)>,
    String,
    u32,
    crate::injection_budget::InjectionBudget,
    String,
);

/// CTX inject-engine pass over the buffered value.
///
/// Runs the inject engine's per-request injection. Sets `changed` on
/// injection. Extracted from `run_ctx_inject_offload` without behavior change.
pub(crate) fn run_ctx_engine_inject(
    value: &mut serde_json::Value,
    state: &AppState,
    request_session_key: &str,
    ctx_project: &str,
    injection_budget: &crate::injection_budget::InjectionBudget,
    request_id: &str,
    changed: &mut bool,
) {
    if let Some(engine) = state.ctx_inject.as_ref() {
        let session_key = request_session_key;
        if engine.maybe_inject_for_request(
            value,
            session_key,
            ctx_project,
            injection_budget,
            request_id,
        ) {
            *changed = true;
        }
    }
}

/// Logs the ctx-offload accounting event for converted blocks.
///
/// Pure logging split of `run_ctx_offload_records`. No behavior change.
pub(crate) fn log_ctx_offload_accounting(
    out: &crate::compression::ctx_offload::OffloadOutcome,
    state: &AppState,
    request_id: &str,
    request_lane_key: &str,
    session_key: &str,
    rebuild_boundary: bool,
    history_rewritten: bool,
) {
    if out.blocks_offloaded > 0 || out.blocks_deferred > 0 {
        // CTX-6: offload metrics are recorded by the
        // offload-store worker after both CCR and FTS writes
        // succeed, not here —
        // see ctx/offload_store.rs.
        // Bounded hash list: first 8 offloaded hashes, so the log joins
        // offload to later retrieval (same request_id or session key)
        // without unbounded log growth on huge turns.
        let hashes: Vec<&str> = out
            .records
            .iter()
            .take(8)
            .map(|r| r.hash.as_str())
            .collect();
        tracing::info!(
            event = "ctx_offload_accounting",
            request_id = %request_id,
            session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
            blocks_offloaded = out.blocks_offloaded,
            blocks_deferred = out.blocks_deferred,
            bytes_deferred = out.bytes_deferred,
            // -1 where there is nothing to report, so the
            // field stays a number the audit scripts can
            // read; `?` would print "Some(4)" as a string.
            deferred_min_distance =
                out.deferred_min_distance.map_or(-1, |d| d as i64),
            deferred_max_distance =
                out.deferred_max_distance.map_or(-1, |d| d as i64),
            // What converting the whole backlog would cost
            // in rewritten prefix, against what it frees.
            bytes_after_deepest_deferral = out.bytes_after_deepest_deferral,
            turns_seen = state
                .replay_store
                .turns_seen(request_lane_key)
                .map_or(-1, |t| t as i64),
            window_offloads = out.window_offloads,
            tokens_saved = out.tokens_saved,
            offloaded_hashes = ?hashes,
            rebuild_boundary,
            history_rewritten,
            "ctx_offload considered tool_result blocks"
        );
    }
}

/// CTX offload-records pass over the buffered value.
///
/// Computes the boundary-gated offload policy and runs the offload.
/// Returns the `(runtime, records)` pair when conversions happened (also
/// setting `changed` and accumulating tokens). Extracted from
/// `run_ctx_inject_offload` without behavior change.
/// The identifiers a ctx stage files its records under.
///
/// A narrower set than [`RequestKeys`]: the ctx stages run below the point
/// where the conversation key and the API kind are known.
#[derive(Clone, Copy)]
pub(crate) struct CtxRequestKeys<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) lane_key: &'a str,
    pub(crate) session_key: &'a str,
}

/// The three boundary decisions the ctx stages branch on.
#[derive(Clone, Copy)]
pub(crate) struct CtxBoundaryFlags {
    pub(crate) rebuild_boundary: bool,
    pub(crate) history_rewritten: bool,
    pub(crate) offload_boundary: bool,
}

pub(crate) fn run_ctx_offload_records<'a>(
    value: &mut serde_json::Value,
    state: &'a AppState,
    keys: CtxRequestKeys<'_>,
    flags: CtxBoundaryFlags,
    changed: &mut bool,
    ctx_transform_tokens_saved: &mut i64,
) -> Option<(
    &'a CtxOffloadRuntime,
    Vec<crate::compression::ctx_offload::OffloadRecord>,
)> {
    let CtxRequestKeys {
        request_id,
        lane_key: request_lane_key,
        session_key: request_session_key,
    } = keys;
    let CtxBoundaryFlags {
        rebuild_boundary,
        history_rewritten,
        offload_boundary,
    } = flags;
    if let Some(runtime) = state.ctx_offload.as_ref() {
        // PR-J4: boundary-gated policy — first conversions of
        // frozen-history blocks only ride a rebuild boundary;
        // live-tail blocks and re-applications always pass.
        let session_key = request_session_key;
        let policy = crate::compression::ctx_offload::OffloadPolicy {
            gate: &runtime.gate,
            session_key,
            rebuild_boundary: offload_boundary,
        };
        let out = crate::compression::ctx_offload::offload_anthropic_request(
            value,
            &runtime.config,
            Some(&policy),
        );
        // Large `tool_use` input strings (a Write's `content`)
        // go the same way, under a stricter gate: a first
        // conversion needs the block to be provably unsent, and
        // that is what the replay store's count says.
        let out = if state.config.ctx_offload_tool_use {
            let forwarded_before = state
                .config
                .prefix_replay
                .then(|| state.replay_store.forwarded_message_count(request_lane_key));
            let ccr = runtime.store.ccr();
            let put = |record: &crate::compression::ctx_offload::OffloadRecord| {
                ccr.put(&record.hash, &record.original)
            };
            let tool_use = crate::compression::ctx_offload::offload_tool_use_inputs(
                value,
                &runtime.config,
                &policy,
                forwarded_before.flatten(),
                &put,
            );
            if tool_use.blocks_offloaded > 0 || tool_use.blocks_deferred > 0 {
                tracing::info!(
                    event = "ctx_offload_tool_use",
                    request_id = %request_id,
                    blocks_offloaded = tool_use.blocks_offloaded,
                    blocks_deferred = tool_use.blocks_deferred,
                    bytes_deferred = tool_use.bytes_deferred,
                    tokens_saved = tool_use.tokens_saved,
                    forwarded_before = ?forwarded_before.flatten(),
                    rebuild_boundary = offload_boundary,
                    "ctx_offload considered tool_use inputs"
                );
            }
            let mut out = out;
            out.blocks_offloaded += tool_use.blocks_offloaded;
            out.blocks_deferred += tool_use.blocks_deferred;
            out.bytes_deferred += tool_use.bytes_deferred;
            if let Some(d) = tool_use.deferred_min_distance {
                out.note_deferred_distance(d);
            }
            if let Some(d) = tool_use.deferred_max_distance {
                out.note_deferred_distance(d);
            }
            out.tokens_saved += tool_use.tokens_saved;
            out.records.extend(tool_use.records);
            out
        } else {
            out
        };
        // PR-J5 thrash guard: an I4 violation (frozen-history
        // conversion on a steady-state turn) is a cache-thrash
        // bug — page-worthy, per the Phase J plan §13.
        if !offload_boundary && out.frozen_new_offloads > 0 {
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
            log_ctx_offload_accounting(
                &out,
                state,
                request_id,
                request_lane_key,
                session_key,
                rebuild_boundary,
                history_rewritten,
            );
        }
        if out.changed() {
            *changed = true;
            *ctx_transform_tokens_saved += out.tokens_saved;
            Some((runtime, out.records))
        } else {
            None
        }
    } else {
        None
    }
}

/// CTX inject + offload head: budget, workspace, injection, offload.
///
/// First half of the ctx-gate `Ok(mut value)` arm: session key, project,
/// budget, CCR workspace/tracker, ctx-inject engine, and the offload
/// records computation. Returns `(changed, tokens_saved, proactive, records,
/// workspace, query, turn)` alongside the mutated `value`.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn run_ctx_inject_offload<'a>(
    value: &mut serde_json::Value,
    state: &'a AppState,
    keys: CtxRequestKeys<'_>,
    headers_snapshot: &Option<HeaderMap>,
    flags: CtxBoundaryFlags,
    ctx_transform_tokens_saved: &mut i64,
    proactive_expansion_applied: &mut bool,
) -> CtxInjectOffloadOut<'a> {
    let CtxRequestKeys {
        request_id,
        lane_key: request_lane_key,
        session_key: request_session_key,
    } = keys;
    let CtxBoundaryFlags {
        rebuild_boundary,
        history_rewritten,
        offload_boundary,
    } = flags;
    let _ctx_session_key = request_session_key;
    let mut changed = false;
    // Which project's stores recall reads and offload writes.
    let ctx_project = resolve_ctx_project(
        headers_snapshot.as_ref(),
        value,
        state.config.memory_project_root.as_deref(),
    );
    let ccr_workspace = resolve_ccr_workspace(
        headers_snapshot.as_ref(),
        value,
        state.config.memory_project_root.as_deref(),
    );
    let latest_user_query = latest_user_query(value);
    let turn_number = anthropic_turn_number(value);

    // One ceiling for every stage that appends to this turn.
    // Drawn down in the order the stages run below; without it
    // three independently-capped appenders could inflate the
    // request while each looked small on its own counter.
    let injection_budget = crate::injection_budget::InjectionBudget::for_request(
        state.config.max_injection_bytes,
        request_id,
    );

    if let Some((workspace_key, workspace_label)) = ccr_workspace.as_ref() {
        if maybe_append_ccr_proactive_expansion(
            state,
            value,
            &latest_user_query,
            workspace_key,
            workspace_label.as_deref(),
            turn_number,
            request_id,
            &injection_budget,
        ) {
            changed = true;
            *proactive_expansion_applied = true;
        }
    } else if state.ccr_context_tracker.is_some() {
        tracing::info!(
            request_id = %request_id,
            "CCR Phase 4: workspace unresolved; proactive expansion disabled for this request"
        );
    }

    run_ctx_engine_inject(
        value,
        state,
        request_session_key,
        &ctx_project,
        &injection_budget,
        request_id,
        &mut changed,
    );

    let offload_records = run_ctx_offload_records(
        value,
        state,
        CtxRequestKeys {
            request_id,
            lane_key: request_lane_key,
            session_key: request_session_key,
        },
        CtxBoundaryFlags {
            rebuild_boundary,
            history_rewritten,
            offload_boundary,
        },
        &mut changed,
        ctx_transform_tokens_saved,
    );
    (
        changed,
        offload_records,
        ccr_workspace,
        latest_user_query,
        turn_number,
        injection_budget,
        ctx_project,
    )
}

/// Semantic-cache lookup on the buffered body.
///
/// Non-streaming only. On a hit returns `Some(response)`; otherwise `None`.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn check_semantic_cache(
    state: &AppState,
    buffered: &bytes::Bytes,
    request_id: &str,
    path_for_log: &str,
) -> Option<Response<Body>> {
    if let Some(ref cache) = state.semantic_cache {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(buffered) {
            let is_streaming = parsed
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !is_streaming {
                let model = parsed
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");
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
                    return Some(
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from(entry.response_body))
                            .unwrap(),
                    );
                }
            }
        }
    }
    None
}

/// Tools-lift, effective auth mode, and replay extraction.
///
/// Lifts codex additional tools on Responses, mirrors the enforcement-flag
/// auth override, and extracts the replay-original messages. Returns
/// `(buffered, restore_plan, effective_auth_mode, replay_original_messages)`.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn prepare_replay_inputs(
    buffered: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    auth_mode: AuthMode,
) -> (
    bytes::Bytes,
    Option<crate::handlers::responses::AdditionalToolsRestorePlan>,
    AuthMode,
    Option<Vec<serde_json::Value>>,
) {
    let (buffered, additional_tools_restore_plan) =
        if matches!(endpoint, compression::CompressibleEndpoint::OpenAiResponses) {
            crate::handlers::responses::lift_codex_additional_tools_body(buffered, request_id)
        } else {
            (buffered, None)
        };

    // Mirror the enforcement-flag override already applied to
    // CompressionPolicy at request entry (line ~416): when
    // `--auth-mode-policy-enforcement disabled` is set, treat
    // every request as PAYG.
    let effective_auth_mode = if state.config.auth_mode_policy_enforcement.is_enabled() {
        auth_mode
    } else {
        AuthMode::Payg
    };

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
    (
        buffered,
        additional_tools_restore_plan,
        effective_auth_mode,
        replay_original_messages,
    )
}

/// Offload boundary flags plus the prior-thinking drop.
///
/// Computes `history_rewritten` / `offload_boundary` / `forwarded_agreement`
/// and applies the thinking-drop pass on boundary turns. Returns
/// `(buffered, history_rewritten, offload_boundary)`.
/// Extracted from `forward_http` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_offload_boundary(
    buffered: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    request_lane_key: &str,
    replay_original_messages: &Option<Vec<serde_json::Value>>,
    rebuild_boundary: bool,
    pre_boundary_agreement: Option<usize>,
) -> (bytes::Bytes, bool, bool) {
    // History the provider cannot read back is written fresh this turn
    // whatever the proxy sends, so offloading its tool
    // result history covers every kind of history, not just the ones we
    // know how to resume. `history_rewritten` marks any session where a
    // length>1 history will be rewritten: some prefix a past turn cached
    // is garbage now, which quietly strands the turns still downstream.
    // Boundaries fire on `offload_boundary`; the replay stage below
    // checks readiness via the tracker instead. Small sessions are
    // eligible too: most sessions are small now, and 7 of 9 sessions with
    // such sessions held 5% of all cache reads over 2026-09-01/02 with
    // none of their history offloaded.
    let history_rewritten = !rebuild_boundary
        && replay_original_messages.as_deref().is_some_and(|messages| {
            messages.len() > 1
                && state
                    .replay_store
                    .history_will_be_rewritten(request_lane_key, messages)
        });
    let offload_boundary = rebuild_boundary || history_rewritten;

    // Same turns, same reason: the provider writes this prefix fresh, so
    // stripping thinking the model will never read back costs nothing a
    // verbatim copy would have kept. Off a boundary the pass must not run —
    // the replay store carries the stripped bytes forward for every
    // message it has seen. Its own block, not part of the ctx pipeline
    // below: entering that one injects the retrieval tool.
    //
    // And not on every boundary either (savings-ideas-2.md §4.2): the
    // drop rewrites the head, so it runs only where the head is
    // rewritten anyway — rebuild boundary, no tracker, or agreement
    // ending at index 0/1. A tail divergence keeps its head thinking;
    // only the tail is re-billed either way.
    // On a boundary turn this is the reading taken before the
    // invalidation; otherwise the tracker is intact and the live read is
    // the same reading. The log below publishes this one too, so the field
    // and the decision can never disagree.
    let forwarded_agreement = pre_boundary_agreement.or_else(|| {
        replay_original_messages.as_deref().and_then(|messages| {
            state
                .replay_store
                .forwarded_agreement_len(request_lane_key, messages)
        })
    });
    let buffered = if state.config.ctx_drop_prior_thinking
        && offload_boundary
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        )
        && buffered
            .windows(b"thinking".len())
            .any(|w| w == b"thinking")
        && crate::compression::prior_thinking::thinking_drop_is_free(
            rebuild_boundary,
            forwarded_agreement,
        ) {
        match serde_json::from_slice::<serde_json::Value>(&buffered) {
            Ok(mut value) => {
                let dropped = crate::compression::prior_thinking::drop_prior_thinking(&mut value);
                if dropped.blocks_removed == 0 {
                    buffered
                } else {
                    tracing::info!(
                        event = "prior_thinking_dropped",
                        request_id = %request_id,
                        blocks_removed = dropped.blocks_removed,
                        bytes_removed = dropped.bytes_removed,
                        rebuild_boundary,
                        history_rewritten,
                        // How much of the history the client still shares
                        // with something this session stored. On a rebuild
                        // boundary the provider rewrites from message 0
                        // and this pass is free; on a history rewrite it
                        // may not be, and nothing in the log said which.
                        // Upper bound only — the agreement is on what the
                        // client sent, and the provider cached what we
                        // forwarded. `-1` where there is no tracker, which
                        // is the case with nothing cached to lose.
                        agreed_prefix_len = replay_original_messages
                            .as_deref()
                            .and_then(|m| {
                                state
                                    .replay_store
                                    .agreed_prefix_len(request_lane_key, m)
                            })
                            .map_or(-1_i64, |n| n as i64),
                        // The head as the provider actually holds it. The
                        // field above compares the client's originals; this
                        // one compares what we last sent, which is what got
                        // cached. Where the two disagree this is the one
                        // that decides whether the drop costs anything.
                        forwarded_agreement_len =
                            forwarded_agreement.map_or(-1_i64, |n| n as i64),
                        incoming_msgs = replay_original_messages
                            .as_deref()
                            .map_or(-1_i64, |m| m.len() as i64),
                        "dropped thinking from assistant turns before the last"
                    );
                    match serde_json::to_vec(&value) {
                        Ok(bytes) => axum::body::Bytes::from(bytes),
                        Err(_) => buffered,
                    }
                }
            }
            Err(_) => buffered,
        }
    } else {
        buffered
    };

    (buffered, history_rewritten, offload_boundary)
}

/// Collects a buffered upstream-error body, logging the provider reason.
///
/// Non-SSE error statuses only. Hands the same bytes on unchanged; on an
/// unreadable body hands back empty. Extracted from `forward_http` without
/// behavior change.
pub(crate) async fn collect_error_body<S>(
    resp_stream: S,
    status: StatusCode,
    outcome_ctx: &Option<OutcomeContext>,
    request_id: &str,
    path_for_log: &str,
) -> Body
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
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
                emit_failed_http_outcome(ctx, request_id, status, Some(&body_bytes));
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
                emit_failed_http_outcome(ctx, request_id, status, None);
            }
            Body::empty()
        }
    }
}

/// CCR/memory alternation loop over a buffered success body.
///
/// Alternates retrieval and memory resolution until a whole pass changes
/// nothing. Returns . Extracted from
/// without behavior change.
/// What the memory half of the alternation needs: the tool context, and the
/// provider whose dialect the calls come back in.
pub(crate) struct MemoryRound<'a> {
    pub(crate) ctx: Option<MemoryToolContext>,
    pub(crate) provider: Option<&'a str>,
}

pub(crate) async fn resolve_ccr_memory_rounds(
    mut body_bytes: bytes::Bytes,
    state: &AppState,
    request_id: &str,
    path_for_log: &str,
    continuation_base: &bytes::Bytes,
    upstream: UpstreamCall<'_>,
    memory: MemoryRound<'_>,
) -> (bytes::Bytes, CcrRoundUsage) {
    let UpstreamCall {
        client: upstream_client,
        url: upstream_url,
        headers: outgoing_headers,
    } = upstream;
    let MemoryRound {
        ctx: memory_ctx,
        provider: memory_provider,
    } = memory;
    let mut ccr_round_usage = CcrRoundUsage::default();
    for _ in 0..MAX_RESOLVER_ALTERNATIONS {
        let before = body_bytes.clone();

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
            if let (Some(ccr_provider), Some(offload)) = (ccr_provider, state.ctx_offload.as_ref())
            {
                let ccr_store = offload.store.ccr();
                let ccr_stores = offload.store.stores();
                let (resolved, extra) = handle_ccr_response(
                    &body_bytes,
                    continuation_base,
                    upstream_url,
                    upstream_client,
                    ccr_store.as_ref(),
                    Some(&ccr_stores),
                    &state.config,
                    request_id,
                    outgoing_headers,
                    ccr_provider,
                    // Anthropic path: redaction lives on routed
                    // translate paths only.
                    None,
                )
                .await;
                body_bytes = resolved;
                ccr_round_usage.absorb(extra);
            }
        }

        if let (Some(memory), Some(provider)) = (memory_ctx.as_ref(), memory_provider) {
            let (resolved, extra) = handle_memory_response(
                &body_bytes,
                continuation_base,
                upstream_url,
                upstream_client,
                memory,
                &state.config,
                request_id,
                outgoing_headers,
                provider,
                None,
            )
            .await;
            body_bytes = resolved;
            ccr_round_usage.absorb(extra);
        }

        if body_bytes == before {
            break;
        }
    }
    (body_bytes, ccr_round_usage)
}

/// Post-response turn hooks over a buffered success body.
///
/// Inert unless hooks are registered. Returns (body, hook_usage).
/// Extracted from forward_http without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_buffered_post_hooks(
    body_bytes: bytes::Bytes,
    request_id: &str,
    path_for_log: &str,
    original_buffered: &bytes::Bytes,
    outcome_ctx: &Option<OutcomeContext>,
    upstream_url: &Url,
    upstream_client: &reqwest::Client,
    outgoing_headers: &HeaderMap,
) -> (bytes::Bytes, TurnHookUsage) {
    if !body_bytes.is_empty() && !crate::turn_hooks::registered_turn_hooks().is_empty() {
        let provider = if path_for_log.contains("/v1/messages") {
            "anthropic"
        } else {
            "openai"
        };
        // The outcome block parses usage by the finer label, and
        // the hook own calls have to be read the same way.
        let usage_provider = outcome_ctx
            .as_ref()
            .map(|c| c.provider.clone())
            .unwrap_or_else(|| provider.to_string());
        let (hooked, hook_usage) = apply_response_hooks(
            body_bytes,
            original_buffered,
            provider,
            &usage_provider,
            upstream_url,
            upstream_client,
            outgoing_headers,
            request_id,
        )
        .await;
        (hooked, hook_usage)
    } else {
        (body_bytes, TurnHookUsage::default())
    }
}

/// Stores a buffered success body in the semantic cache.
///
/// No-op unless the cache is configured and the original request was
/// non-streaming with a non-empty body. Extracted from forward_http
/// without behavior change.
pub(crate) fn store_semantic_cache_response(
    state: &AppState,
    original_buffered: &bytes::Bytes,
    body_bytes: &bytes::Bytes,
    resp_headers: &HeaderMap,
    request_id: &str,
) {
    if let Some(ref cache) = state.semantic_cache {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(original_buffered) {
            let is_streaming = parsed
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !is_streaming && !body_bytes.is_empty() {
                let model = parsed
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");
                let response_headers: std::collections::HashMap<String, String> = resp_headers
                    .iter()
                    .filter_map(|(k, v)| {
                        v.to_str()
                            .ok()
                            .map(|val| (k.as_str().to_string(), val.to_string()))
                    })
                    .collect();
                // Same key derivation as the lookup above; the two
                // have to move together or the cache stops hitting.
                if let Some((messages, extra)) = crate::semantic_cache::cache_key_inputs(&parsed) {
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
}

/// Records the buffered non-streaming outcome.
///
/// Parses the buffered usage block and emits the same outcome the SSE
/// sites build. Extracted from forward_http without behavior change.
pub(crate) fn emit_buffered_outcome(
    outcome_ctx: &Option<OutcomeContext>,
    body_bytes: &bytes::Bytes,
    ccr_round_usage: &CcrRoundUsage,
    turn_hook_usage: &TurnHookUsage,
    status: StatusCode,
    request_id: &str,
) {
    // Non-streaming outcome recording. The SSE state machine
    // emits a `RequestOutcome` at stream close for streaming
    // responses; the buffered (non-streaming) path had no
    // equivalent, so PERF/savings/cost/cache metrics were all
    // silently dropped for backend-routed non-streaming traffic.
    // Parse the buffered body's `usage` block (shape depends on
    // provider) and emit the same outcome the SSE sites build.
    if let Some(ref ctx) = outcome_ctx {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
            let usage = parsed.get("usage");
            let get_i64 = |u: Option<&serde_json::Value>, key: &str| -> i64 {
                u.and_then(|v| v.get(key))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            };
            // (attempted_input, output, cache_read, cache_write)
            let (attempted_input, output_tok, cache_read, cache_write) = match ctx.provider.as_str()
            {
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
            observe_proactive_expansion_cache_write(ctx, u64::try_from(cache_write).unwrap_or(0));
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
                (turn_hook_usage.input_tokens - turn_hook_usage.cache_read_tokens).max(0)
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
                request_id: request_id.to_owned(),
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
                conversation_key: ctx.conversation_key.clone(),
                conversation_tokens_saved: Some(ctx.tokens_saved),
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
            headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
        }
    }
}

/// Wraps a buffered success body: CCR resynthesis or plain passthrough.
///
/// On a flipped buffered-CCR turn resynthesizes SSE (or fails closed);
/// otherwise hands the bytes on unchanged. Mutates status and headers on
/// the resynth paths. Extracted from forward_http without behavior change.
pub(crate) fn wrap_buffered_success_body(
    body_bytes: bytes::Bytes,
    buffered_responses_ccr: bool,
    status: &mut StatusCode,
    resp_headers: &mut HeaderMap,
    request_id: &str,
) -> Body {
    if buffered_responses_ccr {
        // The client asked for a stream but upstream was called
        // buffered so CCR could resolve. Resynthesize SSE — or
        // fail closed, never handing the client an unanswerable
        // `headroom_retrieve` call.
        match serde_json::from_slice::<serde_json::Value>(&body_bytes) {
            Ok(json) => {
                let unresolved =
                    headroom_core::ccr::response_handler::CCRResponseHandler::new(None)
                        .has_ccr_tool_calls(&json, "openai_responses");
                if unresolved {
                    // Handling above did not fully resolve the
                    // retrieve call (max rounds, or mixed with a
                    // client tool call). Fail closed rather than
                    // stream a call the client cannot act on.
                    tracing::warn!(
                        request_id = %request_id,
                        event = "buffered_responses_ccr_unresolved",
                        "buffered streaming Responses reply still contains headroom_retrieve after handling; failing closed"
                    );
                    *status = StatusCode::BAD_GATEWAY;
                    resp_headers.remove(http::header::CONTENT_TYPE);
                    resp_headers.remove(http::header::CONTENT_LENGTH);
                    resp_headers.insert(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("text/event-stream"),
                    );
                    Body::from(crate::openai_buffered_ccr::openai_sse_error_event(
                        "server_error",
                        "Unable to safely complete streamed CCR retrieval.",
                    ))
                } else {
                    resp_headers.remove(http::header::CONTENT_TYPE);
                    resp_headers.remove(http::header::CONTENT_LENGTH);
                    resp_headers.insert(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("text/event-stream"),
                    );
                    let frames = crate::openai_buffered_ccr::responses_json_to_sse(&json);
                    let mut out =
                        bytes::BytesMut::with_capacity(frames.iter().map(|f| f.len()).sum());
                    for f in &frames {
                        out.extend_from_slice(f);
                    }
                    Body::from(out.freeze())
                }
            }
            Err(_) => {
                tracing::warn!(
                    request_id = %request_id,
                    event = "buffered_responses_ccr_malformed",
                    body_bytes = body_bytes.len(),
                    "rejecting malformed buffered Responses 200 reply"
                );
                *status = StatusCode::BAD_GATEWAY;
                resp_headers.remove(http::header::CONTENT_TYPE);
                resp_headers.remove(http::header::CONTENT_LENGTH);
                resp_headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                Body::from(crate::openai_buffered_ccr::openai_json_error_body(
                    crate::openai_buffered_ccr::GENERIC_FAILURE_MESSAGE,
                ))
            }
        }
    } else {
        Body::from(body_bytes)
    }
}

/// Answers a non-streaming client from an SSE upstream body.
///
/// Reads the event stream to its end; on an incomplete stream answers 502.
/// Mutates status and headers. Extracted from forward_http without
/// behavior change.
pub(crate) async fn reframe_sse_as_message<S, E>(
    resp_stream: S,
    request_id: &str,
    status: &mut StatusCode,
    resp_headers: &mut HeaderMap,
) -> Body
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    match buffer_sse_as_message(resp_stream, request_id).await {
        Ok(json) => {
            resp_headers.remove(http::header::CONTENT_TYPE);
            resp_headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            resp_headers.remove(http::header::CONTENT_LENGTH);
            Body::from(json)
        }
        Err(e) => {
            tracing::warn!(
                request_id = %request_id,
                error = %e,
                event = "upstream_protocol_error",
                "upstream sent an event stream for a non-streaming request                  and it did not complete"
            );
            *status = StatusCode::BAD_GATEWAY;
            resp_headers.remove(http::header::CONTENT_TYPE);
            resp_headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            resp_headers.remove(http::header::CONTENT_LENGTH);
            Body::from(
                serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "upstream_protocol_error",
                        "message": "upstream answered a non-streaming request                                     with an incomplete event stream",
                    }
                })
                .to_string(),
            )
        }
    }
}

/// Assembles the teed upstream response stream.
///
/// Builds the retry-wrapped body, reframes buffered-Responses SSE, and runs
/// the streamed CCR rewrite. Returns (stream, is_sse, sse_kind, round usage).
/// Extracted from forward_http without behavior change.
/// Where an upstream request goes and with what headers.
///
/// The verb rides alongside rather than inside: the CCR continuation rounds
/// are POST by construction and never have a method in hand to pass.
#[derive(Clone, Copy)]
pub(crate) struct UpstreamCall<'a> {
    pub(crate) client: &'a reqwest::Client,
    pub(crate) url: &'a Url,
    pub(crate) headers: &'a HeaderMap,
}

/// Everything the stream assembly reads besides the upstream response and
/// the bytes already peeled off it.
pub(crate) struct ResponseStreamCtx<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) request_id: &'a str,
    pub(crate) path_for_log: &'a str,
    pub(crate) slow_upstream_probe: Option<crate::upstream_route_probe::SlowUpstreamProbe>,
    pub(crate) status: StatusCode,
    pub(crate) is_sse: bool,
    pub(crate) sse_kind: SseStreamKind,
    pub(crate) buffered_responses_ccr: bool,
    pub(crate) forwarded_body: &'a Option<bytes::Bytes>,
    pub(crate) original_buffered: &'a bytes::Bytes,
    pub(crate) upstream: UpstreamCall<'a>,
    pub(crate) reqwest_method: &'a reqwest::Method,
    pub(crate) headers_snapshot: &'a Option<HeaderMap>,
}

#[allow(clippy::type_complexity)]
pub(crate) async fn assemble_response_stream(
    upstream_resp: reqwest::Response,
    sse_prefix: bytes::Bytes,
    retry_body: Option<bytes::Bytes>,
    ctx: ResponseStreamCtx<'_>,
) -> (
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    bool,
    SseStreamKind,
    Option<Arc<Mutex<CcrRoundUsage>>>,
    bytes::Bytes,
) {
    let ResponseStreamCtx {
        state,
        request_id,
        path_for_log,
        slow_upstream_probe,
        status,
        is_sse,
        sse_kind,
        buffered_responses_ccr,
        forwarded_body,
        original_buffered,
        upstream,
        reqwest_method,
        headers_snapshot,
    } = ctx;
    let UpstreamCall {
        client: upstream_client,
        url: upstream_url,
        headers: outgoing_headers,
    } = upstream;
    let ccr_stream_eligible = is_sse
        && status.is_success()
        && matches!(sse_kind, SseStreamKind::Anthropic)
        && state.config.ccr_handle_responses
        && state.ctx_offload.is_some()
        && path_for_log.contains("/v1/messages");
    let upstream_body = assemble_upstream_body(
        upstream_resp,
        sse_prefix,
        retry_body,
        is_sse,
        status,
        UpstreamBodyRetry {
            enabled: state.config.retry_enabled,
            hold_bytes: state.config.retry_stream_hold_bytes,
            max_attempts: state.config.retry_max_attempts,
            client: upstream_client.clone(),
            method: reqwest_method.clone(),
            url: upstream_url.to_string(),
            headers: outgoing_headers.clone(),
            request_id: request_id.to_owned(),
            base_delay_ms: state.config.retry_base_delay_ms,
            max_delay_ms: state.config.retry_max_delay_ms,
        },
        slow_upstream_probe,
    );
    let (upstream_body, is_sse, sse_kind) = reframe_buffered_responses_sse(
        upstream_body,
        is_sse,
        sse_kind,
        buffered_responses_ccr,
        status,
        request_id,
    )
    .await;
    // What every continuation round appends to: the bytes the provider saw and
    // cached, falling back to the client own body on the passthrough branch,
    // which forwards nothing of its own. Shared by the streamed CCR rewrite
    // above and the buffered CCR/memory rounds below.
    let continuation_base: bytes::Bytes = forwarded_body
        .clone()
        .unwrap_or_else(|| original_buffered.clone());
    let (upstream_body, ccr_round_usage) = maybe_rewrite_anthropic_stream(
        upstream_body,
        ccr_stream_eligible,
        continuation_base.clone(),
        original_buffered.clone(),
        state,
        upstream_client,
        upstream_url,
        outgoing_headers,
        headers_snapshot,
        request_id,
    )
    .await;
    (
        upstream_body,
        is_sse,
        sse_kind,
        ccr_round_usage,
        continuation_base,
    )
}

/// Builds the outcome context for SSE-close emission.
///
/// Re-parses the buffered body for model and message counts. Returns the
/// populated context. Extracted from forward_http without behavior change.
/// What the compression stage charged this turn, in the shape the outcome
/// record wants it.
#[derive(Clone, Copy)]
pub(crate) struct CompressionTotals<'a> {
    pub(crate) tokens_before: i64,
    pub(crate) tokens_saved: i64,
    pub(crate) strategies: &'a [String],
    pub(crate) proactive_expansion_applied: bool,
}

pub(crate) fn build_outcome_context(
    buffered: &bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    headers_snapshot: &Option<HeaderMap>,
    state: &AppState,
    start: &Instant,
    totals: CompressionTotals<'_>,
    tags: std::collections::HashMap<String, String>,
) -> OutcomeContext {
    let CompressionTotals {
        tokens_before: compress_tokens_before,
        tokens_saved: compress_tokens_saved,
        strategies: compress_strategies,
        proactive_expansion_applied,
    } = totals;
    // Build OutcomeContext for emit_request_outcome at SSE stream close.
    // Re-parses `buffered` for model/num_messages (cheap, happens once).
    {
        let parsed_body: serde_json::Value = serde_json::from_slice(buffered).unwrap_or_default();
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
        let num_messages = message_array_length(buffered, endpoint).unwrap_or(0);

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
            project_root_override: state.config.memory_project_root.clone(),
        };
        let project = crate::memory::router::ProjectResolver::resolve(&project_ctx)
            .map(|(key, _display)| key);

        // Set thread-local project context for downstream modules
        // (compression feedback, audit, output shaping).
        crate::project_context::set_current_project(project.as_deref());

        // Taken before the struct literal moves `model`.
        let model_for_waste = model.clone();

        OutcomeContext {
            sink: Arc::new(ProxyOutcomeSink {
                cost_tracker: state.cost_tracker.clone(),
                savings_tracker: state.savings_tracker.clone(),
                request_logger: state.request_logger.clone(),
            }),
            model,
            provider: provider_label.to_string(),
            tags,
            client: None,
            project,
            // Keep PERF savings scoped to the compression dispatcher. CTX
            // offload has its own per-request accounting below; folding it
            // into this field makes a prior offload re-application look like
            // compression savings and can re-emit a conversation-sized value.
            original_tokens: compress_tokens_before,
            tokens_saved: compress_tokens_saved,
            transforms_applied: compress_strategies.to_vec(),
            num_messages: num_messages as i64,
            total_latency_ms: start.elapsed().as_millis() as f64,
            // Set once compression has run; see the `stage_timer.record`
            // call below.
            overhead_ms: 0.0,
            started_at: *start,
            waste_signals: waste_signals_for_request(&parsed_body, &model_for_waste),
            proactive_expansion_applied,
            // Filled in at the send point, where the final body exists.
            wire_bytes: None,
            forwarded_tokens_estimate: 0,
            upstream_attempts: 1,
            // Derived from the client's own body, before compression:
            // a rewritten first user message must not move the key
            // mid-conversation. `None` on every shape but
            // whole-transcript `/v1/responses`, where the booked
            // per-turn diff is the conversation's running total.
            conversation_key: headroom_core::conversation_savings::savings_conversation_key(
                &parsed_body,
                headers_snapshot.as_ref().and_then(|headers| {
                    headers
                        .get("conversation_id")
                        .or_else(|| headers.get("session_id"))
                        .or_else(|| headers.get("x-headroom-session-id"))
                        .and_then(|v| v.to_str().ok())
                }),
            ),
        }
    }
}

/// Records probe and feedback observations for a compression outcome.
///
/// No-op unless the recorder/feedback stores are configured and tokens
/// moved. Extracted from forward_http without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_compression_observations(
    state: &AppState,
    request_id: &str,
    endpoint: compression::CompressibleEndpoint,
    buffered: &bytes::Bytes,
    start: &Instant,
    compress_tokens_before: i64,
    compress_tokens_saved: i64,
    compress_strategies: &[String],
) {
    if let Some(ref recorder) = state.probe_recorder {
        if compress_tokens_before > 0 {
            let event = crate::probe_recorder::CompressionEvent {
                ts: start.elapsed().as_secs_f64(),
                request_id: request_id.to_owned(),
                provider: endpoint_str(&endpoint).to_string(),
                model: String::new(),
                tokens_before: Some(compress_tokens_before as u64),
                tokens_after: Some((compress_tokens_before - compress_tokens_saved) as u64),
                transforms_applied: compress_strategies.to_vec(),
            };
            recorder.record(&event);
        }
    }
    // Compression feedback: record per-tool compression patterns for learning.
    if let Some(ref feedback) = state.compression_feedback {
        if compress_tokens_saved > 0 {
            let tool_name = extract_tool_name(buffered, endpoint);
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
}

/// Consumes the compression outcome into the body to send.
///
/// Maps outcome to bytes plus the passthrough-bytes alarm. Returns the body.
/// Extracted from forward_http without behavior change.
pub(crate) fn consume_compression_outcome(
    outcome: compression::Outcome,
    buffered: bytes::Bytes,
    outcome_is_passthrough_class: bool,
    original_buffered_len: usize,
    state: &AppState,
    request_id: &str,
    path_for_log: &str,
) -> bytes::Bytes {
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
                request_id,
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
            emit_strategy_ratio_samples(
                &per_strategy_tokens,
                tokens_before,
                tokens_after,
                &strategies_applied,
                request_id,
                path_for_log,
            );
            body
        }
        compression::Outcome::Passthrough { reason } => {
            tracing::warn!(
                event = "compression_passthrough_parse",
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
        crate::observability::record_passthrough_bytes_modified(path_for_log, delta, request_id);
    }
    body_to_send
}

/// Emits per-strategy compression-ratio samples for a Compressed outcome.
///
/// Per-strategy tokens when present, else one aggregate sample.
/// Extracted from consume_compression_outcome without behavior change.
pub(crate) fn emit_strategy_ratio_samples(
    per_strategy_tokens: &[compression::PerStrategyTokens],
    tokens_before: usize,
    tokens_after: usize,
    strategies_applied: &[&str],
    request_id: &str,
    path_for_log: &str,
) {
    if !per_strategy_tokens.is_empty() {
        for entry in per_strategy_tokens {
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
            "emitting one aggregate-labelled compression_ratio sample because              the dispatcher did not surface per-strategy token counts              (Phase E normalization paths)"
        );
        crate::observability::observe_compression_ratio(
            "aggregate",
            "aggregate",
            tokens_before,
            tokens_after,
        );
    }
}

/// Spawns the SSE parser tee and wraps the stream for the client.
///
/// Returns the client-facing response stream. Extracted from forward_http
/// without behavior change.
pub(crate) fn tee_response_stream<S>(
    upstream_body: S,
    sse_kind: SseStreamKind,
    state: &AppState,
    outcome_ctx: Option<OutcomeContext>,
    ccr_round_usage: Option<Arc<Mutex<CcrRoundUsage>>>,
    status: StatusCode,
    request_id: &str,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + use<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send,
{
    let rid = request_id.to_owned();
    let parser_telemetry = std::sync::Arc::new(ParserTelemetry::default());
    let parser_tx = spawn_sse_parser_tee(
        sse_kind,
        state,
        outcome_ctx,
        ccr_round_usage,
        status,
        request_id,
        &parser_telemetry,
    );
    tee_stream_to_parser(upstream_body, parser_tx, parser_telemetry, rid)
}

/// Wraps the final streaming body: SSE finish or plain passthrough.
///
/// Last stop before the client on the streaming arms. Extracted from
/// forward_http without behavior change.
pub(crate) fn wrap_streaming_body<S>(
    resp_stream: S,
    is_sse: bool,
    status: StatusCode,
    sse_kind: SseStreamKind,
    request_id: &str,
) -> Body
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    if is_sse && status.is_success() && matches!(sse_kind, SseStreamKind::Anthropic) {
        // Last stop before the client. retry_on_early_drop above saves the
        // drops it can still take back; past that point the only thing left to
        // save is the shape of the reply. This closes an interrupted message
        // properly - marked as truncated - so a dead connection costs one
        // short answer instead of the session. It runs above the telemetry
        // tee, so accounting still books the turn as the incomplete one it was.
        // track_streaming keeps the drain check nonzero until the last byte:
        // without it the pipeline guard already dropped and a rotation reads
        // idle mid-stream.
        Body::from_stream(track_streaming(
            crate::sse::stream_finisher::finish_on_drop(resp_stream, request_id.to_owned()),
        ))
    } else {
        Body::from_stream(track_streaming(resp_stream))
    }
}

/// Builds the final HTTP response with headers, timing log, and outcome.
///
/// Extracted from forward_http without behavior change.
/// The three pieces of the response as the upstream arms left them.
pub(crate) struct FinalResponseParts {
    pub(crate) status: StatusCode,
    pub(crate) resp_headers: HeaderMap,
    pub(crate) body: Body,
}

/// The ids the upstream gave this turn. Three, not one: a routed provider
/// answers under its own id, and a support ticket needs whichever one the
/// provider recognises.
pub(crate) struct UpstreamRequestIds<'a> {
    pub(crate) generic: &'a Option<String>,
    pub(crate) anthropic: &'a Option<String>,
    pub(crate) openai: &'a Option<String>,
}

/// The clocks the access-log line reports: whole-request latency and the
/// per-stage breakdown.
#[derive(Clone, Copy)]
pub(crate) struct ResponseTiming<'a> {
    pub(crate) start: &'a Instant,
    pub(crate) stage_timer: &'a crate::stage_timer::StageTimer,
}

pub(crate) fn build_final_response(
    parts: FinalResponseParts,
    request_id: &str,
    method: &axum::http::Method,
    path_for_log: &str,
    timing: ResponseTiming<'_>,
    inflight: &InflightGuard,
    upstream_ids: UpstreamRequestIds<'_>,
) -> Result<Response<Body>, ProxyError> {
    let FinalResponseParts {
        status,
        resp_headers,
        body,
    } = parts;
    let ResponseTiming { start, stage_timer } = timing;
    let UpstreamRequestIds {
        generic: upstream_request_id,
        anthropic: upstream_request_id_anthropic,
        openai: upstream_request_id_openai,
    } = upstream_ids;
    // One observation per upstream response, whatever its status. The refusal
    // count alone is the number that let a 22.5% rejection rate pass for
    // ordinary bad luck; the ratio is what makes it obvious.
    crate::observability::upstream_health::observe_upstream_response(status.as_u16());
    let mut response = Response::builder().status(status);
    {
        let h = response.headers_mut().expect("builder has headers");
        h.extend(resp_headers);
        // Echo X-Request-Id back to the client.
        if let Ok(v) = http::HeaderValue::from_str(request_id) {
            h.insert(HeaderName::from_static("x-request-id"), v);
        }
        // PR-A8 / P5-57: surface the upstream id in a distinct
        // header so it never conflated with the proxy own.
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

        upstream_request_id_anthropic = upstream_request_id_anthropic.as_deref().unwrap_or(""),

        upstream_request_id_openai = upstream_request_id_openai.as_deref().unwrap_or(""),

        method = %method,
        path = %path_for_log,
        upstream_status = status.as_u16(),
        latency_ms = start.elapsed().as_millis() as u64,
        protocol = "http",
        "forwarded"
    );
    // Emit stage timings for observability.
    crate::stage_timer::emit_stage_timings_log(
        path_for_log,
        request_id,
        "",
        stage_timer,
        &[
            "buffer",
            "parse",
            "memory",
            "compression",
            "replay",
            "rewrite",
            "footprint",
            "post",
            "pre_forward",
            "upstream",
        ],
        inflight.count(),
    );
    Ok(response)
}

/// Dispatches the compression run by endpoint.
///
/// Runs the Anthropic / OpenAI-chat / OpenAI-responses compressors with
/// dedup post-passes. Returns the outcome. Extracted from forward_http
/// without behavior change.
pub(crate) fn dispatch_compression(
    buffered: &bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    effective_auth_mode: AuthMode,
    auth_mode: AuthMode,
    request_id: &str,
) -> compression::Outcome {
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
                buffered,
                state.config.compression_mode,
                state.config.cache_control_auto_frozen,
                effective_auth_mode,
                request_id,
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
                buffered,
                &state.config,
                "/v1/messages",
                request_id,
            )
        }
        compression::CompressibleEndpoint::OpenAiChatCompletions => {
            let skip = compression::should_skip_compression(buffered);
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
                    buffered,
                    state.config.compression_mode,
                    auth_mode,
                    request_id,
                    &state.config.exclude_tools,
                );
                // Cross-turn verbatim de-dup post-pass over
                // `role == "tool"` message content (no-op unless
                // `--enable-cross-turn-dedup` is set).
                //
                // Folding rewrites a repeated span to a bare
                // `[↑NL same as msg M]` pointer, which only helps if
                // the model can resolve the reference. On the
                // streaming chat path it cannot: this path does not
                // intercept tool calls, so the retrieval tool is never
                // injected, and OpenAI-compatible clients never show
                // the model numbered messages. The pointer then reads
                // as deleted content and models retry-loop on output
                // they think went missing.
                //
                // Same predicate that gates the retrieval tool itself
                // upstream in this function; recomputed because that
                // binding is out of scope by here.
                let client_streams = serde_json::from_slice::<serde_json::Value>(buffered)
                    .ok()
                    .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
                    .unwrap_or(false);
                if client_streams {
                    tracing::debug!(
                        event = "cross_turn_dedup_skipped",
                        request_id = %request_id,
                        path = "/v1/chat/completions",
                        reason = "pointers_unrecoverable_on_stream",
                        "skipping cross-turn dedup: a folded pointer cannot be \
                         resolved on the streaming chat path"
                    );
                    outcome
                } else {
                    compression::apply_cross_turn_dedup(
                        outcome,
                        buffered,
                        &state.config,
                        "/v1/chat/completions",
                        request_id,
                    )
                }
            }
        }
        // PR-C3: OpenAI Responses (`/v1/responses`). The Responses
        // dispatcher walks an explicitly-typed `input` array and
        // only rewrites the latest of each compressible `*_output`
        // kind plus the latest `message` text. Cache hot zone is
        // every other item type (passthrough verbatim).
        compression::CompressibleEndpoint::OpenAiResponses => {
            compression::compress_openai_responses_request(
                buffered,
                state.config.compression_mode,
                auth_mode,
                request_id,
                &state.config.exclude_tools,
            )
        }
    }
}

/// Refines the compression decision once the body is parsed.
///
/// Validates the message array size and re-runs the decision with the
/// known has_messages flag. Returns the decision. Extracted from
/// forward_http without behavior change.
pub(crate) fn refine_compression_decision(
    buffered: &bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    headers_snapshot: &Option<HeaderMap>,
    state: &AppState,
    request_id: &str,
) -> Result<(crate::compression_decision::CompressionDecision, HeaderMap), ProxyError> {
    let empty_headers = HeaderMap::new();
    let decision_headers = headers_snapshot.as_ref().unwrap_or(&empty_headers);
    let has_messages = request_has_messages(buffered, endpoint);
    // Validate message array size (mirrors Python MAX_MESSAGE_ARRAY_LENGTH).
    if let Some(count) = message_array_length(buffered, endpoint) {
        if count > MAX_MESSAGE_ARRAY_LENGTH {
            tracing::warn!(
                event = "request_message_array_too_large",
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
        true, // license_allows — no licensing in this binary; see the gate
        has_messages,
    );
    Ok((decision, decision_headers.clone()))
}

/// Applies pre-replay holds and reasoning strips.
///
/// System holds plus unsigned/signed reasoning-block drops on Anthropic
/// messages; other endpoints pass through. Extracted from forward_http
/// without behavior change.
pub(crate) fn run_prereplay_holds(
    body_to_send: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    request_lane_key: &str,
) -> bytes::Bytes {
    // Hold the working-directory line in system still. Runs BEFORE prefix
    // replay so the note it adds is part of the tail the overlay stores.
    let body_to_send = if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        apply_system_holds_to_bytes(state, body_to_send, request_lane_key, request_id)
    } else {
        body_to_send
    };
    // Ahead of prefix replay, which parks the forwarded message array in
    // the replay store to overlay onto the next turn. Stripping after that
    // would leave the store holding a block that never went on the wire,
    // and every later turn would overlay it back in - the proxy idea of
    // the cached prefix drifting from the provider, which is the shape
    // of a cache_recache_observed mismatch.
    if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) {
        // Both stages remove a thinking block Anthropic would refuse, for
        // two unrelated reasons - a stream that died before the signature
        // arrived, and a signature this proxy wrote on a routed turn - so
        // they count and log separately.
        drop_headroom_signed_reasoning_blocks(
            drop_unsigned_reasoning_blocks(body_to_send, request_id),
            request_id,
        )
    } else {
        body_to_send
    }
}

/// Runs the prefix-replay overlay over the compressed body.
///
/// Replays the previously-forwarded prefix byte-identical when this turn
/// append-only-extends it, with a cold-prefix recompaction fork. Records
/// the replay stage timing. Returns the body. Extracted from forward_http
/// without behavior change.
pub(crate) fn run_prefix_replay(
    body_to_send: bytes::Bytes,
    replay_original_messages: Option<Vec<serde_json::Value>>,
    scope: RequestScope<'_>,
    request_lane_key: &str,
    outcome_ctx: &mut Option<OutcomeContext>,
    stage_timer: &mut crate::stage_timer::StageTimer,
    replay_start: &Instant,
) -> bytes::Bytes {
    let RequestScope {
        state,
        request_id,
        headers_snapshot,
        ..
    } = scope;
    let body_to_send = match (replay_original_messages, headers_snapshot.as_ref()) {
        // `_headers` is matched, not used: the key was derived once above
        // from the unmutated body. The arm still guards on `Some` because
        // `request_lane_key` is empty without headers, which would key
        // every session into one replay slot.
        (Some(original_messages), Some(_headers)) => {
            let session_key = request_lane_key;
            // Cold-prefix fork (upstream `HEADROOM_COLD_RECOMPACT`,
            // default off): a lane idle past its real TTL holds a dead
            // prefix, so the byte-identical splice below preserves
            // nothing — skip it, recompacing losslessly instead in
            // cache mode. Warm lanes (and the default flag) fall
            // through to the normal replay untouched. Recompaction runs
            // on the post-compression messages, so compression savings
            // are kept and only lossless whole-prefix folds add on top.
            let cold_fork = (|| {
                use headroom_core::transforms::cold_prefix as cp;
                if !cp::cold_recompact_enabled() {
                    return None;
                }
                let mut parsed: serde_json::Value = serde_json::from_slice(&body_to_send).ok()?;
                let messages = parsed.get("messages")?.as_array()?.clone();
                let system = parsed.get("system").cloned();
                let model = outcome_ctx
                    .as_ref()
                    .map(|ctx| ctx.model.as_str())
                    .unwrap_or("");
                let fork = maybe_cold_fork(
                    true,
                    crate::modes::is_cache_mode(Some(&state.config.mode)),
                    model,
                    &messages,
                    system.as_ref(),
                    state.replay_store.idle_seconds(session_key),
                    state.ccr_store(),
                )?;
                tracing::info!(
                    event = "cold_prefix_recompaction",
                    request_id = %request_id,
                    cc_cache_ttl = %fork.ttl_desc,
                    idle_secs = format!("{:.0}", fork.idle_secs),
                    "cold-prefix recompaction: cache lapsed — recompacting whole prefix"
                );
                if let Some(ctx) = outcome_ctx.as_mut() {
                    ctx.transforms_applied
                        .extend(fork.transforms.iter().cloned());
                }
                parsed["messages"] = serde_json::Value::Array(fork.messages);
                serde_json::to_vec(&parsed).ok().map(bytes::Bytes::from)
            })();
            if let Some(bytes) = cold_fork {
                bytes
            } else {
                apply_prefix_replay(
                    &state.replay_store,
                    session_key,
                    request_id,
                    original_messages,
                    body_to_send,
                    Some(&state.usage_observer),
                    state.started_at.elapsed().as_secs(),
                    state.config.cache_tail_breakpoints as usize,
                    state.config.strip_system_cache_breakpoints,
                )
            }
        }
        _ => body_to_send,
    };
    stage_timer.record("replay", replay_start.elapsed().as_secs_f64() * 1000.0);
    body_to_send
}

/// Runs the endpoint rewrite match over the compressed body.
///
/// OpenAI prompt-cache-key injection or the Anthropic rewrite pipeline.
/// Returns the body. Extracted from forward_http without behavior change.
pub(crate) fn run_endpoint_rewrite(
    body_to_send: bytes::Bytes,
    scope: RequestScope<'_>,
    path_for_log: &str,
    auth_mode: AuthMode,
    selected_upstream_base: &str,
    skip_model_routing: bool,
    outcome_ctx: &mut Option<OutcomeContext>,
) -> bytes::Bytes {
    let RequestScope {
        state,
        endpoint,
        request_id,
        ..
    } = scope;
    match endpoint {
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
                request_id,
                path_for_log,
            )
        }
        compression::CompressibleEndpoint::AnthropicMessages => rewrite_anthropic_body(
            body_to_send,
            state,
            request_id,
            selected_upstream_base,
            skip_model_routing,
            outcome_ctx,
        ),
    }
}

/// Runs the tool-shape stabilization stages.
///
/// Schema compaction, roster pin, stable tool order, and tail breakpoint
/// on Anthropic messages; other endpoints pass through. Returns the body.
/// Extracted from forward_http without behavior change.
pub(crate) fn run_tool_shape_stages(
    body_to_send: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    request_lane_key: &str,
    decision_should_compress: bool,
) -> bytes::Bytes {
    // Tool schema compaction. Runs last, once tools are final for every
    // endpoint (routing, sanitising, pruning and CCR injection are all
    // done), so what goes on the wire is what gets compacted. Byte-
    // identical passthrough when there is nothing to strip.
    //
    // Auxiliary passes honor the same disable/bypass decision as message
    // compression (upstream fb79055b): a request the decision passes
    // through must keep its tools byte-identical and accrue no
    // compaction savings, so this stage is skipped outright rather than
    // merely finding nothing to strip.
    let body_to_send = if decision_should_compress
        && !matches!(
            state.config.compression_mode,
            crate::config::CompressionMode::Off
        ) {
        maybe_compact_tool_schemas(body_to_send, request_id)
    } else {
        body_to_send
    };
    // B2 tool-order stabilization. Must follow every other tool mutation
    // above, so the order we record is the order the provider caches.
    let body_to_send = if state.config.cache_pin_tool_roster
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
        maybe_pin_tool_roster(
            body_to_send,
            &state.roster_pin_state,
            request_lane_key,
            request_id,
        )
    } else {
        body_to_send
    };
    let body_to_send = if state.config.cache_stable_tool_order
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        ) {
        maybe_stabilize_tool_order(
            body_to_send,
            &state.tool_order_state,
            request_lane_key,
            request_id,
        )
    } else {
        body_to_send
    };
    // Tail breakpoint. Before the TTL pin, so the moved marker is one of
    // the markers that pin covers.
    if state.config.cache_tail_breakpoint
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        )
    {
        maybe_push_tail_breakpoint(body_to_send, request_id)
    } else {
        body_to_send
    }
}

/// Applies the B1 cache-TTL pin on Anthropic messages.
///
/// Skips on PAYG and on explicit client-5m tiers. Returns the body.
/// Extracted from forward_http without behavior change.
pub(crate) fn run_ttl_pin(
    body_to_send: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    auth_mode: AuthMode,
) -> bytes::Bytes {
    // B1 cache-TTL pin. Last of all, so every marker any earlier stage
    // placed or moved is covered. Skipped on PAYG, where a 1h write is
    // priced 60% above a 5m one and the operator pays the difference in
    // dollars rather than in a token-counted usage window.
    //
    // Subagent passthrough: with --respect-client-5m-ttl, a body whose
    // markers are all explicitly 5m keeps the tier it asked for instead
    // of being upgraded to 1h entries that die unused. Read back from
    // the observer, which saw the client markers before any stage
    // added or moved one; a turn that never reached the gate falls back
    // to pinning.
    let pin_1h = state
        .usage_observer
        .client_ttl_for(request_id)
        .map_or(true, |shape| {
            cache_stabilization::cache_ttl::pin_1h_applies(
                shape,
                state.config.respect_client_5m_ttl,
            )
        });
    if !pin_1h {
        tracing::info!(
            event = "ttl_1h_pin_skipped",
            request_id = %request_id,
            "client asked 5m everywhere; leaving its tier alone (--respect-client-5m-ttl)"
        );
    }
    if (state.config.force_1h_cache_ttl || state.config.split_cache_ttl)
        && pin_1h
        && auth_mode != AuthMode::Payg
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        )
    {
        // The split takes precedence: pinning the moving message tail to 1h
        // buys an hour of retention for content the next turn supersedes in
        // seconds, at 2.0x base input against 5m's 1.25x.
        maybe_pin_cache_ttl(body_to_send, request_id, state.config.split_cache_ttl)
    } else {
        body_to_send
    }
}

/// Answers the spinner-text sidecar request directly when applicable.
///
/// Returns Some(response) when the body is a sidecar describe-action handled
/// without the main pipeline; None otherwise. Extracted from forward_http
/// without behavior change.
pub(crate) async fn maybe_handle_sidecar(
    buffered: &bytes::Bytes,
    uri_path: &str,
    state: &AppState,
    request_id: &str,
    upstream_client: &reqwest::Client,
    upstream_url: &Url,
    headers_snapshot: &Option<HeaderMap>,
) -> Option<Response<Body>> {
    const DESCRIBE: &[u8] = crate::sidecar::DESCRIBE_ACTION_PREFIX.as_bytes();
    if compression::classify_compressible_path(uri_path)
        == Some(compression::CompressibleEndpoint::AnthropicMessages)
        && memchr::memmem::find(buffered, DESCRIBE).is_some()
    {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(buffered) {
            let empty = http::HeaderMap::new();
            if let Some(sidecar_model) = crate::sidecar::direct_sidecar_model(&state.config) {
                if let Some(resp) = crate::sidecar::try_handle(
                    upstream_client,
                    upstream_url,
                    request_id,
                    headers_snapshot.as_ref().unwrap_or(&empty),
                    &parsed,
                    &sidecar_model,
                    crate::sidecar::SidecarRetry::from_config(&state.config),
                )
                .await
                {
                    return Some(resp);
                }
            }
        }
    }
    None
}

/// Logs the image-prefix census over the replay-original messages.
///
/// Observability only. Extracted from forward_http without behavior change.
pub(crate) fn log_image_prefix_census(
    replay_original_messages: &Option<Vec<serde_json::Value>>,
    headers_snapshot: &Option<HeaderMap>,
    request_id: &str,
    request_session_key: &str,
) {
    if let Some(messages) = replay_original_messages.as_deref() {
        let (image_blocks, collapsed_blocks, image_b64_bytes) = image_census(messages);
        if image_blocks > 0 || collapsed_blocks > 0 {
            // What makes the client let go of an image is still unknown.
            // Message count is out: one session collapsed at 235 messages
            // while another held six images past 385. Prompt size is out
            // too - 172,658 tokens at the collapse against 230,693 without
            // one - and so is the fraction of the window, since both of
            // those sessions ran the 1M context. Record the window anyway,
            // so the next collapse is judged against something written
            // down rather than remembered.
            let beta = headers_snapshot
                .as_ref()
                .and_then(|h| h.get("anthropic-beta"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            tracing::info!(
                request_id = %request_id,
                event = "image_prefix_census",
                session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(
                    request_session_key
                ),
                message_count = messages.len(),
                image_blocks,
                collapsed_blocks,
                image_b64_bytes,
                context_1m = beta.contains("context-1m"),
                anthropic_beta = %beta,
                "images the client is carrying in its prefix"
            );
        }
    }
}

/// Injects memory tool definitions into the request value.
///
/// Creates the tools array on demand; sets changed on injection.
/// Extracted from forward_http without behavior change.
pub(crate) fn inject_memory_tool_definitions(
    value: &mut serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    changed: &mut bool,
) {
    if let Some(handler) = state.memory_handler.as_ref() {
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
            // Requests without a tools array still get the
            // memory tools - create the array on demand.
            let existing: Vec<serde_json::Value> = value
                .get("tools")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let (new_tools, injected) = handler.inject_memory_tools(Some(&existing), provider);
            // Info, not debug: in tool mode this is the only
            // proof the model was ever offered memory. Without
            // it, "the tools never arrived" and "the model
            // chose not to call them" both read as an empty
            // log. Logged on both branches for that reason.
            let added = new_tools.len().saturating_sub(existing.len());
            // Only the definitions we appended, not the whole
            // array: the tools block costs about $32/day in
            // cache reads and the memory tools share of it was
            // a guess. prefix_composition already carries the
            // block total size, so this is the other half of
            // the ratio. Serialising the tail is a handful of
            // small objects; serialising the array would not be.
            let added_bytes: usize = new_tools
                .iter()
                .skip(existing.len())
                .filter_map(|t| serde_json::to_string(t).ok())
                .map(|s| s.len())
                .sum();
            if injected {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("tools".to_string(), serde_json::Value::Array(new_tools));
                    *changed = true;
                    tracing::info!(
                        request_id = %request_id,
                        event = "memory_tools_injected",
                        tools_added = added,
                        tools_total = existing.len() + added,
                        bytes_added = added_bytes,
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
}

/// One memory-continuation send attempt, classified into its loop action.
///
/// Extracted from `send_memory_continuation` without behavior change.
pub(crate) enum MemorySendOutcome {
    Done(MemorySendDone),
    Next(u32),
}

/// What a finished send attempt means for the rounds loop: a response to
/// fold, a deterministic rejection (the body will never pass — stop
/// sending it), or a transport failure (also stop; nothing was decided).
pub(crate) enum MemorySendDone {
    Sent(reqwest::Response),
    Rejected,
    Failed,
}

/// Builds and sends one memory-continuation request: fresh Zen request id per
/// send (no-op off zen routes), JSON headers, cloned body, 30s header timeout.
/// Extracted from `send_memory_continuation` without behavior change.
pub(crate) async fn send_memory_continuation_once(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    outgoing_headers: &http::HeaderMap,
    continuation_body: &[u8],
) -> Result<Result<reqwest::Response, reqwest::Error>, tokio::time::error::Elapsed> {
    // Same Zen hygiene as the CCR continuation above: fresh
    // `x-opencode-request` per send, no-op off zen routes.
    let mut continuation_headers = outgoing_headers.clone();
    crate::routed::quirks::refresh_zen_request_id(&mut continuation_headers);
    tokio::time::timeout(
        CCR_CONTINUATION_SEND_TIMEOUT,
        client
            .post(upstream_url.clone())
            .headers(crate::headers::headers_for_json_body(
                &continuation_headers,
                continuation_body,
            ))
            .body(continuation_body.to_vec())
            .send(),
    )
    .await
}

/// Handles the error-status arm of one memory-continuation attempt: logs the
/// rejected item's shape (`input[N]`/`messages[N]`) so the next schema
/// mismatch is diagnosable from the log alone.
/// Extracted from `send_memory_continuation` without behavior change.
pub(crate) async fn log_memory_send_rejection(
    r: reqwest::Response,
    request_id: &str,
    attempt: u32,
    round: usize,
    items_field: &str,
    current_request: &serde_json::Value,
) {
    let status = r.status();
    let detail = r.text().await.unwrap_or_default();
    // A 400 names the item it rejected as `input[N]` (or
    // `messages[N]`); log that item's shape so the next
    // schema mismatch is diagnosable from the log alone.
    let rejected = rejected_item_summary(&detail, current_request, items_field);
    // …but some 400s name no item at all (e.g. "the conversation must end
    // with a user message"), so also snapshot the tail roles: a trailing
    // assistant message is the whole diagnosis for that class, and without
    // this line the next one is as unreadable as the 2026-09-24 Opus
    // prefill rejections were.
    let tail_roles = continuation_tail_summary(current_request, items_field);
    tracing::warn!(
        event = "memory_continuation_rejected",
        request_id = %request_id,
        status = %status,
        attempt,
        round = round + 1,
        detail = %first_bytes(&detail, 600),
        rejected_item = %rejected,
        tail_roles = %tail_roles,
        "memory: upstream returned error during continuation"
    );
}

/// Roles (and, for Responses items, item types) of the last three entries
/// of a continuation array, most recent last. Companions
/// `rejected_item_summary` for rejections that name no item. Shared with
/// the pre-send continuation check in `proxy.rs`, which logs the same
/// shape when it skips a body that would fail the same way.
pub(crate) fn continuation_tail_summary(request: &serde_json::Value, items_field: &str) -> String {
    let items = request
        .get(items_field)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tail: Vec<String> = items
        .iter()
        .rev()
        .take(3)
        .map(|item| {
            if let Some(role) = item.get("role").and_then(|v| v.as_str()) {
                let kinds = item
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.get("type").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("+")
                    })
                    .unwrap_or_default();
                if kinds.is_empty() {
                    role.to_string()
                } else {
                    format!("{role}[{kinds}]")
                }
            } else {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string()
            }
        })
        .collect();
    format!(
        "[{}]",
        tail.iter().rev().cloned().collect::<Vec<_>>().join(",")
    )
}

/// Whether a memory-continuation HTTP status is worth another send.
///
/// 429 and 5xx are the historical retry set on every route. On the Zen
/// route only, 403 joins them. Over 2026-09-22, 4 of 8 memory continuations
/// on muse-spark-1.3-contributor-free returned 403 FreeTierError ("OpenCode's
/// free tier can only be used from within OpenCode") at attempt 0, round 1,
/// while 15 memory continuations on Anthropic-route models and 294 CCR
/// continuations (sonnet/opus; CCR never runs on Zen) all returned 200. The
/// nonce refresh (`refresh_zen_request_id`) was already live and did not
/// stop these 403s.
///
/// Ruled out that window: fallback session id (no zen_session_mint_started
/// events, so a real synced OpenCode session id went out every time) and
/// concurrency (zero other Spark requests in flight at each 403). Body
/// shape is the better-supported hypothesis: all four failures had msgs=2
/// / tok_before about 24.4k, all four passes had msgs=1 / tok_before
/// 12.4-15.8k. This retry is a cheap hedge for the 403s, not a proven
/// fix for that split.
pub(crate) fn memory_status_is_retryable(status: http::StatusCode, zen_route: bool) -> bool {
    status.as_u16() == 429 || status.is_server_error() || (zen_route && status.as_u16() == 403)
}

/// Classifies one `send_memory_continuation` attempt result: break the retry
/// loop with a response, or bump the attempt count and retry after a backoff
/// sleep. Extracted from `send_memory_continuation` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn classify_memory_send(
    sent: Result<Result<reqwest::Response, reqwest::Error>, tokio::time::error::Elapsed>,
    attempt: u32,
    request_id: &str,
    round: usize,
    items_field: &str,
    current_request: &serde_json::Value,
    zen_route: bool,
) -> MemorySendOutcome {
    match sent {
        Err(_) => {
            tracing::warn!(
                event = "memory_continuation_headers_timeout",
                request_id = %request_id,
                attempt,
                round = round + 1,
                timeout_secs = CCR_CONTINUATION_SEND_TIMEOUT.as_secs(),
                "memory: continuation timed out waiting for response headers"
            );
            MemorySendOutcome::Done(MemorySendDone::Failed)
        }
        Ok(Ok(r)) if r.status().is_success() => MemorySendOutcome::Done(MemorySendDone::Sent(r)),
        Ok(Ok(r)) => {
            let status = r.status();
            let retryable = memory_status_is_retryable(status, zen_route);
            if retryable && attempt < MEMORY_CONTINUATION_RETRIES {
                let attempt = attempt + 1;
                tokio::time::sleep(memory_continuation_backoff(attempt)).await;
                return MemorySendOutcome::Next(attempt);
            }
            log_memory_send_rejection(r, request_id, attempt, round, items_field, current_request)
                .await;
            MemorySendOutcome::Done(MemorySendDone::Rejected)
        }
        Ok(Err(e)) => {
            if attempt < MEMORY_CONTINUATION_RETRIES && is_retryable_transport_error(&e) {
                let attempt = attempt + 1;
                tokio::time::sleep(memory_continuation_backoff(attempt)).await;
                return MemorySendOutcome::Next(attempt);
            }
            tracing::warn!(
                event = "memory_continuation_send_failed",
                request_id = %request_id,
                attempt,
                round = round + 1,
                error = %e,
                "memory: upstream request failed during continuation"
            );
            MemorySendOutcome::Done(MemorySendDone::Failed)
        }
    }
}

/// Injects the `headroom_retrieve` tool definition when CCR is on.
///
/// Adds an empty `tools` array first when the body carries none but content
/// was offloaded, so the digest cannot name a tool the model never got. Sets
/// `changed` on injection. Extracted from `forward_http` without behavior
/// change.
pub(crate) fn inject_ccr_retrieve_tool(
    value: &mut serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    ctx_transform_tokens_saved: i64,
    changed: &mut bool,
) {
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
            let already_has = tools
                .iter()
                .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("headroom_retrieve"));
            if !already_has {
                let ccr_tool = match endpoint {
                    compression::CompressibleEndpoint::AnthropicMessages => {
                        serde_json::json!({
                            "name": "headroom_retrieve",
                            "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. Provide `hash` from a compression marker like [N items compressed... hash=abc123], or `query` with keywords to search previously offloaded content. Exactly one of the two.",
                            "input_schema": {
                                "type": "object",
                                "properties": {
                                    "hash": {
                                        "type": "string",
                                        "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                                    },
                                    "query": {
                                        "type": "string",
                                        "description": "Keyword query to search previously offloaded content (e.g., 'provider squad retry logic'). Use when no marker hash is at hand."
                                    }
                                }
                            }
                        })
                    }
                    _ => {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": "headroom_retrieve",
                                "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. Provide `hash` from a compression marker like [N items compressed... hash=abc123], or `query` with keywords to search previously offloaded content. Exactly one of the two.",
                                "parameters": {
                                    "type": "object",
                                    "properties": {
                                        "hash": {
                                            "type": "string",
                                            "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                                        },
                                        "query": {
                                            "type": "string",
                                            "description": "Keyword query to search previously offloaded content (e.g., 'provider squad retry logic'). Use when no marker hash is at hand."
                                        }
                                    }
                                }
                            }
                        })
                    }
                };
                tools.push(ccr_tool);
                *changed = true;
                tracing::debug!(
                    request_id = %request_id,
                    "ccr: injected headroom_retrieve tool definition"
                );
            }
        }
    }
}

/// Verbosity steering on Anthropic-shaped request bodies.
///
/// Sets `changed` when the shaper rewrote anything. Extracted from
/// `forward_http` without behavior change.
pub(crate) fn run_output_shaper(
    value: &mut serde_json::Value,
    state: &AppState,
    request_id: &str,
    changed: &mut bool,
) {
    // Output shaping: verbosity steering on Anthropic-shaped
    // request bodies. Only runs when the output shaper is
    // enabled in config. The shaping is idempotent (steering
    // text includes a sentinel prefix) so repeated
    // applications are safe.
    // Disabled in cache mode: steering writes into the
    // provider prefix-cache key that mode exists to freeze.
    if state.config.output_shaper_enabled {
        let shape_result = crate::output_shaper::shape_request_for_mode(
            value,
            true,
            state.config.verbosity_level,
            &state.config.mode,
        );
        if shape_result.changed {
            *changed = true;
            tracing::debug!(
                request_id = %request_id,
                labels = ?shape_result.labels,
                "output_shaper applied"
            );
        }
    }
}

/// Returns a memory answer held back from a turn that also called a client tool.
///
/// Sets `changed` when an answer was put back. Extracted from `forward_http`
/// without behavior change.
pub(crate) fn restore_deferred_memory_answer(
    value: &mut serde_json::Value,
    request_id: &str,
    changed: &mut bool,
) {
    // Memory: put back any answer held from a turn that also
    // called a client tool. This request carries the client's
    // `tool_result`, so the turn can finally be completed —
    // and because it goes out as part of this request, the
    // cache write it causes is the one this turn needed
    // anyway. Prefix replay carries the repaired history
    // forward from here.
    if let Some(messages) = value.get_mut("messages").and_then(|v| v.as_array_mut()) {
        let applied = match crate::memory::deferred::store().lock() {
            Ok(mut held) if !held.is_empty() => held.apply(messages),
            _ => 0,
        };
        if applied > 0 {
            *changed = true;
            tracing::info!(
                request_id = %request_id,
                event = "memory_answer_restored",
                restored = applied,
                "memory: held answer returned to its turn"
            );
        }
    }
}

/// Memory search, injected into the latest user message tail.
///
/// Sets `changed` when the tail grew. Extracted from `forward_http` without
/// behavior change.
pub(crate) async fn inject_memory_context(
    value: &mut serde_json::Value,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    headers_snapshot: &Option<HeaderMap>,
    injection_budget: &crate::injection_budget::InjectionBudget,
    changed: &mut bool,
) {
    if let Some(handler) = state.memory_handler.as_ref() {
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
            if let Some(messages) = value.get("messages").and_then(|v| v.as_array()) {
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
                        headers: header_map_to_lowercase_strings(headers_snapshot.as_ref()),
                        system_prompt: crate::memory::router::extract_system_prompt(value),
                        base_user_id: base_user_id.to_string(),
                        project_root_override: state.config.memory_project_root.clone(),
                    },
                );
                // Memory runs last, so it sees whatever the
                // expansion and recall stages left. Clipping
                // here is cache-safe: this appends to the live
                // tail, which is re-sent every turn anyway.
                if let Some(context) = crate::memory::ctx_backend::SEARCH_REQUEST_ID
                    .scope(
                        request_id.to_string(),
                        handler.search_and_format_context(
                            &user_id, &msgs, None, // request_context
                            None, // ranker
                            None, // query
                            None, // budget
                        ),
                    )
                    .await
                    .and_then(|context| {
                        injection_budget
                            .take(crate::injection_budget::InjectionStage::Memory, context)
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
                    let (new_msgs, bytes) =
                        crate::memory::handler::MemoryHandler::append_to_latest_user_tail(
                            &msgs, &context, provider, 0,
                        );
                    if bytes > 0 {
                        if let Some(msgs_val) = value.get_mut("messages") {
                            *msgs_val = serde_json::Value::Array(new_msgs);
                            *changed = true;
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
}

/// What the ctx re-serialization step owes the offload stores.
pub(crate) struct CtxSerializeInputs<'a> {
    pub(crate) offload_records: Option<(
        &'a CtxOffloadRuntime,
        Vec<crate::compression::ctx_offload::OffloadRecord>,
    )>,
    pub(crate) ccr_workspace: Option<(String, Option<String>)>,
    pub(crate) latest_user_query: String,
    pub(crate) turn_number: u32,
    pub(crate) ctx_project: String,
}

/// Re-serializes the ctx-transformed body and records what the transform owes.
///
/// Hands back `buffered` untouched when nothing changed or the re-serialize
/// failed, so a broken write forwards the original bytes. Extracted from
/// `forward_http` without behavior change.
pub(crate) fn finalize_ctx_transformed_body(
    changed: bool,
    value: &serde_json::Value,
    buffered: bytes::Bytes,
    inputs: CtxSerializeInputs<'_>,
    state: &AppState,
    request_id: &str,
) -> bytes::Bytes {
    if !changed {
        return buffered;
    }
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(e) => {
            if let Some((runtime, records)) = &inputs.offload_records {
                runtime.gate.rollback_unstored_records(records);
            }
            tracing::warn!(
                event = "ctx_transform_reserialize_failed",
                request_id = %request_id,
                error = %e,
                "ctx transform re-serialization failed; forwarding original body"
            );
            return buffered;
        }
    };
    if let Some((runtime, records)) = &inputs.offload_records {
        if !runtime.store.persist(records, &inputs.ctx_project) {
            runtime.gate.rollback_unstored_records(records);
            tracing::warn!(
                event = "ctx_offload_backpressure_passthrough",
                request_id = %request_id,
                "forwarding the original request because CTX-3 index work could not be queued"
            );
            return buffered;
        }
        track_offloaded_ccr_records(state, records, &inputs, request_id);
    }
    axum::body::Bytes::from(bytes)
}

/// Hands stored offload records to CCR Phase 4 tracking, when the workspace
/// resolved.
fn track_offloaded_ccr_records(
    state: &AppState,
    records: &[crate::compression::ctx_offload::OffloadRecord],
    inputs: &CtxSerializeInputs<'_>,
    request_id: &str,
) {
    if let Some((workspace_key, _)) = inputs.ccr_workspace.as_ref() {
        track_ccr_context_records(
            state,
            records,
            workspace_key,
            &inputs.latest_user_query,
            inputs.turn_number,
            request_id,
        );
    } else if state.ccr_context_tracker.is_some() {
        tracing::info!(
            request_id = %request_id,
            "CCR Phase 4: workspace unresolved; skipping compression tracking"
        );
    }
}

/// Everything the response-body arms read besides the stream itself.
pub(crate) struct ResponseBodyCtx<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) request_id: &'a str,
    pub(crate) path_for_log: &'a str,
    pub(crate) headers_snapshot: &'a Option<HeaderMap>,
    pub(crate) original_buffered: &'a bytes::Bytes,
    pub(crate) continuation_base: &'a bytes::Bytes,
    pub(crate) upstream_url: &'a Url,
    pub(crate) upstream_client: &'a reqwest::Client,
    pub(crate) outgoing_headers: &'a HeaderMap,
    pub(crate) outcome_ctx: &'a Option<OutcomeContext>,
    pub(crate) buffered_responses_ccr: bool,
    pub(crate) is_sse: bool,
    pub(crate) sse_kind: SseStreamKind,
}

/// Picks the response body shape: buffered error, buffered success, reframed
/// SSE, or a straight stream.
///
/// Hands back `status` and `resp_headers` because the buffered and reframed
/// arms rewrite them. Extracted from `forward_http` without behavior change.
pub(crate) async fn build_response_body<S>(
    resp_stream: S,
    ctx: ResponseBodyCtx<'_>,
    status: StatusCode,
    resp_headers: HeaderMap,
) -> (Body, StatusCode, HeaderMap)
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    let ResponseBodyCtx {
        state,
        request_id,
        path_for_log,
        headers_snapshot,
        original_buffered,
        continuation_base,
        upstream_url,
        upstream_client,
        outgoing_headers,
        outcome_ctx,
        buffered_responses_ccr,
        is_sse,
        sse_kind,
    } = ctx;
    let mut status = status;
    let mut resp_headers = resp_headers;
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
        forward::collect_error_body(resp_stream, status, outcome_ctx, request_id, path_for_log)
            .await
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
                // Same for a turn hook's own re-drives. Bound at the post-hooks
                // call below (inert default when unregistered).

                // Memory tools: same contract as CCR below. The proxy
                // injects `memory_search` and friends, so the proxy runs them —
                // the client has never heard of them. Resolved up here because
                // it depends only on the request, and the pair below runs more
                // than once.
                let memory_provider = if path_for_log.contains("/v1/messages") {
                    Some("anthropic")
                } else if path_for_log.contains("/v1/chat/completions") {
                    Some("openai")
                } else if path_for_log.contains("/v1/responses") {
                    Some("openai_responses")
                } else {
                    None
                };
                let memory_ctx = memory_tool_context(
                    state,
                    headers_snapshot,
                    memory_provider,
                    original_buffered,
                )
                .await;

                // Retrieval and memory each run only the calls standing when
                // they start, and either one's continuation can come back
                // asking for the other. Alternate until a whole pass changes
                // nothing. See `MAX_RESOLVER_ALTERNATIONS`.
                let (resolved_body, loop_usage) = forward::resolve_ccr_memory_rounds(
                    body_bytes,
                    state,
                    request_id,
                    path_for_log,
                    continuation_base,
                    UpstreamCall {
                        client: upstream_client,
                        url: upstream_url,
                        headers: outgoing_headers,
                    },
                    MemoryRound {
                        ctx: memory_ctx,
                        provider: memory_provider,
                    },
                )
                .await;
                body_bytes = resolved_body;
                ccr_round_usage.absorb(loop_usage);
                let (hooked_body, hook_usage) = forward::apply_buffered_post_hooks(
                    body_bytes,
                    request_id,
                    path_for_log,
                    original_buffered,
                    outcome_ctx,
                    upstream_url,
                    upstream_client,
                    outgoing_headers,
                )
                .await;
                body_bytes = hooked_body;
                let turn_hook_usage = hook_usage;
                forward::store_semantic_cache_response(
                    state,
                    original_buffered,
                    &body_bytes,
                    &resp_headers,
                    request_id,
                );
                forward::emit_buffered_outcome(
                    outcome_ctx,
                    &body_bytes,
                    &ccr_round_usage,
                    &turn_hook_usage,
                    status,
                    request_id,
                );

                forward::wrap_buffered_success_body(
                    body_bytes,
                    buffered_responses_ccr,
                    &mut status,
                    &mut resp_headers,
                    request_id,
                )
            }
            Err(e) => {
                tracing::warn!(
                    event = "non_sse_buffer_failed",
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
    } else if is_sse
        && status.is_success()
        && matches!(sse_kind, SseStreamKind::Anthropic)
        && !client_wants_stream(original_buffered)
    {
        // The client asked for one JSON reply and upstream answered with an
        // event stream. Handing the SSE body straight back gives a client that
        // never opted into streaming something it cannot parse. Read the stream
        // to its end and answer in the shape that was asked for; if it did not
        // complete, say so with a 502 rather than inventing a partial turn.
        forward::reframe_sse_as_message(resp_stream, request_id, &mut status, &mut resp_headers)
            .await
    } else {
        forward::wrap_streaming_body(resp_stream, is_sse, status, sse_kind, request_id)
    };
    (body, status, resp_headers)
}

/// Picks the compression policy for the classified auth mode and logs both.
///
/// Inserts the policy as a request extension for the handlers downstream.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn select_compression_policy(
    req: &mut Request<Body>,
    state: &AppState,
    auth_mode: AuthMode,
    request_id: &str,
    method: &axum::http::Method,
    path_for_log: &str,
    body_bytes_hint: Option<u64>,
) {
    // Phase F PR-F2.1, c2/6: derive the per-mode CompressionPolicy at
    // request entry and stash alongside auth_mode. Storing the policy
    // (not just auth_mode) in extensions lets downstream stages read
    // the gate they need directly — no per-stage `for_mode` call.
    //
    // c3/6: when `auth_mode_policy_enforcement` is `Disabled`,
    // force the policy to PAYG regardless of classifier output.
    // (Historical note: `Disabled` used to be the default so the
    // rollout could land without changing live behavior; the default
    // is now `Enabled`, see `config.rs`.)
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
}

/// Resolves which upstream base and client this request goes to.
///
/// Extracted from `forward_http` without behavior change.
pub(crate) async fn resolve_selected_upstream(
    extensions: &axum::http::Extensions,
    headers: &HeaderMap,
    state: &AppState,
) -> Result<SelectedUpstream, ProxyError> {
    // Provider routes (Foundry) may pin a different upstream base for
    // this request via the `UpstreamOverride` extension; everything
    // else forwards to the configured `--upstream`. Absent that, honor a
    // per-request `x-headroom-base-url` header (mirrors the Python proxy):
    // trim whitespace and strip a trailing `/`; an empty/whitespace-only
    // value or an unparseable URL falls through to the default upstream.
    Ok(match extensions.get::<UpstreamOverride>() {
        Some(o) => SelectedUpstream {
            base: o.0.clone(),
            client: state.client.clone(),
            configured_http_proxy: state.config.http_proxy.is_some(),
            allow_slow_path_probe: true,
        },
        None => match header_upstream_override(headers).await {
            Some(resolved) => SelectedUpstream {
                client: caller_upstream_client(state, &resolved)?,
                base: resolved.url().clone(),
                configured_http_proxy: false,
                allow_slow_path_probe: false,
            },
            None => SelectedUpstream {
                base: state.effective_upstream().await,
                client: state.client.clone(),
                configured_http_proxy: state.config.http_proxy.is_some(),
                allow_slow_path_probe: true,
            },
        },
    })
}

/// Inputs the ctx/offload/memory transform pass reads from `forward_http`.
pub(crate) struct CtxGateInputs<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) request_id: &'a str,
    pub(crate) request_lane_key: &'a str,
    pub(crate) request_session_key: &'a str,
    pub(crate) headers_snapshot: &'a Option<HeaderMap>,
    pub(crate) rebuild_boundary: bool,
    pub(crate) history_rewritten: bool,
    pub(crate) offload_boundary: bool,
}

/// Runs the ctx inject/offload/memory transforms over an Anthropic body.
///
/// Hands back the buffered bytes untouched when the endpoint or the config
/// puts the request out of scope, or when the body will not parse. Extracted
/// from `forward_http` without behavior change.
pub(crate) async fn run_ctx_transform_gate(
    buffered: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    inputs: CtxGateInputs<'_>,
    stage_timer: &mut crate::stage_timer::StageTimer,
    ctx_transform_tokens_saved: &mut i64,
    proactive_expansion_applied: &mut bool,
) -> bytes::Bytes {
    let CtxGateInputs {
        state,
        request_id,
        request_lane_key,
        request_session_key,
        headers_snapshot,
        rebuild_boundary,
        history_rewritten,
        offload_boundary,
    } = inputs;
    if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) && (state.ctx_inject.is_some() || state.ctx_offload.is_some())
    {
        match serde_json::from_slice::<serde_json::Value>(&buffered) {
            Ok(mut value) => {
                let (
                    mut changed,
                    offload_records,
                    ccr_workspace,
                    latest_user_query,
                    turn_number,
                    injection_budget,
                    ctx_project,
                ) = forward::run_ctx_inject_offload(
                    &mut value,
                    state,
                    forward::CtxRequestKeys {
                        request_id,
                        lane_key: request_lane_key,
                        session_key: request_session_key,
                    },
                    headers_snapshot,
                    forward::CtxBoundaryFlags {
                        rebuild_boundary,
                        history_rewritten,
                        offload_boundary,
                    },
                    ctx_transform_tokens_saved,
                    proactive_expansion_applied,
                );

                forward::inject_memory_tool_definitions(
                    &mut value,
                    endpoint,
                    state,
                    request_id,
                    &mut changed,
                );
                forward::inject_ccr_retrieve_tool(
                    &mut value,
                    endpoint,
                    state,
                    request_id,
                    *ctx_transform_tokens_saved,
                    &mut changed,
                );

                forward::run_output_shaper(&mut value, state, request_id, &mut changed);

                forward::restore_deferred_memory_answer(&mut value, request_id, &mut changed);

                // Memory: search and inject context into user message tail.
                // Runs after output shaping, before final serialization.
                let memory_start = Instant::now();
                forward::inject_memory_context(
                    &mut value,
                    endpoint,
                    state,
                    request_id,
                    headers_snapshot,
                    &injection_budget,
                    &mut changed,
                )
                .await;

                stage_timer.record("memory", memory_start.elapsed().as_secs_f64() * 1000.0);

                forward::finalize_ctx_transformed_body(
                    changed,
                    &value,
                    buffered,
                    forward::CtxSerializeInputs {
                        offload_records,
                        ccr_workspace,
                        latest_user_query,
                        turn_number,
                        ctx_project,
                    },
                    state,
                    request_id,
                )
            }
            Err(_) => buffered,
        }
    } else {
        buffered
    }
}

/// What `forward_http` hands the compression dispatcher.
pub(crate) struct CompressionStageIn<'a> {
    pub(crate) buffered: &'a bytes::Bytes,
    pub(crate) endpoint: compression::CompressibleEndpoint,
    pub(crate) state: &'a AppState,
    pub(crate) decision: &'a crate::compression_decision::CompressionDecision,
    pub(crate) effective_auth_mode: AuthMode,
    pub(crate) auth_mode: AuthMode,
    pub(crate) request_id: &'a str,
    pub(crate) path_for_log: &'a str,
}

/// The compression outcome plus the snapshots taken before it is consumed.
pub(crate) struct CompressionStageOut {
    pub(crate) outcome: compression::Outcome,
    pub(crate) original_buffered_len: usize,
    pub(crate) outcome_is_passthrough_class: bool,
    pub(crate) compress_tokens_before: i64,
    pub(crate) compress_tokens_saved: i64,
    pub(crate) compress_strategies: Vec<String>,
}

/// Dispatches compression, or logs the passthrough the decision asked for.
///
/// Reads the byte length and the passthrough class out of `outcome` before
/// the caller consumes it. Extracted from `forward_http` without behavior
/// change.
pub(crate) fn run_compression_stage(input: CompressionStageIn<'_>) -> CompressionStageOut {
    let CompressionStageIn {
        buffered,
        endpoint,
        state,
        decision,
        effective_auth_mode,
        auth_mode,
        request_id,
        path_for_log,
    } = input;
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
        forward::dispatch_compression(
            buffered,
            endpoint,
            state,
            effective_auth_mode,
            auth_mode,
            request_id,
        )
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
    CompressionStageOut {
        outcome,
        original_buffered_len,
        outcome_is_passthrough_class,
        compress_tokens_before,
        compress_tokens_saved,
        compress_strategies,
    }
}

#[cfg(test)]
mod memory_status_is_retryable_tests {
    use super::memory_status_is_retryable;
    use http::StatusCode;

    #[test]
    fn retryable_matrix() {
        let too_many = StatusCode::TOO_MANY_REQUESTS;
        let server = StatusCode::INTERNAL_SERVER_ERROR;
        let forbidden = StatusCode::FORBIDDEN;
        let bad = StatusCode::BAD_REQUEST;
        assert!(memory_status_is_retryable(too_many, false));
        assert!(memory_status_is_retryable(too_many, true));
        assert!(memory_status_is_retryable(server, false));
        assert!(memory_status_is_retryable(server, true));
        assert!(!memory_status_is_retryable(forbidden, false));
        assert!(memory_status_is_retryable(forbidden, true));
        assert!(!memory_status_is_retryable(bad, false));
        assert!(!memory_status_is_retryable(bad, true));
    }
}
