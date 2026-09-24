//! Pre-send observation and rewrites: upstream head, Anthropic body
//! rewrite, wire ledger, fingerprints, the presend seam and finalize
//! pipeline, and outgoing headers.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

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
