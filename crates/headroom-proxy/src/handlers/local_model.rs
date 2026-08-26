//! Local model routing handler.
//!
//! Intercepts `/v1/messages` requests whose `model` field matches the
//! configured local model name. Translates Anthropic Messages API format
//! to OpenAI Chat Completions format, forwards to the local upstream,
//! and translates the response back.

use crate::routed::outcome::{
    book_routed_outcome, book_routed_outcome_with_ccr, build_routed_outcome_context,
    RoutedOutcomeContext,
};
use crate::routed::transforms::{
    apply_bytes_stage, apply_compression_and_replay, apply_ctx_request_transforms,
    apply_tool_schema_compaction, merge_routed_compression_report,
};
use crate::openai::response::{openai_to_anthropic_response, responses_stream_to_turn};
use crate::openai::stream::translate_openai_stream_to_anthropic;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::proxy::{forward_http, AppState};

use crate::openai::request::{anthropic_to_openai_request, anthropic_to_openai_responses_request};

use crate::codex::{
    derive_session_uuid, refresh_codex_token, resolve_codex_routing_headers, turn_state_map,
};











fn apply_target_model_override(
    mut body: Value,
    target_model: Option<&str>,
    force_store_false: bool,
    force_stream_true: bool,
) -> Value {
    if let Some(target) = target_model {
        body["model"] = Value::String(target.to_string());
    }
    if force_store_false {
        body["store"] = Value::Bool(false);
    }
    if force_stream_true {
        body["stream"] = Value::Bool(true);
    }
    body
}

/// Handle GET `/v1/models` for Claude Code's gateway model-discovery feature
/// (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`).
///
/// Claude Code reads `id`/`display_name` from `data[]` and drops any entry
/// whose `id` doesn't start with `claude` or `anthropic`, so only exact-match
/// routes (never `*`-suffixed prefix routes, which aren't a single
/// selectable model) with a qualifying `model_prefix` are listed. Operators
/// who want a routed model (Codex, Grok, ...) to show up in `/model` name
/// its route with a `claude-`/`anthropic-` prefix, e.g.
/// `--extra-model-route "claude-grok-4.6=cursor:cursor-grok-4.6-high"`.
pub async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    fn discoverable(id: &str) -> bool {
        id.starts_with("claude") || id.starts_with("anthropic")
    }

    let mut data: Vec<Value> = Vec::new();

    if let Some(local_model) = &state.config.local_model {
        if discoverable(local_model) {
            data.push(json!({
                "id": local_model,
                "display_name": format!("{local_model} (headroom local model)"),
            }));
        }
    }

    for route in &state.config.model_routes {
        if route.prefix_match || !discoverable(&route.model_prefix) {
            continue;
        }
        let display_name = if let Some(cursor_model) = &route.cursor_agent {
            format!("{cursor_model} (via cursor-agent)")
        } else if let Some(upstream) = &route.upstream {
            format!("{} (via {})", route.model_prefix, upstream.authority())
        } else {
            route.model_prefix.clone()
        };
        data.push(json!({
            "id": route.model_prefix,
            "display_name": display_name,
        }));
    }

    axum::Json(json!({ "data": data }))
}

