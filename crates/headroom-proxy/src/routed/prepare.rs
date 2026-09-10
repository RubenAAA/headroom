//! Pre-translation preparation for a routed turn.
//!
//! The Claude path's closing sequence, in its order: CTX transforms, the
//! conversation-concurrency shed, lane-pin inheritance, system holds,
//! reversible redaction, live-zone compression with prefix replay, then tool
//! pruning, schema compaction, roster pinning and order stabilization.
//! Everything downstream (translation, booking, fallback) runs on the
//! prepared body.

use crate::proxy::AppState;
use crate::routed::redaction::maybe_redact_outbound;
use crate::routed::transforms::{
    apply_bytes_stage, apply_compression_and_replay, apply_ctx_request_transforms,
    apply_tool_schema_compaction, merge_routed_compression_report, CtxTransformReport,
};
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;
use std::net::SocketAddr;

/// A routed turn ready to translate: the prepared body plus everything the
/// response side and a possible fallback need to close it out.
pub(crate) struct PreparedTurn {
    pub parsed: Value,
    pub ctx_report: CtxTransformReport,
    pub redacted: bool,
    pub redact_session_key: String,
    pub replay_parked: bool,
    pub overhead_ms: f64,
}

/// Run the preparation chain. `Err` is the response to return directly (today
/// only the concurrency-cap shed); `Ok` hands the prepared turn on.
pub(crate) async fn prepare_turn(
    state: &AppState,
    mut parsed: Value,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    request_id: &str,
    identity_model: Option<&str>,
) -> Result<PreparedTurn, Response> {
    // Apply headroom's CTX request-side transforms (session capture +
    // tool_result offload) so routed models get the same optimizations and
    // searchable archive as the Claude passthrough path, gated on the same
    // flags. Mutates `parsed` before translation.
    let transform_started = std::time::Instant::now();
    let mut ctx_report = apply_ctx_request_transforms(
        state,
        &mut parsed,
        headers,
        client_addr,
        request_id,
        identity_model,
    )
    .await;

    // Conversation-concurrency cap, same contract as the passthrough path:
    // shed fan-out overlap with a 429 the client retries, before compression
    // or translation spend work on a turn that would race the provider's
    // cache commit. The pending entry parked above is popped by the shed
    // call. The key rides on the transform report so the decision uses the
    // pre-transform identity the observer parked, not re-derived bytes.
    if let Some(in_flight) = state.usage_observer.shed_if_over_conversation_cap(
        request_id,
        &ctx_report.conversation_key,
        state.config.max_conversation_concurrency,
    ) {
        crate::observability::proxy_counters::record_concurrency_shed();
        tracing::warn!(
            event = "conversation_concurrency_shed",
            request_id = %request_id,
            conversation_key = %ctx_report.conversation_key,
            in_flight,
            cap = state.config.max_conversation_concurrency,
            "conversation over its concurrency cap; shed with 429 so the client retries against a committed prefix"
        );
        return Err(crate::proxy::conversation_concurrency_shed_response(
            in_flight,
            state.config.max_conversation_concurrency,
        ));
    }

    // Live-zone compression + freeze-replay, on the same flags as the Claude
    // path and in the same order (compress, then replay the cached prefix).
    let session_key = ctx_report.session_key.clone();
    // Behavior stores take the lane, not the session: same-opener streams
    // (subagent fan-out) share the session key but must not share replay
    // trackers, drift baselines, or pins.
    let lane_key = ctx_report.lane_key.clone();
    // A lane switch that continues another lane's message lineage inherits
    // its hold pins before the hold below reads them — same contract as the
    // Claude path, and fully consistent here: the CTX transforms above
    // already ran, so these messages are the snapshot replay will see.
    if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
        crate::proxy::inherit_lane_pins(state, &lane_key, messages, request_id);
    }
    // A routed turn never passes through `forward_http`, so before this it
    // reached its upstream with the volatile `system` lines the holds exist
    // to pin — the client's own working directory and role sentence moving
    // mid-conversation, re-creating the cached prefix from `system` down.
    // Held here, on the Anthropic-shaped body, before any translation
    // restructures it.
    crate::proxy::apply_system_holds(state, &mut parsed, &lane_key, request_id);
    // Reversible redaction, after every stage that reads the client's text
    // (capture, holds) and before every stage that forwards it (compression,
    // offload, replay, translation). Everything upstream-bound from here on
    // carries placeholders; the response arms restore them at the edge.
    // Only translate paths reach this point — the passthrough above returned
    // early — so there is nothing extra to gate on.
    let redacted = state.config.redact_sensitive
        && maybe_redact_outbound(&state.redact_store, &session_key, &mut parsed, request_id);
    // Kept aside: a later `session_key` binding (the Codex turn-state one)
    // shadows the String above, and the fallback below needs this one.
    let redact_session_key = state
        .config
        .redact_sensitive
        .then(|| session_key.clone())
        .unwrap_or_default();
    let compression_report =
        apply_compression_and_replay(state, &mut parsed, headers, request_id, &lane_key);
    let compression_tokens_saved = compression_report.tokens_saved;
    let replay_parked = compression_report.replay_parked;
    let ctx_tokens_saved = merge_routed_compression_report(&mut ctx_report, compression_report);
    tracing::info!(
        event = "routed_compression_accounting",
        request_id = %request_id,
        compression_tokens_saved,
        ctx_transform_tokens_saved = ctx_tokens_saved,
        "routed-model savings split by transform scope"
    );
    // `parsed` from here on is what the fallback re-dispatches if the routed
    // upstream refuses the turn: the replay prefix is already parked and the
    // session already captured against this shape, so the second attempt
    // reuses it rather than the client's original. See
    // [`crate::routed::routing::dispatch_route_fallback`].

    // Tool pruning, schema compaction, then order stabilization — the Claude
    // path's closing sequence, and order matters within it: compaction runs
    // once tools are final, and stabilization must follow every other tool
    // mutation so the order recorded is the order the provider caches.
    //
    // Both prune and stabilize are shape-agnostic (they match a tool by name
    // in either the Anthropic or the OpenAI wrapper) and run here on the
    // pre-translation body, which is Anthropic-shaped. `forward_http` gates
    // them to `AnthropicMessages`, but that is a call-site choice rather than
    // a limitation of either function.
    apply_bytes_stage(&mut parsed, |body| {
        if state.config.tool_prune_policy.is_noop() {
            body
        } else {
            crate::proxy::maybe_prune_tools(body, &state.config.tool_prune_policy, request_id)
        }
    });

    let (compacted, compaction_saved) = apply_tool_schema_compaction(&mut parsed);
    if compacted {
        ctx_report
            .transforms_applied
            .push("tool_schema_compaction".to_string());
        ctx_report.tokens_saved += compaction_saved;
    }

    if state.config.cache_pin_tool_roster {
        apply_bytes_stage(&mut parsed, |body| {
            crate::proxy::maybe_pin_tool_roster(
                body,
                &state.roster_pin_state,
                &session_key,
                request_id,
            )
        });
    }

    if state.config.cache_stable_tool_order {
        apply_bytes_stage(&mut parsed, |body| {
            crate::proxy::maybe_stabilize_tool_order(
                body,
                &state.tool_order_state,
                &session_key,
                request_id,
            )
        });
    }

    // Two stages from the Claude path's closing sequence are deliberately
    // absent, because they do not apply rather than because they were missed:
    //
    // - `--context-edit` injects Anthropic's `context_management` block and its
    //   `context-management-2025-06-27` beta header. That is a server-side
    //   feature of the Anthropic API; this path always talks to OpenAI, which
    //   has no equivalent to translate it into.
    // - `--force-1h-cache-ttl` rewrites `cache_control.ttl`, an Anthropic
    //   prompt-caching control. The Responses API has no TTL knob.
    //
    // The OpenAI-side counterpart, `prompt_cache_key`, is injected after
    // translation instead — see below.
    let overhead_ms = transform_started.elapsed().as_secs_f64() * 1000.0;

    Ok(PreparedTurn {
        parsed,
        ctx_report,
        redacted,
        redact_session_key,
        replay_parked,
        overhead_ms,
    })
}
