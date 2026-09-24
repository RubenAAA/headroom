//! Compression: policy selection, dispatch and decision refinement, the
//! compression stage, and its observations and ratio samples.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

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
    if let Some(ref recorder) = state.probe_recorder
        && compress_tokens_before > 0
    {
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
    // Compression feedback: record per-tool compression patterns for learning.
    if let Some(ref feedback) = state.compression_feedback
        && compress_tokens_saved > 0
    {
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
    if let Some(count) = message_array_length(buffered, endpoint)
        && count > MAX_MESSAGE_ARRAY_LENGTH
    {
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
    let decision = crate::compression_decision::CompressionDecision::decide(
        decision_headers,
        state.config.compression,
        true, // license_allows — no licensing in this binary; see the gate
        has_messages,
    );
    Ok((decision, decision_headers.clone()))
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