/// Handle POST `/v1/messages` with local model routing.
///
/// 1. Buffer the body
/// 2. Check if `model` matches the configured local model
/// 3. If yes: translate request -> forward to local upstream -> translate response
/// 4. If no: delegate to `forward_http()` (transparent passthrough)
pub async fn handle_messages(
    State(state): State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Clock for the request outcome, started before any work so the recorded
    // latency covers what the client actually waited for.
    let request_started = std::time::Instant::now();
    // One id for this turn, shared by the request outcome and the prefix-replay
    // store — `begin_request` parks under it and the response side hands it
    // back to `complete`, so the two must be the same value.
    let request_id = uuid::Uuid::new_v4().to_string();
    // Parse body to extract model name.
    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON — can't be a model-routed request, delegate to forward_http.
            let mut builder = Request::builder().method(method).uri(uri);
            if let Some(hs) = builder.headers_mut() {
                *hs = headers;
            }
            let req = builder.body(Body::from(body)).expect("valid request");
            return forward_http(state, client_addr, req)
                .await
                .unwrap_or_else(|e| e.into_response());
        }
    };

    let body_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body_model = body_model.as_str();

    // Find a matching route: first check local_model (backward compat),
    // then check model_routes table.
    // A `cursor:` route runs Cursor's agent CLI rather than an HTTP upstream.
    // Checked before the URL matching below, which has nothing to match on: a
    // subprocess transport has no upstream URL.
    let cursor_model = state
        .config
        .model_routes
        .iter()
        .find(|r| r.matches(body_model))
        .and_then(|r| r.cursor_agent.clone());

    if let Some(ref cursor_model) = cursor_model {
        let session_key = crate::cache_stabilization::drift_detector::derive_session_key(
            &headers,
            &client_addr,
            &parsed,
            crate::cache_stabilization::drift_detector::ApiKind::Anthropic,
        );
        return crate::cursor::handler::handle(state, &parsed, &session_key, cursor_model).await;
    }

    let matched = if let (Some(model), Some(upstream)) =
        (&state.config.local_model, &state.config.local_upstream)
    {
        if body_model == model.as_str() {
            Some((upstream.clone(), true, None))
        } else {
            None
        }
    } else {
        None
    };

    let matched = matched.or_else(|| {
        state
            .config
            .model_routes
            .iter()
            .find(|r| r.matches(body_model))
            .and_then(|r| Some((r.upstream.clone()?, r.translate, r.target_model.clone())))
    });

    let (upstream, translate, target_model) = match matched {
        Some((u, t, tm)) => (u.clone(), t, tm),
        None => {
            // No route matched — delegate to standard forwarder.
            let mut builder = Request::builder().method(method).uri(uri);
            if let Some(hs) = builder.headers_mut() {
                *hs = headers;
            }
            let req = match builder.body(Body::from(body)) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        event = "handler_error",
                        handler = "messages_local_model",
                        error = %e,
                        "failed to reconstruct request"
                    );
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::from("internal handler error"))
                        .expect("static response");
                }
            };
            return forward_http(state, client_addr, req)
                .await
                .unwrap_or_else(|e| e.into_response());
        }
    };

    let (upstream_headers, is_chatgpt_auth) =
        resolve_codex_routing_headers(&headers, state.config.codex_auth_file.as_deref());

    if !translate {
        // No translation needed — forward Anthropic format directly to the upstream.
        let upstream_url = format!("{}{}", upstream.as_str().trim_end_matches('/'), uri.path());
        let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
        let full_url = format!("{upstream_url}{query}");

        tracing::info!(
            event = "model_route_passthrough",
            model = %body_model,
            upstream = %full_url,
            "routing to upstream without translation"
        );

        let resp = match state
            .client
            .post(&full_url)
            .headers(upstream_headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    event = "model_route_upstream_error",
                    error = %e,
                    upstream = %full_url,
                    "failed to connect to upstream"
                );
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("upstream error: {e}")))
                    .expect("static response");
            }
        };

        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let resp_headers = resp.headers().clone();
        let body_bytes = resp.bytes().await.unwrap_or_default();

        let mut response = Response::builder().status(status);
        for (name, value) in resp_headers.iter() {
            if !crate::headers::is_response_drop(name) {
                response = response.header(name.clone(), value.clone());
            }
        }
        return response
            .body(Body::from(body_bytes))
            .expect("static response");
    }

    // Apply headroom's CTX request-side transforms (session capture +
    // tool_result offload) so routed models get the same optimizations and
    // searchable archive as the Claude passthrough path, gated on the same
    // flags. Mutates `parsed` before translation.
    let transform_started = std::time::Instant::now();
    let mut ctx_report =
        apply_ctx_request_transforms(&state, &mut parsed, &headers, &client_addr, &request_id)
            .await;

    // Live-zone compression + freeze-replay, on the same flags as the Claude
    // path and in the same order (compress, then replay the cached prefix).
    let session_key = ctx_report.session_key.clone();
    let compression_report =
        apply_compression_and_replay(&state, &mut parsed, &headers, &request_id, &session_key);
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
            crate::proxy::maybe_prune_tools(body, &state.config.tool_prune_policy, &request_id)
        }
    });

    let (compacted, compaction_saved) = apply_tool_schema_compaction(&mut parsed);
    if compacted {
        ctx_report
            .transforms_applied
            .push("tool_schema_compaction".to_string());
        ctx_report.tokens_saved += compaction_saved;
    }

    if state.config.cache_stable_tool_order {
        apply_bytes_stage(&mut parsed, |body| {
            crate::proxy::maybe_stabilize_tool_order(
                body,
                &state.tool_order_state,
                &session_key,
                &request_id,
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

    // Translation path: Anthropic → OpenAI.
    let openai_body = match if target_model.is_some() {
        anthropic_to_openai_responses_request(&parsed, false)
    } else {
        anthropic_to_openai_request(&parsed, true, true)
    } {
        Ok(v) => apply_target_model_override(
            v,
            target_model.as_deref(),
            target_model.is_some(),
            target_model.is_some(),
        ),
        Err(e) => {
            tracing::warn!(
                event = "local_model_translate_error",
                error = %e,
                "failed to translate Anthropic request to OpenAI format"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("translation error"))
                .expect("static response");
        }
    };

    // PR-E4: OpenAI `prompt_cache_key`. Injected *after* translation, because
    // the field belongs to the OpenAI request shape — before translation there
    // is nowhere valid to put it, and Anthropic has no equivalent. This is the
    // one cache-stabilization stage whose natural home on this path is the
    // post-translation body.
    //
    // Same gating as the Claude path's OpenAI arm: PAYG only, and it self-skips
    // when the caller already set a key. A ChatGPT-subscription codex route
    // classifies as subscription, not PAYG, so this is a no-op there by
    // design — those clients are fingerprinted upstream and a synthesised key
    // works against them.
    let mut openai_body = openai_body;
    apply_bytes_stage(&mut openai_body, |body| {
        crate::proxy::maybe_inject_openai_prompt_cache_key(
            body,
            if target_model.is_some() {
                crate::cache_stabilization::openai_cache_key::OpenAiShape::Responses
            } else {
                crate::cache_stabilization::openai_cache_key::OpenAiShape::ChatCompletions
            },
            headroom_core::auth_mode::classify(&headers),
            &request_id,
            if target_model.is_some() {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            },
        )
    });

    let is_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let upstream_is_stream = is_stream || target_model.is_some();
    let downstream_is_stream = is_stream;

    // Upstream may be configured either as the API root (e.g.
    // `https://api.openai.com`) or already including `/v1` (e.g.
    // `https://api.openai.com/v1`, per the --extra-model-route example in
    // config.rs) — strip a trailing `/v1` so we don't double it up into
    // `.../v1/v1/chat/completions`, which OpenAI 404s on.
    let upstream_base = upstream.as_str().trim_end_matches('/');
    let upstream_url = if target_model.is_some() {
        if is_chatgpt_auth && upstream.host_str() == Some("api.openai.com") {
            "https://chatgpt.com/backend-api/codex/responses".to_string()
        } else {
            let base = upstream_base.trim_end_matches("/v1");
            format!("{base}/v1/responses")
        }
    } else {
        let base = upstream_base.trim_end_matches("/v1");
        format!("{base}/v1/chat/completions")
    };

    tracing::info!(
        event = "model_route_translate",
        request_id = %request_id,
        model = %body_model,
        upstream = %upstream_url,
        stream = upstream_is_stream,
        "routing to upstream with format translation"
    );

    // Book this turn through the same outcome funnel `forward_http` uses, so
    // routed spend shows up in /stats, /stats-history, and the dashboard
    // alongside Claude traffic.
    let forwarded_tokens_estimate = serde_json::to_string(&openai_body)
        .ok()
        .map(|body| {
            headroom_core::tokenizer::get_tokenizer(target_model.as_deref().unwrap_or(body_model))
                .count_text(&body) as i64
        })
        .unwrap_or(0);
    let mut outcome_ctx = build_routed_outcome_context(
        &state,
        &parsed,
        &headers,
        target_model.as_deref(),
        body_model,
        ctx_report,
        overhead_ms,
        request_started,
        request_id.clone(),
        // Only hand the response side a store when this turn was actually
        // parked; `complete` on an unparked id is a no-op, but passing `None`
        // keeps the flag-off path from cloning a handle it will never use.
        replay_parked.then(|| state.replay_store.clone()),
        forwarded_tokens_estimate,
    );

    let openai_body_bytes = match serde_json::to_vec(&openai_body) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize OpenAI request"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    let mut upstream_headers = upstream_headers;

    // Session correlation headers and turn-state echo, mirroring the real
    // Codex client (codex-api/src/requests/headers.rs, client.rs).
    let session_key = parsed
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    if is_chatgpt_auth {
        if let Some(key) = &session_key {
            let session_uuid = derive_session_uuid(key);
            if let Ok(val) = http::HeaderValue::from_str(&session_uuid) {
                upstream_headers.insert("session-id", val.clone());
                upstream_headers.insert("thread-id", val);
            }
            let stored = turn_state_map()
                .lock()
                .ok()
                .and_then(|m| m.get(key).cloned());
            if let Some(ts) = stored {
                if let Ok(val) = http::HeaderValue::from_str(&ts) {
                    upstream_headers.insert("x-codex-turn-state", val);
                }
            }
        }
    }

    // Send with retry: refresh the OAuth token once on 401, back off on
    // 429/5xx/transport errors (honoring Retry-After), like the Codex CLI.
    //
    // Bounds come from the same config the Claude path uses, so
    // `--retry-max-attempts` and the backoff window mean one thing across both
    // paths. The 401-refresh is codex-specific and sits outside the budget:
    // it is a credential fix, not a transient failure, and always gets its one
    // shot regardless of how retries are configured.
    let max_attempts = if state.config.retry_enabled {
        state.config.retry_max_attempts.max(1)
    } else {
        1
    };
    let max_delay_ms = state.config.retry_max_delay_ms;
    let mut refreshed = false;
    let mut attempt: u32 = 0;
    let upstream_resp = loop {
        attempt += 1;
        let result = state
            .client
            .post(&upstream_url)
            .headers(upstream_headers.clone())
            .body(openai_body_bytes.clone())
            .send()
            .await;

        match result {
            Ok(r) => {
                let status = r.status();
                if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed && is_chatgpt_auth {
                    if let Some(auth_file) = state.config.codex_auth_file.as_deref() {
                        if let Some(token) = refresh_codex_token(&state.client, auth_file).await {
                            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}"))
                            {
                                upstream_headers.insert(http::header::AUTHORIZATION, val);
                            }
                            refreshed = true;
                            continue;
                        }
                    }
                    break r;
                }
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < max_attempts {
                    let retry_after_uncapped = r
                        .headers()
                        .get(http::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(headroom_core::retry::retry_after_ms_uncapped);
                    if retry_after_uncapped.is_some_and(|delay| delay > max_delay_ms as f64) {
                        tracing::warn!(
                            event = "local_model_retry_after_exceeds_cap",
                            status = status.as_u16(),
                            attempt,
                            max_attempts,
                            retry_after_ms = retry_after_uncapped.unwrap_or_default(),
                            retry_max_delay_ms = max_delay_ms,
                            request_id = %request_id,
                            session_key_hash = %session_key.as_deref().map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
                            "upstream Retry-After exceeds the internal wait cap; returning the response without an early retry"
                        );
                        break r;
                    }
                    let retry_after = retry_after_uncapped;
                    let backoff = retry_after
                        .map(|delay| {
                            std::time::Duration::from_millis(
                                delay.ceil().min(u64::MAX as f64) as u64
                            )
                        })
                        .unwrap_or_else(|| {
                            std::time::Duration::from_millis(crate::proxy::backoff_ms(
                                &state,
                                attempt - 1,
                            ))
                        })
                        .min(std::time::Duration::from_millis(max_delay_ms));
                    tracing::warn!(
                        event = "local_model_upstream_retry",
                        status = status.as_u16(),
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        retry_after_header = r.headers().contains_key(http::header::RETRY_AFTER),
                        delay_source = if retry_after.is_some() { "header" } else { "backoff" },
                        retry_after_clamped = false,
                        request_id = %request_id,
                        session_key_hash = %session_key.as_deref().map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
                        "retrying transient upstream error"
                    );
                    crate::observability::record_upstream_retry(
                        "local_model",
                        crate::observability::retry_reason::from_status(status.as_u16()),
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                break r;
            }
            Err(e) => {
                // Same filter the Claude path uses: a decode or builder error
                // is not transient and gets no retry, only the transport-level
                // ones do. This arm used to retry every error alike, which
                // spent the whole budget re-sending a request that could not
                // succeed and delayed the 502 the caller was owed.
                let is_retryable = crate::proxy::is_retryable_transport_error(&e);
                if is_retryable && attempt < max_attempts {
                    // Was a hardcoded 250ms doubling with no ceiling, which
                    // ignored `retry_base_delay_ms` and could outrun
                    // `retry_max_delay_ms`. Same backoff as every other site now.
                    let backoff = std::time::Duration::from_millis(crate::proxy::backoff_ms(
                        &state,
                        attempt - 1,
                    ));
                    tracing::warn!(
                        event = "local_model_upstream_retry",
                        error = %e,
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        delay_source = "transport_backoff",
                        request_id = %request_id,
                        "retrying failed upstream connection"
                    );
                    crate::observability::record_upstream_retry(
                        "local_model",
                        crate::observability::retry_reason::TRANSPORT,
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                if is_retryable {
                    crate::observability::record_upstream_retry_exhausted(
                        "local_model",
                        crate::observability::retry_reason::TRANSPORT,
                    );
                }
                tracing::warn!(
                    event = "local_model_upstream_error",
                    error = %e,
                    retryable = is_retryable,
                    attempts = attempt,
                    upstream = %upstream_url,
                    "failed to connect to local model upstream"
                );
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("local upstream error: {e}")))
                    .expect("static response");
            }
        }
    };
    if let Some(ctx) = outcome_ctx.as_mut() {
        ctx.upstream_attempts = i64::from(attempt.max(1));
    }

    // Capture the turn-state token for sticky routing on follow-up requests.
    if let Some(key) = &session_key {
        if let Some(ts) = upstream_resp
            .headers()
            .get("x-codex-turn-state")
            .and_then(|v| v.to_str().ok())
        {
            if let Ok(mut map) = turn_state_map().lock() {
                map.insert(key.clone(), ts.to_string());
            }
        }
    }

    let upstream_status = upstream_resp.status();

    // This path injects `headroom_retrieve` (above) and hands compression a
    // CCR store, so it owns resolving the calls the model makes. Present only
    // when the store exists; without it there is nothing to look a hash up in.
    // Scoped off the client's own request: that is where the system prompt
    // with the working directory lives, and the memory partition is keyed on
    // it. The translated body upstream no longer carries it in that shape.
    let routed_memory = crate::proxy::memory_tool_context(
        &state,
        &Some(headers.clone()),
        Some("openai"),
        &serde_json::to_vec(&parsed).map(Bytes::from).unwrap_or_default(),
    )
    .await;
    let ccr = state.ccr_store().map(|store| RoutedCcr {
        store,
        memory: routed_memory,
        client: state.client.clone(),
        upstream_url: upstream_url.clone(),
        headers: upstream_headers.clone(),
        request_body: openai_body_bytes.clone(),
        config: state.config.clone(),
        request_id: request_id.to_string(),
        responses_shape: target_model.is_some(),
    });

    if upstream_status != StatusCode::OK {
        handle_routed_error_response(upstream_resp, upstream_status, outcome_ctx).await
    } else if downstream_is_stream {
        handle_streaming_response(
            upstream_resp,
            &parsed,
            state.codex_rate_limits.clone(),
            outcome_ctx,
            ccr,
        )
        .await
    } else if target_model.is_some() {
        handle_buffered_responses_response(
            upstream_resp,
            &parsed,
            upstream_status,
            outcome_ctx,
            ccr,
        )
        .await
    } else {
        handle_buffered_response(upstream_resp, &parsed, upstream_status, outcome_ctx, ccr).await
    }
}

/// Return a routed upstream failure without translating its status or
/// `Retry-After`. This is especially important when the requested delay is
/// longer than our in-request cap: retrying early violates the upstream's
/// instruction, while converting a 429 stream into a 200 SSE body hides it
/// from the client that can schedule the next request correctly.
async fn handle_routed_error_response(
    upstream_resp: reqwest::Response,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
) -> Response {
    let retry_after = upstream_resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .cloned();
    let body_text = upstream_resp.text().await.unwrap_or_default();
    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome(ctx, None, 0, 0.0, upstream_status.as_u16() as i64);
    }
    tracing::warn!(
        event = "local_model_upstream_error",
        status = upstream_status.as_u16(),
        body = %body_text,
        retry_after_preserved = retry_after.is_some(),
        "local model upstream returned error"
    );
    let mut response = Response::builder().status(upstream_status);
    if let Some(value) = retry_after {
        response = response.header(http::header::RETRY_AFTER, value);
    }
    response
        .body(Body::from(body_text))
        .expect("static response")
}

/// Resolve `headroom_retrieve` on a buffered routed reply, in the upstream's
/// own shape. Returns the resolved response and the usage of the rounds the
/// client never saw.
async fn resolve_routed_ccr(
    response: &Value,
    ccr: &RoutedCcr,
) -> (Value, crate::proxy::CcrRoundUsage) {
    let provider = if ccr.responses_shape {
        "openai_responses"
    } else {
        "openai"
    };
    let Ok(url) = url::Url::parse(&ccr.upstream_url) else {
        tracing::warn!(
            event = "routed_ccr_bad_upstream_url",
            url = %ccr.upstream_url,
            "cannot resolve headroom_retrieve on this turn"
        );
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let body = match serde_json::to_vec(response) {
        Ok(b) => Bytes::from(b),
        Err(_) => return (response.clone(), crate::proxy::CcrRoundUsage::default()),
    };
    let (resolved, usage) = crate::proxy::handle_ccr_response(
        &body,
        &ccr.request_body,
        &url,
        &ccr.client,
        ccr.store.as_ref(),
        &ccr.config,
        &ccr.request_id,
        &ccr.headers,
        provider,
    )
    .await;
    match serde_json::from_slice(&resolved) {
        Ok(v) => (v, usage),
        Err(_) => (response.clone(), usage),
    }
}

/// Run any `memory_*` call the model made, in the upstream's own shape.
///
/// The twin of [`resolve_routed_ccr`]. `handle_memory_response` was already
/// provider-aware — it knows the Responses API keeps its items under `input`
/// rather than `messages` — but only the two Anthropic seams ever called it,
/// so on this path the injected tools had no one to run them.
async fn resolve_routed_memory(
    response: &Value,
    ccr: &RoutedCcr,
) -> (Value, crate::proxy::CcrRoundUsage) {
    let Some(memory) = ccr.memory.as_ref() else {
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let provider = if ccr.responses_shape {
        "openai_responses"
    } else {
        "openai"
    };
    let Ok(url) = url::Url::parse(&ccr.upstream_url) else {
        tracing::warn!(
            event = "routed_memory_bad_upstream_url",
            url = %ccr.upstream_url,
            "cannot resolve a memory tool call on this turn"
        );
        return (response.clone(), crate::proxy::CcrRoundUsage::default());
    };
    let body = match serde_json::to_vec(response) {
        Ok(b) => Bytes::from(b),
        Err(_) => return (response.clone(), crate::proxy::CcrRoundUsage::default()),
    };
    let (resolved, usage) = crate::proxy::handle_memory_response(
        &body,
        &ccr.request_body,
        &url,
        &ccr.client,
        memory,
        &ccr.config,
        &ccr.request_id,
        &ccr.headers,
        provider,
    )
    .await;
    match serde_json::from_slice(&resolved) {
        Ok(v) => (v, usage),
        Err(_) => (response.clone(), usage),
    }
}

/// What the routed response arms need to resolve a `headroom_retrieve` call.
///
/// Assembled once at the dispatch point because the request shape, URL and
/// headers are only in scope there.
pub(crate) struct RoutedCcr {
    pub store: Arc<dyn headroom_core::ccr::CcrStore>,
    /// Memory tools are injected on this path too (see the injection site in
    /// `apply_ctx_request_transforms`), so this path has to run them. Without
    /// it the call streamed on to the client, which has never heard of
    /// `memory_search` and answered `No such tool available: memory_search`.
    pub memory: Option<crate::proxy::MemoryToolContext>,
    pub client: reqwest::Client,
    pub upstream_url: String,
    pub headers: HeaderMap,
    /// The request as translated and sent upstream, in the upstream's shape.
    pub request_body: Bytes,
    pub config: Arc<crate::config::Config>,
    pub request_id: String,
    /// True when the upstream is the Responses API rather than
    /// chat-completions. The two disagree about where tool calls live.
    pub responses_shape: bool,
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic → OpenAI
// ---------------------------------------------------------------------------

/// Buffered (non-streaming) Chat Completions reply, translated back to the
/// Anthropic response shape.
///
/// Resolves `headroom_retrieve` before translating, on the OpenAI shape the
/// upstream actually returned. The streaming arm does the same through
/// `sse::ccr_stream`; both are required, because this path injects the tool
/// and a tool the client cannot run must never leave the proxy.
/// Read a routed turn's body, or hand back the response the caller should
/// return instead.
///
/// A non-OK status books the turn against its own status code before it goes
/// back to the client — the spend is real whether or not the turn succeeded.
/// Both buffered arms need exactly this, and having written it twice is how the
/// two drifted the first time.
async fn read_routed_body(
    upstream_resp: reqwest::Response,
    upstream_status: StatusCode,
    outcome: Option<&RoutedOutcomeContext>,
) -> Result<String, Response> {
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if status != StatusCode::OK {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        if let Some(ctx) = outcome {
            book_routed_outcome(ctx, None, 0, 0.0, status.as_u16() as i64);
        }
        tracing::warn!(
            event = "local_model_upstream_error",
            status = status.as_u16(),
            body = %body_text,
            "local model upstream returned error"
        );
        return Err(Response::builder()
            .status(status)
            .body(Body::from(body_text))
            .expect("static response"));
    }

    upstream_resp.text().await.map_err(|e| {
        tracing::warn!(
            event = "local_model_response_parse_error",
            error = %e,
            "failed to read upstream response body"
        );
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from("failed to read upstream response"))
            .expect("static response")
    })
}

async fn handle_buffered_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let openai_text =
        match read_routed_body(upstream_resp, upstream_status, outcome.as_ref()).await {
            Ok(text) => text,
            Err(response) => return response,
        };
    let openai_body: Value = match serde_json::from_str(&openai_text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "local_model_response_parse_error",
                error = %e,
                "failed to parse OpenAI response JSON"
            );
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("failed to parse upstream response"))
                .expect("static response");
        }
    };

    // Resolve any `headroom_retrieve` the model asked for, before the outcome
    // is booked: the continuation rounds are billed too, and booking the first
    // round's usage as the turn's would under-report the retrieval.
    let (openai_body, ccr_rounds) = match ccr {
        Some(ccr) => {
            let (body, mut rounds) = resolve_routed_ccr(&openai_body, &ccr).await;
            let (body, mem_rounds) = resolve_routed_memory(&body, &ccr).await;
            rounds.absorb(mem_rounds);
            (body, rounds)
        }
        None => (openai_body, crate::proxy::CcrRoundUsage::default()),
    };

    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome_with_ccr(ctx, openai_body.get("usage"), 0, 0.0, 200, ccr_rounds);
    }

    let anthropic_response = openai_to_anthropic_response(&openai_body, original);

    let body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize Anthropic response"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}

async fn handle_buffered_responses_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    upstream_status: StatusCode,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let responses_text =
        match read_routed_body(upstream_resp, upstream_status, outcome.as_ref()).await {
            Ok(text) => text,
            Err(response) => return response,
        };

    let (responses_turn, output_tokens) = responses_stream_to_turn(&responses_text);

    // Resolve before the outcome is booked: continuation rounds are billed
    // too, and booking the first round's usage as the turn's would under-report
    // the retrieval. Same ordering as the chat arm.
    let (resolved, ccr_rounds) = match ccr {
        Some(ccr) => {
            let (body, mut rounds) = resolve_routed_ccr(&responses_turn, &ccr).await;
            let (body, mem_rounds) = resolve_routed_memory(&body, &ccr).await;
            rounds.absorb(mem_rounds);
            (body, rounds)
        }
        None => (responses_turn, crate::proxy::CcrRoundUsage::default()),
    };

    if let Some(ctx) = outcome.as_ref() {
        book_routed_outcome_with_ccr(
            ctx,
            resolved.get("usage"),
            output_tokens as i64,
            0.0,
            200,
            ccr_rounds,
        );
    }

    let anthropic_response =
        crate::sse::ccr_stream::responses_output_as_anthropic_turn(&resolved, original);

    let body_bytes = match serde_json::to_vec(&anthropic_response) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "local_model_serialize_error",
                error = %e,
                "failed to serialize Anthropic responses translation"
            );
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("serialization error"))
                .expect("static response");
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .expect("static response")
}




