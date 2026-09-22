//! OpenAI SSE stream tasks: drive loops plus close/outcome booking.
//!
//! Pure module move of the `SseStreamKind::OpenAiChat` and
//! `SseStreamKind::OpenAiResponses` arms of
//! [`super::run_sse_state_machine`]: per-arm drive loops, hit-rate
//! notes, tier/status notes (Responses), close logs, and the shared
//! outcome emit. No logic changes — every block is verbatim motion.
//!
//! `use super::*` keeps the parent's private items
//! (`emit_openai_stream_outcome`, …) reachable with zero visibility
//! churn elsewhere in the crate.

use super::*;
use axum::http::StatusCode;

/// Drain ready framer events into the Chat chunk state, warning (not
/// failing) on apply/framing errors.
/// Extracted from the OpenAiChat arm without behavior change.
fn drain_chat_events(
    framer: &mut crate::sse::framing::SseFramer,
    state: &mut crate::sse::openai_chat::ChunkState,
    request_id: &str,
) {
    while let Some(ev_result) = framer.next_event() {
        match ev_result {
            Ok(ev) => {
                if let Err(e) = state.apply(ev) {
                    tracing::warn!(
                        event = "sse_openai_chat_apply_error",
                        request_id = %request_id,
                        error = %e,
                        "sse openai_chat state-machine apply error"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "sse_openai_chat_framer_error",
                    request_id = %request_id,
                    error = %e,
                    "sse framer error"
                );
            }
        }
    }
}

/// Drain ready framer events into the Responses state, warning (not
/// failing) on apply/framing errors.
/// Extracted from the OpenAiResponses arm without behavior change.
fn drain_responses_events(
    framer: &mut crate::sse::framing::SseFramer,
    state: &mut crate::sse::openai_responses::ResponseState,
    request_id: &str,
) {
    while let Some(ev_result) = framer.next_event() {
        match ev_result {
            Ok(ev) => {
                if let Err(e) = state.apply(ev) {
                    tracing::warn!(
                        event = "sse_openai_responses_apply_error",
                        request_id = %request_id,
                        error = %e,
                        "sse openai_responses state-machine apply error"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "sse_openai_responses_framer_error",
                    request_id = %request_id,
                    error = %e,
                    "sse framer error"
                );
            }
        }
    }
}

/// Drive the Chat chunk state over the channel. Returns the finished
/// state; `ttfb_ms` latches on the first chunk as usual.
pub(super) async fn drive_openai_chat_stream(
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    mut framer: crate::sse::framing::SseFramer,
    request_id: &str,
    outcome_ctx: &Option<OutcomeContext>,
    ttfb_ms: &mut f64,
) -> crate::sse::openai_chat::ChunkState {
    let mut state = crate::sse::openai_chat::ChunkState::new();
    while let Some(chunk) = rx.recv().await {
        latch_ttfb(ttfb_ms, outcome_ctx);
        framer.push(&chunk);
        drain_chat_events(&mut framer, &mut state, request_id);
    }
    state
}

/// Drive the Responses state over the channel. Returns the finished
/// state; `ttfb_ms` latches on the first chunk as usual.
pub(super) async fn drive_openai_responses_stream(
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    mut framer: crate::sse::framing::SseFramer,
    request_id: &str,
    outcome_ctx: &Option<OutcomeContext>,
    ttfb_ms: &mut f64,
) -> crate::sse::openai_responses::ResponseState {
    let mut state = crate::sse::openai_responses::ResponseState::new();
    while let Some(chunk) = rx.recv().await {
        latch_ttfb(ttfb_ms, outcome_ctx);
        framer.push(&chunk);
        drain_responses_events(&mut framer, &mut state, request_id);
    }
    state
}

/// Phase G PR-G3: emit Chat cache-hit-rate from the final usage chunk.
/// OpenAI only emits this when `stream_options.include_usage = true`;
/// absence is a signal, not a fallback condition — `usage = None` →
/// skip. The H2 gate is implicit here: the final usage chunk only
/// arrives when the stream completed (it's OpenAI's terminal-status
/// equivalent).
pub(super) fn note_openai_chat_hit_rate(
    state: &crate::sse::openai_chat::ChunkState,
    request_id: &str,
) {
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
            match crate::observability::compute_cache_hit_rate(non_cached, cached_tokens, 0) {
                Some(rate) => {
                    crate::observability::observe_cache_hit_rate(
                        crate::observability::cache_hit_rate_provider::OPENAI_CHAT,
                        request_id,
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
}

/// Read the Chat final usage triple (input, cached, output), zeros when
/// the stream carried no usage chunk.
pub(super) fn extract_chat_usage(state: &crate::sse::openai_chat::ChunkState) -> (i64, i64, i64) {
    if let Some(ref usage) = state.usage {
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
    }
}

/// Close the Chat stream: hit rate, close log, and the shared outcome
/// emit when the task has an outcome context.
pub(super) fn close_openai_chat_stream(
    state: &crate::sse::openai_chat::ChunkState,
    request_id: &str,
    ttfb_ms: f64,
    outcome_ctx: &Option<OutcomeContext>,
    upstream_status: StatusCode,
) {
    note_openai_chat_hit_rate(state, request_id);
    tracing::info!(
        request_id = %request_id,
        provider = "openai_chat",
        choices = state.choices.len(),
        has_usage = state.usage.is_some(),
        "sse stream closed"
    );
    if let Some(ref ctx) = outcome_ctx {
        let (input_tok, cached_tok, output_tok) = extract_chat_usage(state);
        emit_openai_stream_outcome(
            ctx,
            request_id,
            ttfb_ms,
            input_tok,
            cached_tok,
            output_tok,
            upstream_status.as_u16() as i64,
        );
    }
}

/// Emit the Responses hit-rate sample for a split input count, or log
/// the zero-denominator skip.
/// Extracted from `note_openai_responses_hit_rate` without behavior change.
fn emit_responses_hit_rate(non_cached: u64, cached_tokens: u64, request_id: &str) {
    match crate::observability::compute_cache_hit_rate(non_cached, cached_tokens, 0) {
        Some(rate) => {
            crate::observability::observe_cache_hit_rate(
                crate::observability::cache_hit_rate_provider::OPENAI_RESPONSES,
                request_id,
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

/// Phase G PR-G3 + H2: Responses cache hit rate ONLY when the stream
/// reached a terminal status (`response.completed/failed/incomplete`).
/// Mid-stream client disconnects close the channel without a terminal
/// — `terminal_status().is_none()` then guards emit so we don't
/// observe garbage samples.
///
/// The Responses API uses `input_tokens` / `cached_input_tokens`
/// shape (Responses-specific — distinct from Chat Completions'
/// `prompt_tokens`).
pub(super) fn note_openai_responses_hit_rate(
    state: &crate::sse::openai_responses::ResponseState,
    stream_completed: bool,
    request_id: &str,
) {
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
                emit_responses_hit_rate(non_cached, cached_tokens, request_id);
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
}

/// Service tier + response status from `state.last_response_envelope`,
/// populated by the ResponseState on
/// `response.completed/failed/incomplete`.
///
/// C1 fix: the tier value comes from the upstream response body; even
/// though the upstream is more trustworthy than a client-side header,
/// an unrecognised value would still grow the metric vector
/// unboundedly. We bucket through the same validator the
/// request-side handler uses.
pub(super) fn note_responses_tier_status(
    state: &crate::sse::openai_responses::ResponseState,
    request_id: &str,
) {
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
        crate::observability::record_service_tier(bucketed, request_id);
    }
    if let Some(status) = state.terminal_status() {
        crate::observability::record_response_status(
            status,
            state.incomplete_reason.as_deref(),
            request_id,
        );
    }
}

/// Read the Responses final usage triple (input, cached, output),
/// zeros when the stream carried no usage chunk.
pub(super) fn extract_responses_usage(
    state: &crate::sse::openai_responses::ResponseState,
) -> (i64, i64, i64) {
    if let Some(ref usage) = state.usage {
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
    }
}

/// Close the Responses stream: hit rate (terminal-gated), tier +
/// status, close log, and the shared outcome emit when the task has
/// an outcome context.
pub(super) fn close_openai_responses_stream(
    state: &crate::sse::openai_responses::ResponseState,
    request_id: &str,
    ttfb_ms: f64,
    outcome_ctx: &Option<OutcomeContext>,
    upstream_status: StatusCode,
) {
    let stream_completed = state.terminal_status().is_some();
    note_openai_responses_hit_rate(state, stream_completed, request_id);
    note_responses_tier_status(state, request_id);
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
        let (input_tok, cached_tok, output_tok) = extract_responses_usage(state);
        emit_openai_stream_outcome(
            ctx,
            request_id,
            ttfb_ms,
            input_tok,
            cached_tok,
            output_tok,
            upstream_status.as_u16() as i64,
        );
    }
}
