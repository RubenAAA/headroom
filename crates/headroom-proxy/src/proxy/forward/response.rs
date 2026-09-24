//! Response assembly: CCR/memory resolution rounds, post hooks, semantic
//! cache store, buffered outcome, SSE reframing, streaming wrap, and the
//! final response.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

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