// ---------------------------------------------------------------------------
// Streaming response translation: OpenAI SSE → Anthropic SSE
// ---------------------------------------------------------------------------

async fn handle_streaming_response(
    upstream_resp: reqwest::Response,
    original: &Value,
    codex_limits: crate::codex_rate_limits::CodexRateLimitStore,
    outcome: Option<RoutedOutcomeContext>,
    ccr: Option<RoutedCcr>,
) -> Response {
    let original_model = original
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Quota headers ride on the response envelope and are gone once the body
    // is taken, so read them first. Nothing downstream depends on this.
    let quota_seen_in_headers =
        codex_limits.record_headers(&original_model, upstream_resp.headers());

    let stream = upstream_resp.bytes_stream();
    let translated_stream = translate_openai_stream_to_anthropic(
        stream,
        original_model,
        codex_limits,
        quota_seen_in_headers,
        outcome,
    );

    // The translator has already put the turn into the Anthropic event
    // vocabulary, which is the one the client reads and the one the stream
    // rewriter speaks — so the same rewriter that serves the Claude path
    // serves this one. Only the continuation differs: it has to go back to
    // the routed upstream in its own shape.
    let body = match ccr {
        Some(ccr) => {
            let anthropic_request = original.clone();
            let ctx = crate::sse::ccr_stream::CcrStreamContext {
                client: ccr.client,
                upstream_url: match url::Url::parse(&ccr.upstream_url) {
                    Ok(u) => u,
                    Err(e) => {
                        // Unparseable upstream means no continuation is
                        // possible. Stream on untouched rather than fail the
                        // turn; the client is no worse off than before.
                        tracing::warn!(
                            event = "routed_ccr_bad_upstream_url",
                            error = %e,
                            url = %ccr.upstream_url,
                            "cannot resolve headroom_retrieve on this turn"
                        );
                        return streaming_body_response(axum::body::Body::from_stream(
                            translated_stream,
                        ));
                    }
                },
                outgoing_headers: ccr.headers,
                forwarded_request: ccr.request_body,
                ccr_store: ccr.store,
                config: ccr.config,
                request_id: ccr.request_id,
                shape: if ccr.responses_shape {
                    crate::sse::ccr_stream::CcrShape::RoutedResponses { anthropic_request }
                } else {
                    crate::sse::ccr_stream::CcrShape::RoutedChat { anthropic_request }
                },
                // Memory tools ARE injected into routed requests — see the
                // `codex_memory_tools` site in `apply_ctx_request_transforms`.
                // This said otherwise and passed `None`, so the rewriter did
                // not own the block, `memory_search` streamed through to a
                // client that has never heard of it, and the turn died with
                // `No such tool available: memory_search`.
                //
                // Nothing types the agreement between the two sites, so it
                // rests on a gate relation: `memory_tool_context` asks only
                // that the handler exist and be initialized, while injection
                // asks that *and* that the tool array grew. The rewriter's
                // view is therefore a superset of what was injected, and the
                // only way the two can disagree is the harmless way — the
                // rewriter watches for a tool the model was never handed.
                // Narrowing this gate would reopen the bug above.
                memory: ccr.memory,
            };
            let (rewritten, _usage) =
                crate::sse::ccr_stream::rewrite_anthropic_stream(translated_stream, ctx);
            axum::body::Body::from_stream(rewritten)
        }
        None => axum::body::Body::from_stream(translated_stream),
    };

    streaming_body_response(body)
}

