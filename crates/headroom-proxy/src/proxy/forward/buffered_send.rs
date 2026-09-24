//! Buffered upstream send: body read, request preparation and transforms,
//! the retry loop (transient status, leading in-band SSE error, transport
//! error), and error-body collection.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

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
pub(super) async fn maybe_retry_status(
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
pub(super) async fn maybe_retry_leading_error(
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
pub(super) async fn handle_transport_error(
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
