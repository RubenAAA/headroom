//! Request pipeline stages around the send: semantic cache lookup, replay
//! inputs and holds, prefix replay, endpoint rewrite, tool shape, TTL pin,
//! sidecar, image census, output shaper, and upstream selection.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

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