/// The SSE response envelope every routed streaming reply uses.
fn streaming_body_response(body: axum::body::Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("static response")
}








// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_state;
    use crate::routed::transforms::{CompressionReport, CtxTransformReport};
    use base64::Engine as _;
    use crate::codex::{codex_user_agent, generate_traceparent};
    use axum::http::HeaderValue;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;



    fn conversation(tail: &str) -> Value {
        json!({
            "model": "claude-codex-5.6",
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": tail}
            ]
        })
    }

    /// Compression is off unless the operator turned it on — the routed path
    /// must not start rewriting bodies that the Claude path would forward
    /// untouched.
    #[test]
    fn routed_compression_is_off_by_default() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = false;
        });
        let mut body = conversation("hello");
        let before = body.clone();
        let report =
            apply_compression_and_replay(&state, &mut body, &HeaderMap::new(), "req-1", "sess-1");
        assert_eq!(body, before, "body must forward byte-equal");
        assert_eq!(report.tokens_saved, 0);
        assert!(!report.replay_parked);
    }

    /// `x-headroom-bypass` wins over the config, same as on the Claude path.
    #[test]
    fn routed_compression_honours_the_bypass_header() {
        let state = test_state(|c| {
            c.compression = true;
            c.compression_mode = crate::config::CompressionMode::AllMessages;
            c.prefix_replay = false;
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-headroom-bypass", HeaderValue::from_static("true"));
        let mut body = conversation("hello");
        let before = body.clone();
        let report = apply_compression_and_replay(&state, &mut body, &headers, "req-2", "sess-2");
        assert_eq!(body, before);
        assert!(report.transforms_applied.is_empty());
    }

    /// A conversation whose client-side `cache_control` markers move each turn
    /// — exactly the churn the replay stage exists to absorb. The stage
    /// rewrites these, so the forwarded bytes differ from the input and the
    /// replay assertion below cannot pass vacuously.
    fn conversation_with_cache_markers(marker_on: usize) -> Value {
        let mut messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "first turn"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "reply"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "second turn"}]}),
        ];
        messages[marker_on]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        json!({"model": "claude-codex-5.6", "messages": messages})
    }

    /// The point of the stage: turn two forwards the bytes turn one forwarded,
    /// so the provider's prompt-cache prefix does not move even though the
    /// client shuffled its `cache_control` breakpoint in between.
    #[test]
    fn prefix_replay_reuses_the_previously_forwarded_prefix() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = true;
        });
        let headers = HeaderMap::new();

        let mut turn1 = conversation_with_cache_markers(0);
        let raw1 = turn1["messages"].as_array().unwrap().clone();
        let r1 = apply_compression_and_replay(&state, &mut turn1, &headers, "req-a", "sess-x");
        assert!(r1.replay_parked, "turn one must park for turn two");
        let forwarded1 = turn1["messages"].as_array().unwrap().clone();
        assert_ne!(
            forwarded1, raw1,
            "guard: the stage must rewrite something, else the assertion below proves nothing"
        );

        // Close the turn out the way a clean stream does, then extend the
        // conversation append-only with the marker moved, as a client does.
        state.replay_store.complete("req-a", 1_000, 0);
        let mut turn2 = conversation_with_cache_markers(2);
        turn2["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": [{"type": "text", "text": "third"}]}));
        apply_compression_and_replay(&state, &mut turn2, &headers, "req-b", "sess-x");

        let forwarded2 = turn2["messages"].as_array().unwrap();
        assert_eq!(
            forwarded2.len(),
            forwarded1.len() + 1,
            "only the new message should be appended"
        );
        // Compared with `cache_control` stripped, matching the contract: the
        // replayed prefix is byte-identical in *content*, while the single
        // ephemeral breakpoint is deliberately re-placed on the new last
        // message each turn. That re-placement is the mechanism keeping the
        // marker count bounded — Anthropic hard-errors above four.
        assert_eq!(
            strip_cache_control(&forwarded2[..forwarded1.len()]),
            strip_cache_control(&forwarded1),
            "the replayed prefix must match what turn one forwarded"
        );
        let markers = forwarded2
            .iter()
            .filter(|m| {
                m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
            })
            .count();
        assert_eq!(markers, 1, "markers must not accumulate across turns");
    }

    fn strip_cache_control(messages: &[Value]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(blocks) = m["content"].as_array_mut() {
                    for b in blocks {
                        if let Some(obj) = b.as_object_mut() {
                            obj.remove("cache_control");
                        }
                    }
                }
                m
            })
            .collect()
    }

    /// The append-only guard: when an earlier message actually changed, the
    /// stored prefix no longer describes this conversation and replaying it
    /// would forward content the client did not send.
    #[test]
    fn prefix_replay_declines_when_history_was_rewritten() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = true;
        });
        let headers = HeaderMap::new();

        let mut turn1 = conversation_with_cache_markers(0);
        apply_compression_and_replay(&state, &mut turn1, &headers, "req-a", "sess-y");
        state.replay_store.complete("req-a", 1_000, 0);

        // Rewrite history rather than appending to it.
        let mut turn2 = conversation_with_cache_markers(0);
        turn2["messages"][0]["content"][0]["text"] = json!("a different first turn");
        let expected_tail = turn2["messages"][0].clone();
        apply_compression_and_replay(&state, &mut turn2, &headers, "req-b", "sess-y");

        assert_eq!(
            turn2["messages"][0]["content"][0]["text"], expected_tail["content"][0]["text"],
            "the client's own first message must survive, not turn one's"
        );
    }

    /// CCR's retrieve tool only extends an existing `tools` array — the Claude
    /// path does not create one here, and a routed request must not either.
    #[tokio::test]
    async fn ccr_tool_is_injected_only_when_the_request_carries_tools() {
        let state = test_state(|c| c.ccr_inject_tool = true);
        let headers = HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();

        let mut with_tools = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        });
        apply_ctx_request_transforms(&state, &mut with_tools, &headers, &addr, "req-test").await;
        let names: Vec<&str> = with_tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"headroom_retrieve"), "got {names:?}");

        let mut without_tools = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}]
        });
        apply_ctx_request_transforms(&state, &mut without_tools, &headers, &addr, "req-test").await;
        assert!(
            without_tools.get("tools").is_none(),
            "a request with no tools array must not grow one"
        );
    }

    /// Injecting the same tool twice would send the model a duplicate
    /// definition and move the cached prefix every turn.
    #[tokio::test]
    async fn ccr_tool_injection_is_idempotent() {
        let state = test_state(|c| c.ccr_inject_tool = true);
        let headers = HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let mut body = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        });
        apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test").await;
        apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test").await;
        let count = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "headroom_retrieve")
            .count();
        assert_eq!(count, 1);
    }

    /// `--ccr-inject-tool` defaults to *true*, so the retrieve tool is the one
    /// stage that lands without being asked for — on both paths. Everything
    /// else here stays dormant until its flag is set.
    #[tokio::test]
    async fn only_ccr_injects_under_default_config() {
        let state = test_state(|_| {});
        assert!(
            state.config.ccr_inject_tool,
            "guard: this test encodes the shipped default"
        );
        let headers = HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let mut body = json!({
            "model": "claude-codex-5.6",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}]
        });
        let report =
            apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test").await;
        assert_eq!(report.transforms_applied, vec!["ccr_tool".to_string()]);
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    /// Ordering guarantee: compaction runs after injection, so an injected
    /// tool is compacted like any other rather than slipping in behind it.
    #[test]
    fn tool_schema_compaction_strips_injected_tool_noise() {
        let mut body = json!({
            "model": "claude-codex-5.6",
            "tools": [{
                "name": "headroom_retrieve",
                "input_schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "title": "Retrieve",
                    "type": "object",
                    "properties": {"hash": {"type": "string"}}
                }
            }]
        });
        let (changed, saved) = apply_tool_schema_compaction(&mut body);
        assert!(changed);
        assert!(saved > 0, "stripping schema noise should save tokens");
        let schema = &body["tools"][0]["input_schema"];
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
        assert_eq!(schema["properties"]["hash"]["type"], "string");
    }

    /// A late MCP handshake splicing tools into the middle of the array moves
    /// the cached prefix. Stabilization replays last turn's order and appends
    /// genuinely-new tools at the end.
    #[test]
    fn tool_order_is_stable_when_a_late_tool_appears() {
        let store = crate::cache_stabilization::tool_order::ToolOrderStore::default();
        let tools = |names: &[&str]| {
            json!({
                "model": "claude-codex-5.6",
                "tools": names
                    .iter()
                    .map(|n| json!({"name": n, "input_schema": {"type": "object"}}))
                    .collect::<Vec<_>>()
            })
        };
        let order_of = |v: &Value| -> Vec<String> {
            v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };

        let mut turn1 = tools(&["Read", "Write"]);
        apply_bytes_stage(&mut turn1, |b| {
            crate::proxy::maybe_stabilize_tool_order(b, &store, "sess-order", "r1")
        });
        assert_eq!(order_of(&turn1), vec!["Read", "Write"]);

        // An MCP server registers and the client splices its tool in first.
        let mut turn2 = tools(&["mcp__late__tool", "Read", "Write"]);
        apply_bytes_stage(&mut turn2, |b| {
            crate::proxy::maybe_stabilize_tool_order(b, &store, "sess-order", "r2")
        });
        assert_eq!(
            order_of(&turn2),
            vec!["Read", "Write", "mcp__late__tool"],
            "the established prefix must keep its order, new tools go last"
        );
    }

    /// `prompt_cache_key` belongs to the OpenAI request shape, so it is
    /// injected after translation — and only for PAYG callers.
    #[test]
    fn prompt_cache_key_is_injected_only_for_payg() {
        use crate::cache_stabilization::openai_cache_key::OpenAiShape;
        let inject = |auth| {
            let mut body = json!({"model": "gpt-5.6-luna", "input": [], "store": false});
            apply_bytes_stage(&mut body, |b| {
                crate::proxy::maybe_inject_openai_prompt_cache_key(
                    b,
                    OpenAiShape::Responses,
                    auth,
                    "r1",
                    "/v1/responses",
                )
            });
            body
        };
        assert!(
            inject(headroom_core::auth_mode::AuthMode::Payg)
                .get("prompt_cache_key")
                .is_some(),
            "a PAYG caller should get a synthesised key"
        );
        assert!(
            inject(headroom_core::auth_mode::AuthMode::Subscription)
                .get("prompt_cache_key")
                .is_none(),
            "a subscription caller is fingerprinted upstream; injecting would work against them"
        );
    }

    /// The bytes adapter must leave the body alone when a stage hands back
    /// something unparseable, rather than dropping the request on the floor.
    #[test]
    fn bytes_stage_adapter_preserves_the_body_on_failure() {
        let mut body = json!({"model": "m", "messages": []});
        let before = body.clone();
        apply_bytes_stage(&mut body, |_| bytes::Bytes::from_static(b"not json"));
        assert_eq!(body, before);
    }

    /// Compression is wired to a live dispatcher, not just gated correctly:
    /// a body with a compressible block must come back smaller.
    #[test]
    fn routed_compression_actually_shrinks_a_compressible_body() {
        let state = test_state(|c| {
            c.compression = true;
            c.compression_mode = crate::config::CompressionMode::AllMessages;
            c.prefix_replay = false;
        });
        // Repeated whitespace-heavy log output: the shape the live-zone
        // strategies are built for.
        let noisy = "ERROR   module.rs:12    something failed\n".repeat(400);
        let mut body = json!({
            "model": "claude-codex-5.6",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": noisy},
                    {"type": "text", "text": "what went wrong?"}
                ]
            }]
        });
        let before = serde_json::to_string(&body).unwrap().len();
        let report =
            apply_compression_and_replay(&state, &mut body, &HeaderMap::new(), "req-c", "sess-c");
        let after = serde_json::to_string(&body).unwrap().len();
        assert!(
            report.tokens_saved > 0,
            "expected a real saving, got {} (bytes {before} -> {after})",
            report.tokens_saved
        );
        assert!(after < before, "body should shrink: {before} -> {after}");
        assert!(
            !report.transforms_applied.is_empty(),
            "the strategy that ran should be named in the outcome"
        );
    }

    /// Regression for observation item 15: a conversation-sized CTX saving
    /// used to survive into later routed turns even when the live-zone
    /// dispatcher did nothing. The booked value must be this turn's measured
    /// compression result, including zero, never the incoming CTX value.
    #[test]
    fn routed_booking_does_not_reemit_ctx_savings_without_compression() {
        let mut ctx_report = CtxTransformReport {
            transforms_applied: vec!["ctx_offload".to_string()],
            tokens_saved: 4_522,
            session_key: "sess-stale".to_string(),
        };
        let compression_report = CompressionReport::default();

        let separately_measured_ctx =
            merge_routed_compression_report(&mut ctx_report, compression_report);

        assert_eq!(separately_measured_ctx, 4_522);
        assert_eq!(
            ctx_report.tokens_saved, 0,
            "no routed compression means the outcome must book zero, not a stale CTX value"
        );
        assert_eq!(ctx_report.transforms_applied, vec!["ctx_offload"]);
    }

    /// A different session must not inherit another's prefix.
    #[test]
    fn prefix_replay_is_scoped_to_its_session() {
        let state = test_state(|c| {
            c.compression = false;
            c.compression_mode = crate::config::CompressionMode::Off;
            c.prefix_replay = true;
        });
        let headers = HeaderMap::new();
        let mut a = conversation("session a");
        apply_compression_and_replay(&state, &mut a, &headers, "req-a", "sess-a");
        state.replay_store.complete("req-a", 1_000, 0);

        let mut b = conversation("session b");
        let before = b.clone();
        apply_compression_and_replay(&state, &mut b, &headers, "req-b", "sess-b");
        assert_eq!(
            crate::cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(&b),
            crate::cache_stabilization::prefix_replay::canonicalize_for_prefix_compare(&before),
            "a cold session may gain a marker but must not replay session a"
        );
        let after_messages = b["messages"].as_array().unwrap();
        // Wrapped to block form (every eligible string is), so compare content
        // rather than bytes: session a's prefix would show up as other text.
        assert_eq!(after_messages[0]["content"][0]["text"], "first turn");
        assert_eq!(after_messages[1]["content"][0]["text"], "reply");
        assert!(
            after_messages[..2]
                .iter()
                .all(|m| m["content"][0]["cache_control"].is_null()),
            "history must not carry the breakpoint — it belongs on the newest message"
        );
        assert_eq!(after_messages[2]["content"][0]["text"], "session b");
        assert_eq!(
            after_messages[2]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
    }

    /// Translator wired to real trackers, so a test can assert on what the
    /// outcome funnel recorded rather than on the events it emitted.
    #[test]
    fn apply_target_model_override_rewrites_model() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi"});
        let output = apply_target_model_override(body, Some("gpt-5.5"), false, false);
        assert_eq!(output["model"], "gpt-5.5");
        assert_eq!(output["input"], "hi");
    }

    #[test]
    fn apply_target_model_override_leaves_model_when_absent() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi"});
        let output = apply_target_model_override(body, None, false, false);
        assert_eq!(output["model"], "claude-codex-5.5");
        assert_eq!(output["input"], "hi");
    }

    #[test]
    fn apply_target_model_override_forces_store_false_when_requested() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi", "store": true});
        let output = apply_target_model_override(body, Some("gpt-5.5"), true, false);
        assert_eq!(output["model"], "gpt-5.5");
        assert_eq!(output["store"], false);
    }

    #[test]
    fn apply_target_model_override_forces_stream_true_when_requested() {
        let body = json!({"model": "claude-codex-5.5", "input": "hi", "stream": false});
        let output = apply_target_model_override(body, Some("gpt-5.5"), false, true);
        assert_eq!(output["stream"], true);
    }

    #[test]
    fn codex_user_agent_matches_cli_format() {
        let ua = codex_user_agent(None);
        assert!(ua.starts_with("codex_cli_rs/"));
        assert!(ua.contains('('));
        let tp = generate_traceparent();
        assert_eq!(tp.len(), 55);
        assert!(tp.starts_with("00-"));
        assert!(tp.ends_with("-01"));
    }

    #[test]
    fn derive_session_uuid_is_stable_and_uuid_shaped() {
        let a = derive_session_uuid("user_abc_session_123");
        let b = derive_session_uuid("user_abc_session_123");
        let c = derive_session_uuid("user_abc_session_456");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|&ch| ch == '-').count(), 4);
    }

    #[test]
    fn resolve_codex_routing_headers_detects_chatgpt_jwt_auth() {
        let payload = json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-from-jwt",
            }
        });
        let token = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let temp_dir = tempfile::tempdir().unwrap();
        let auth_file = temp_dir.path().join("auth.json");
        std::fs::write(
            &auth_file,
            json!({
                "tokens": {
                    "access_token": token,
                }
            })
            .to_string(),
        )
        .unwrap();

        let headers = HeaderMap::new();
        let (upstream_headers, is_chatgpt_auth) =
            resolve_codex_routing_headers(&headers, auth_file.to_str());

        assert!(is_chatgpt_auth);
        assert_eq!(
            upstream_headers.get(http::header::AUTHORIZATION),
            Some(&HeaderValue::from_str(&format!("Bearer {}", token)).unwrap())
        );
        assert_eq!(
            upstream_headers
                .get("ChatGPT-Account-ID")
                .and_then(|v| v.to_str().ok()),
            Some("acct-from-jwt")
        );
    }
}
