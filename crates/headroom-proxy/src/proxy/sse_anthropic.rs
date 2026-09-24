//! Anthropic SSE stream task: drive loop plus close/outcome booking.
//!
//! Pure module move of the `SseStreamKind::Anthropic` arm of
//! [`super::run_sse_state_machine`]: the drive loop (framer + state
//! machine), the unterminated-tail flush, and the eight close blocks
//! (billed totals, hit rate, watchdog, replay, close/defect logs,
//! incomplete booking, CCR fold, outcome emit). No logic changes —
//! every block is verbatim motion, reading through [`AnthropicClose`].
//!
//! `use super::*` keeps the parent's private items (`OutcomeContext`,
//! `CcrRoundUsage`, `latch_ttfb`, …) reachable with zero visibility
//! churn elsewhere in the crate.

use super::*;
use axum::http::StatusCode;
use std::sync::Arc;

/// Everything the Anthropic close blocks read: the finished state plus
/// the CCR baseline triple and the shared task context.
pub(super) struct AnthropicClose<'a> {
    pub state: &'a crate::sse::anthropic::AnthropicStreamState,
    pub ccr_rounds: CcrRoundUsage,
    pub cache_baseline_input: u64,
    pub cache_baseline_read: u64,
    pub cache_baseline_write: u64,
    pub usage_observer: &'a Arc<cache_stabilization::usage_observer::UsageObserver>,
    pub outcome_ctx: &'a Option<OutcomeContext>,
    pub replay_store: &'a Option<SessionReplayStore>,
    pub request_id: &'a str,
    pub ttfb_ms: f64,
    pub upstream_status: StatusCode,
}

impl<'a> AnthropicClose<'a> {
    fn stream_completed(&self) -> bool {
        self.state.status == crate::sse::anthropic::StreamStatus::MessageStop
    }
}

/// Drain ready framer events into the state machine, warning (not
/// failing) on apply/framing errors.
/// Extracted from `drive_anthropic_stream` without behavior change.
fn drain_framer_events(
    framer: &mut crate::sse::framing::SseFramer,
    state: &mut crate::sse::anthropic::AnthropicStreamState,
    request_id: &str,
) {
    while let Some(ev_result) = framer.next_event() {
        match ev_result {
            Ok(ev) => {
                if let Err(e) = state.apply(ev) {
                    tracing::warn!(
                        event = "sse_anthropic_apply_error",
                        request_id = %request_id,
                        error = %e,
                        "sse anthropic state-machine apply error"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    event = "sse_anthropic_framer_error",
                    request_id = %request_id,
                    error = %e,
                    "sse framer error"
                );
            }
        }
    }
}

/// Drive the Anthropic state machine over the channel. Returns the
/// finished state; `ttfb_ms` latches on the first chunk as usual.
pub(super) async fn drive_anthropic_stream(
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    mut framer: crate::sse::framing::SseFramer,
    request_id: &str,
    outcome_ctx: &Option<OutcomeContext>,
    ttfb_ms: &mut f64,
) -> crate::sse::anthropic::AnthropicStreamState {
    let mut state = crate::sse::anthropic::AnthropicStreamState::new();
    while let Some(chunk) = rx.recv().await {
        latch_ttfb(ttfb_ms, outcome_ctx);
        framer.push(&chunk);
        drain_framer_events(&mut framer, &mut state, request_id);
    }
    // A stream can end with its last event unterminated: the framer
    // only yields a block once it sees the blank line, so a
    // `message_stop` that straddles the final two chunks sits in the
    // buffer and the turn reads as unfinished. Supplying the
    // terminator here costs nothing when the provider sent one.
    if framer.buffered_len() > 0 {
        let stranded = framer.buffered_len();
        framer.push(b"\n\n");
        drain_framer_events(&mut framer, &mut state, request_id);
        tracing::warn!(
            request_id = %request_id,
            event = "sse_tail_flushed",
            stranded_bytes = stranded,
            "flushed an unterminated trailing event at end of stream"
        );
    }
    state
}

/// Snapshot hidden continuation usage before any cache observer runs.
/// The final streamed usage belongs to the proxy's private continuation
/// request; the first discarded response below is the cache footprint of
/// the request the client actually sent. Hand the observer the full
/// billed total so the two events agree on one request's billed usage.
pub(super) fn note_anthropic_billed_totals(close: &AnthropicClose<'_>) {
    // The baseline above is the right thing to classify against and the
    // wrong thing to bill: it drops the continuation rounds, which the
    // provider charged for and which `savings_pricing_counterfactual`
    // already counts off the outcome. Hand the observer the full total
    // so the two events agree on one request's billed usage.
    if close.ccr_rounds.rounds > 0 {
        close.usage_observer.note_billed_totals(
            close.request_id,
            close
                .state
                .usage
                .input_tokens
                .saturating_add(close.ccr_rounds.input_tokens.max(0) as u64),
            close
                .state
                .usage
                .cache_read_input_tokens
                .saturating_add(close.ccr_rounds.cache_read_tokens.max(0) as u64),
            close
                .state
                .usage
                .cache_creation_input_tokens
                .saturating_add(close.ccr_rounds.cache_write_tokens.max(0) as u64),
        );
    }
}

/// Phase G PR-G3 + H2: emit per-session cache-hit-rate ONLY when the
/// stream completed cleanly with `message_stop`. The gate is
/// encapsulated by the pure function
/// `compute_anthropic_session_hit_rate` so the H2 contract has a
/// unit-testable surface.
pub(super) fn note_anthropic_hit_rate(close: &AnthropicClose<'_>) {
    // Phase G PR-G3 + H2: emit per-session cache-hit-rate
    // ONLY when the stream completed cleanly with
    // `message_stop`. The gate is encapsulated by the
    // pure function `compute_anthropic_session_hit_rate`
    // so the H2 contract has a unit-testable surface.
    let cache_hit_rate = if close.state.status == crate::sse::anthropic::StreamStatus::MessageStop {
        crate::observability::cache_hit_rate::compute_hit_rate(
            close.cache_baseline_input,
            close.cache_baseline_read,
            close.cache_baseline_write,
        )
    } else {
        None
    };
    match cache_hit_rate {
        Some(rate) => {
            crate::observability::observe_cache_hit_rate(
                crate::observability::cache_hit_rate_provider::ANTHROPIC,
                close.request_id,
                rate,
            );
        }
        None => {
            tracing::debug!(
                event = "cache_hit_rate_skipped",
                request_id = %close.request_id,
                provider = "anthropic",
                status = ?close.state.status,
                input_tokens = close.cache_baseline_input,
                cache_read_input_tokens = close.cache_baseline_read,
                cache_creation_input_tokens = close.cache_baseline_write,
                "skipping proxy_cache_hit_rate_per_session: H2 gate or zero denominator"
            );
        }
    }
}

/// CTX-7: feed the re-cache watchdog with this turn's billed usage.
/// Same H2 gate as the hit-rate metric: only a cleanly completed
/// stream (`message_stop`) carries trustworthy final usage.
pub(super) fn complete_anthropic_watchdog(close: &AnthropicClose<'_>) {
    // CTX-7: feed the re-cache watchdog with this turn's
    // billed usage. Same H2 gate as the hit-rate metric: only
    // a cleanly completed stream (`message_stop`) carries
    // trustworthy final usage.
    if close.state.status == crate::sse::anthropic::StreamStatus::MessageStop {
        if let Some(ctx) = close.outcome_ctx.as_ref() {
            observe_proactive_expansion_cache_write(ctx, close.cache_baseline_write);
        }
        close
            .usage_observer
            .note_output_tokens(close.request_id, close.state.usage.output_tokens);
        let class = close.usage_observer.complete(
            close.request_id,
            close.cache_baseline_input,
            close.cache_baseline_read,
            close.cache_baseline_write,
            // Read off the streamed usage rather than the CCR baseline
            // beside it: continuation rounds carry no TTL breakdown, so
            // the split stays a subset of the write total, exactly as
            // the buffered path treats it.
            Some((
                close.state.usage.cache_creation_5m_input_tokens,
                close.state.usage.cache_creation_1h_input_tokens,
            )),
        );
        // The observer's counters reset on restart, so persist the
        // classification here where the savings tracker is reachable.
        // Without this there is no way to answer "is the proxy paying
        // for itself" across sessions.
        if let (Some(class), Some(ctx)) = (class, close.outcome_ctx.as_ref()) {
            use headroom_core::request_outcome::OutcomeSink as _;
            let (reason, wasted) = class.as_record();
            ctx.sink.record_cache_outcome("anthropic", reason, wasted);
        }
    }
}

/// Freeze-replay: feed the completed turn's cache tokens back into the
/// session tracker so the next turn can replay the forwarded prefix
/// (Python parity: `update_from_response` after every API call). Gated
/// on clean completion — a half-finished stream may be a client
/// disconnect with unreliable usage totals. `complete` is a no-op when
/// this request was never parked (non-Anthropic, or the buffered path
/// didn't run).
pub(super) fn complete_anthropic_replay(close: &AnthropicClose<'_>) {
    // Freeze-replay: feed the completed turn's cache tokens
    // back into the session tracker so the next turn can
    // replay the forwarded prefix (Python parity:
    // `update_from_response` after every API call). Gated on
    // clean completion for the same reason as the H2 gate
    // above — a half-finished stream may be a client
    // disconnect with unreliable usage totals. `complete` is
    // a no-op when this request was never parked
    // (non-Anthropic, or the buffered path didn't run).
    if let Some(store) = close.replay_store.as_ref() {
        if close.state.status == crate::sse::anthropic::StreamStatus::MessageStop {
            store.complete(
                close.request_id,
                close.cache_baseline_read,
                close.cache_baseline_write,
            );
        }
    }
}

/// Close + defect logs: the output census split and the tool-call
/// defect warn for a turn the client would refuse whole.
pub(super) fn log_anthropic_close(close: &AnthropicClose<'_>) {
    let split = close.state.output_split();
    // Server-tool pairing inventory (ids, capped): joins against the next
    // turn's history when an orphan-result 400 needs wire-vs-client
    // attribution. Empty on turns without server tools.
    let inv = close.state.server_tool_inventory();
    tracing::info!(
        request_id = %close.request_id,
        provider = "anthropic",
        input_tokens = close.state.usage.input_tokens,
        output_tokens = close.state.usage.output_tokens,
        cache_creation_input_tokens = close.state.usage.cache_creation_input_tokens,
        cache_read_input_tokens = close.state.usage.cache_read_input_tokens,
        cleared_input_tokens = close.state.cleared_input_tokens,
        stop_reason = close.state.stop_reason.as_deref().unwrap_or(""),
        blocks = close.state.blocks.len(),
        // Which of the three buckets the output went to. Nothing in
        // the log said whether a turn's tokens were thinking, prose or
        // tool arguments, so there was no way to size an output lever
        // without guessing from transcripts.
        thinking_chars = split.thinking_chars,
        text_chars = split.text_chars,
        tool_input_chars = split.tool_input_chars,
        thinking_blocks = split.thinking_blocks,
        text_blocks = split.text_blocks,
        tool_use_blocks = split.tool_use_blocks,
        thinking_deltas = split.thinking_deltas,
        server_tool_calls = inv.calls.join(" "),
        server_tool_results = inv.results.join(" "),
        server_tool_calls_total = inv.calls_total,
        server_tool_results_total = inv.results_total,
        "sse stream closed"
    );
    // A turn the client will refuse whole: it was told a tool call
    // was coming and got nothing it can run. Warn here or the only
    // symptom is "tool call could not be parsed" on the far side,
    // with a clean `sse stream closed` on this one.
    if let Some(defect) = close.state.tool_call_defect() {
        tracing::warn!(
            event = "tool_call_defect",
            request_id = %close.request_id,
            kind = defect.kind(),
            stop_reason = close.state.stop_reason.as_deref().unwrap_or(""),
            output_tokens = close.state.usage.output_tokens,
            detail = %defect,
            "upstream declared a tool call the client cannot execute"
        );
    }
    // A server result this stream never paired with a call. Provider-side
    // evidence for the orphan-result 400 class: if the call is absent here
    // too, the client cannot be blamed for losing it. Gated on completion
    // so a cut stream's partial inventory doesn't false-positive.
    if close.stream_completed() {
        let orphans = close.state.server_result_orphans();
        if !orphans.is_empty() {
            tracing::warn!(
                event = "sse_server_result_orphaned_in_stream",
                request_id = %close.request_id,
                orphan_results = orphans.join(" "),
                "stream carried server results with no matching server_tool_use call"
            );
        }
    }
}

/// Incomplete-stream booking. Same H2 gate the consumers above use:
/// a stream cut short by a client disconnect carries whatever partial
/// count had arrived — booking that as final under-reports output.
/// A 5xx served as SSE is failed work, never a missing turn. A final
/// stop_reason without the terminator still books: the numbers are
/// right, and the missing tail is a fault worth seeing.
pub(super) fn book_anthropic_incomplete(close: &AnthropicClose<'_>) {
    // Same H2 gate the three consumers above use, and for the same
    // reason. Anthropic reports the turn's final `output_tokens` in
    // the `message_delta` that precedes `message_stop`; a stream cut
    // short by a client disconnect carries whatever partial count had
    // arrived by then. Booking that as final under-reported output —
    // silently, because a truncated turn is indistinguishable from a
    // cheap one once it is in the ledger. Dropping the turn also
    // under-reports, but visibly: the counter says how many turns the
    // books are missing, and the log below keeps the partial numbers.
    // A `stop_reason` on a `message_delta` is not a terminator. It says
    // how the model meant to end, not that the stream got there, and
    // the proxy synthesises stop reasons of its own elsewhere
    // (`stream_finisher`, `ccr_stream`), so it is the weaker of the two
    // signals. `message_stop` off the wire is the gate, same as above.
    let split = close.state.output_split();
    if !close.stream_completed() {
        crate::observability::record_stream_incomplete("anthropic");
        // Also booked into the persisted savings state, so the lifetime
        // verdict can report how many turns it is missing. The
        // Prometheus counter above resets with the process; the books
        // do not.
        //
        // Upstream 4949cd55: when the HTTP status itself is a 5xx, the
        // errored stream is failed work, not a missing turn — an
        // exhausted 529 served as SSE must land in `record_failed`,
        // never in the success stats nor the unbooked counter.
        if close.upstream_status.is_server_error() {
            if let Some(ref ctx) = close.outcome_ctx {
                let outcome = headroom_core::request_outcome::RequestOutcome {
                    request_id: close.request_id.to_string(),
                    provider: ctx.provider.clone(),
                    model: ctx.model.clone(),
                    status_code: close.upstream_status.as_u16() as i64,
                    output_tokens: close.state.usage.output_tokens as i64,
                    uncached_input_tokens: close.state.usage.input_tokens as i64,
                    total_latency_ms: ctx.started_at.elapsed().as_secs_f64() * 1000.0,
                    overhead_ms: ctx.overhead_ms,
                    transforms_applied: ctx.transforms_applied.clone(),
                    num_messages: ctx.num_messages,
                    tags: ctx.tags.clone(),
                    client: ctx.client.clone(),
                    project: ctx.project.clone(),
                    ..Default::default()
                };
                headroom_core::request_outcome::emit_failed_request_outcome(
                    ctx.sink.as_ref(),
                    &outcome,
                );
            }
        } else if let Some(ref ctx) = close.outcome_ctx {
            ctx.sink.savings_tracker.record_unbooked_turn(
                close.state.usage.input_tokens as i64,
                close.state.usage.output_tokens as i64,
            );
        }
        tracing::warn!(
            request_id = %close.request_id,
            event = "stream_incomplete",
            provider = "anthropic",
            status = ?close.state.status,
            partial_input_tokens = close.state.usage.input_tokens,
            partial_output_tokens = close.state.usage.output_tokens,
            // Same split as a clean close, so a day's output census
            // does not silently drop the turns that broke.
            thinking_chars = split.thinking_chars,
            text_chars = split.text_chars,
            tool_input_chars = split.tool_input_chars,
            "stream ended without message_stop; usage is partial, \
             so this turn is not booked into cost or savings"
        );
    } else if close.state.status != crate::sse::anthropic::StreamStatus::MessageStop {
        // Booked on a final stop_reason, but the stream still ended
        // without its terminator. Say so: the turn's numbers are
        // right, and the missing tail is a fault worth seeing.
        tracing::warn!(
            request_id = %close.request_id,
            event = "stream_booked_without_message_stop",
            provider = "anthropic",
            status = ?close.state.status,
            stop_reason = close.state.stop_reason.as_deref().unwrap_or(""),
            output_tokens = close.state.usage.output_tokens,
            "stream ended without message_stop but carried a final \
             stop_reason; usage is final, so the turn is booked"
        );
    }
}

/// Fold in the CCR continuation rounds, exactly as the buffered path
/// does. The client saw one turn; the upstream billed several, and only
/// the last one's usage reached the stream. Without this the savings
/// figures are computed against a fraction of what the turn cost.
/// Emits the completed-turn outcome when the stream completed.
pub(super) fn emit_anthropic_outcome(close: &AnthropicClose<'_>) {
    // Fold in the CCR continuation rounds, exactly as the buffered
    // path does. The client saw one turn; the upstream billed several,
    // and only the last one's usage reached the stream. Without this
    // the savings figures are computed against a fraction of what the
    // turn cost. Reading it here is safe: the rewriter fills it in
    // before it sends the final events, and this runs after the
    // channel those events travelled on has closed.
    if !close.ccr_rounds.is_empty() {
        tracing::info!(
            request_id = %close.request_id,
            event = "ccr_continuation_usage",
            rounds = close.ccr_rounds.rounds,
            input_tokens = close.ccr_rounds.input_tokens,
            output_tokens = close.ccr_rounds.output_tokens,
            cache_write_tokens = close.ccr_rounds.cache_write_tokens,
            // Folded into RequestOutcome since CCR-1 but never logged,
            // so `turn_cost_ledger` (which sums the rounds in) could
            // not be reconciled against `sse stream closed` (which
            // does not) on any turn that retrieved.
            cache_read_tokens = close.ccr_rounds.cache_read_tokens,
            client_cache_read_tokens = close.cache_baseline_read,
            client_cache_write_tokens = close.cache_baseline_write,
            "billed CCR continuation rounds the client never saw"
        );
    }
    let attempted_input = close.state.usage.input_tokens as i64 + close.ccr_rounds.input_tokens;
    if let (Some(ref ctx), true) = (close.outcome_ctx, close.stream_completed()) {
        // Search round-trip counts joined to the same outcome row as the
        // deferral tags: how often the model reached for the search tool this
        // turn. Zero on turns without server tools (inventory is empty, not
        // missing). The inventory ids stay log-only; only totals join here.
        let search_inv = close.state.server_tool_inventory();
        let mut tags = ctx.tags.clone();
        tags.insert(
            "tool_search_calls".to_string(),
            search_inv.calls_total.to_string(),
        );
        tags.insert(
            "tool_search_results".to_string(),
            search_inv.results_total.to_string(),
        );
        let outcome = headroom_core::request_outcome::RequestOutcome {
            request_id: close.request_id.to_string(),
            provider: ctx.provider.clone(),
            model: ctx.model.clone(),
            original_tokens: ctx.sizes(attempted_input).0,
            optimized_tokens: ctx.sizes(attempted_input).1,
            output_tokens: close.state.usage.output_tokens as i64 + close.ccr_rounds.output_tokens,
            tokens_saved: ctx.tokens_saved,
            conversation_key: ctx.conversation_key.clone(),
            conversation_tokens_saved: Some(ctx.tokens_saved),
            attempted_input_tokens: ctx.attempted(attempted_input),
            cache_read_tokens: close.state.usage.cache_read_input_tokens as i64
                + close.ccr_rounds.cache_read_tokens,
            cache_write_tokens: close.state.usage.cache_creation_input_tokens as i64
                + close.ccr_rounds.cache_write_tokens,
            cache_write_5m_tokens: close.state.usage.cache_creation_5m_input_tokens as i64,
            cache_write_1h_tokens: close.state.usage.cache_creation_1h_input_tokens as i64,
            // Anthropic's `input_tokens` already excludes cache reads
            // and writes, so it *is* the uncached count. Python's
            // Bedrock path has to subtract instead, because there
            // `input_tokens` is the total — do not copy that formula
            // here.
            uncached_input_tokens: attempted_input,
            waste_signals: ctx.waste_signals.clone(),
            total_latency_ms: ctx.total_latency_ms,
            overhead_ms: ctx.overhead_ms,
            ttfb_ms: close.ttfb_ms,
            transforms_applied: ctx.transforms_applied.clone(),
            num_messages: ctx.num_messages,
            tags,
            client: ctx.client.clone(),
            project: ctx.project.clone(),
            // Upstream 4949cd55: stamp the real HTTP status so a
            // 5xx served as SSE diverts to the failure funnel
            // instead of booking a success.
            status_code: close.upstream_status.as_u16() as i64,
            ..Default::default()
        };
        record_wire_footprint(
            ctx,
            outcome.uncached_input_tokens,
            outcome.cache_read_tokens,
            outcome.cache_write_tokens,
        );
        headroom_core::request_outcome::emit_request_outcome(ctx.sink.as_ref(), &outcome);
    }
}
