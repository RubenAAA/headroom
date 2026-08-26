//! Request transforms shared by the routed paths.
//!
//! These run before a routed request leaves the proxy: ctx offload, tool
//! schema compaction, prefix replay, and body compression. None of them are
//! specific to local models — codex and cursor traffic goes through the same
//! stages.

use crate::proxy::AppState;
use axum::http::HeaderMap;
use crate::routed::outcome::count_tools_tokens;
use serde_json::{json, Value};
use std::net::SocketAddr;

/// What [`apply_ctx_request_transforms`] did, for the request outcome.
///
/// `transforms_applied` uses the same label strings the Claude path feeds to
/// `RequestOutcome`, so one transform reads identically in `/stats` whichever
/// path served it.
#[derive(Debug, Default)]
pub(crate) struct CtxTransformReport {
    pub(crate) transforms_applied: Vec<String>,
    pub(crate) tokens_saved: i64,
    /// The session key the drift detector and offload gate used. Prefix replay
    /// must key off the same one — the Claude path shares a single key across
    /// all three deliberately, so they agree on what "this conversation" is.
    pub(crate) session_key: String,
}

/// Apply headroom's CTX request-side transforms to a routed model's parsed
/// Anthropic body, reusing the same flags/state as the Claude passthrough path
/// (`forward_http`). Runs the passive session capture (read-only) and, when
/// `ctx_offload` is enabled, the tool_result offload — which both feeds
/// `headroom ctx search` and shrinks the request. Mutates `parsed` in place.
///
/// Note: offload rewrites frozen history only on rebuild boundaries (the gate
/// prevents cache thrash), exactly as the Claude path does.
pub(crate) async fn apply_ctx_request_transforms(
    state: &AppState,
    parsed: &mut Value,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    request_id: &str,
) -> CtxTransformReport {
    let mut report = CtxTransformReport::default();
    use crate::cache_stabilization::drift_detector::{
        compute_structural_hash, derive_session_key, observe_drift, ApiKind,
    };

    // PR-E5: volatile-content detector. Pure observer — one WARN per finding
    // for content that busts the cache (timestamps, UUIDs, ID-named fields).
    // Runs on the body as received so the warning names what the client sent,
    // not what our own transforms left behind.
    let findings = crate::cache_stabilization::volatile_detector::detect_volatile_content(
        parsed,
        crate::cache_stabilization::volatile_detector::ApiKind::Anthropic,
    );
    if !findings.is_empty() {
        crate::cache_stabilization::volatile_detector::emit_volatile_warnings(
            &findings, request_id, None, None,
        );
    }

    // Derived from the body as received — this runs before any transform
    // mutates `parsed`, which matters because `derive_session_key`
    // fingerprints the conversation's first message when no
    // `x-headroom-session-id` header is present.
    let session_key = derive_session_key(headers, client_addr, parsed, ApiKind::Anthropic);
    report.session_key = session_key.clone();

    // Observe cache-prefix drift on the incoming body (before any transform),
    // matching the Claude path's ordering. Runs unconditionally so the
    // `cache_drift_observed` signal (which axis of system/tools/early_messages
    // changed turn-to-turn) is available regardless of which CTX flags are on.
    // A drift means the codex prompt-cache prefix moved this turn.
    let hash = compute_structural_hash(parsed, ApiKind::Anthropic);
    let drift_dims = observe_drift(&state.drift_state, &session_key, hash);
    let rebuild_boundary = drift_dims.is_some();

    // CTX-7: park conversation identity + drift dims under the request id so
    // the response side can classify this turn's billed usage against the
    // conversation's previous turn. This is what feeds the re-cache watchdog
    // that `scripts/statusline-cache-health.sh` renders — without it the cache
    // segment simply has nothing to say about routed turns.
    state.usage_observer.begin_request(
        request_id,
        crate::cache_stabilization::usage_observer::conversation_key(parsed, &session_key),
        Some(session_key.as_str()),
        drift_dims,
        Some(crate::cache_stabilization::usage_observer::prefix_fingerprint(parsed)),
    );

    // CTX-2: passive session capture. Read-only — clones the body onto a
    // detached worker; never mutates and never blocks.
    // Which project's ctx stores this turn is captured into and recalled from.
    let ctx_project = crate::proxy::resolve_ctx_project(Some(headers), parsed);
    if let Some(observer) = state.ctx_observer.as_ref() {
        observer.observe(parsed, &session_key, &ctx_project);
    }

    // CCR identity for this turn. All three helpers read the Anthropic
    // `messages` shape, which is exactly what `parsed` still is here.
    let ccr_workspace = crate::proxy::resolve_ccr_workspace(Some(headers), parsed);
    let user_query = crate::proxy::latest_user_query(parsed);
    let turn_number = crate::proxy::anthropic_turn_number(parsed);

    // One ceiling shared by every stage that appends to this turn, same as
    // the Claude path. The routed path runs the same three appenders, so it
    // needs the same combined bound.
    let injection_budget = crate::injection_budget::InjectionBudget::for_request(
        state.config.max_injection_bytes,
        request_id,
    );

    // CCR proactive expansion: pull back previously-offloaded content the
    // query looks like it needs, before anything else touches the body. First
    // in the block on the Claude path too.
    if let Some((workspace_key, workspace_label)) = ccr_workspace.as_ref() {
        if crate::proxy::maybe_append_ccr_proactive_expansion(
            state,
            parsed,
            &user_query,
            workspace_key,
            workspace_label.as_deref(),
            turn_number,
            request_id,
            &injection_budget,
        ) {
            report
                .transforms_applied
                .push("ccr_proactive_expansion".to_string());
        }
    }

    // CTX-4: recall/resume injection. Runs BEFORE offload (matching the
    // Claude path order). Cache-safe by construction — the engine decides
    // once per conversation and replays the exact same bytes into the first
    // user message on every later turn (nothing volatile), so the codex
    // prompt-cache prefix stays byte-stable after the one-time introduction.
    // It never touches `system`/`tools`.
    if let Some(engine) = state.ctx_inject.as_ref() {
        if engine.maybe_inject_for_request(
            parsed,
            &session_key,
            &ctx_project,
            &injection_budget,
            request_id,
        ) {
            report.transforms_applied.push("ctx_inject".to_string());
            tracing::debug!(
                event = "codex_ctx_inject",
                "injected recall/resume block into routed-model request"
            );
        }
    }

    // CTX-3: tool_result offload. Feeds the FTS search store and shrinks the
    // body. Gated on the same `ctx_offload` flag as the Claude path.
    if let Some(runtime) = state.ctx_offload.as_ref() {
        let policy = crate::compression::ctx_offload::OffloadPolicy {
            gate: &runtime.gate,
            session_key: &session_key,
            rebuild_boundary,
        };
        let out = crate::compression::ctx_offload::offload_anthropic_request(
            parsed,
            &runtime.config,
            Some(&policy),
        );
        if out.changed() {
            report.transforms_applied.push("ctx_offload".to_string());
            report.tokens_saved += out.tokens_saved;
            tracing::debug!(
                event = "codex_ctx_offload",
                blocks_offloaded = out.blocks_offloaded,
                blocks_deferred = out.blocks_deferred,
                tokens_saved = out.tokens_saved,
                rebuild_boundary,
                "offloaded tool_result blocks on routed-model request"
            );
            // Record what was offloaded against the workspace so a later turn's
            // proactive expansion can find it. Without this the expansion above
            // has an empty index to consult and can never fire.
            if let Some((workspace_key, _)) = ccr_workspace.as_ref() {
                crate::proxy::track_ccr_context_records(
                    state,
                    &out.records,
                    workspace_key,
                    &user_query,
                    turn_number,
                    request_id,
                );
            } else if state.ccr_context_tracker.is_some() {
                tracing::info!(
                    event = "codex_ccr_workspace_unresolved",
                    "CCR: workspace unresolved; skipping compression tracking"
                );
            }
            runtime.store.persist(out.records, &ctx_project);
        }
    }

    // The routed body is still Anthropic-shaped here — translation runs after
    // — so every stage below uses the Anthropic provider and the Anthropic
    // tool shape, exactly as `forward_http` does for `/v1/messages`.
    const PROVIDER: crate::memory::tool_adapter::Provider =
        crate::memory::tool_adapter::Provider::Anthropic;

    // Memory: inject tool definitions. Without this, a routed model has no way
    // to write memories at all — `--memory` looked enabled and silently did
    // nothing.
    if let Some(handler) = state.memory_handler.as_ref() {
        let handler = handler.lock().await;
        if handler.is_initialized() {
            // A request with no `tools` array still gets the memory tools; the
            // array is created on demand, matching the Claude path.
            let existing: Vec<Value> = parsed
                .get("tools")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let (new_tools, injected) = handler.inject_memory_tools(Some(&existing), PROVIDER);
            if injected {
                if let Some(obj) = parsed.as_object_mut() {
                    obj.insert("tools".to_string(), Value::Array(new_tools));
                    report.transforms_applied.push("memory_tools".to_string());
                    tracing::debug!(
                        event = "codex_memory_tools",
                        "injected memory tool definitions into routed-model request"
                    );
                }
            }
        }
    }

    // CCR: the `headroom_retrieve` tool, so the model can pull back original
    // content by hash from a compression marker. Only extends an existing
    // `tools` array — same as the Claude path, which does not create one here.
    if state.config.ccr_inject_tool {
        if let Some(tools) = parsed.get_mut("tools").and_then(|v| v.as_array_mut()) {
            let already_has = tools
                .iter()
                .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("headroom_retrieve"));
            if !already_has {
                tools.push(json!({
                    "name": "headroom_retrieve",
                    "description": "Retrieve original uncompressed content that was compressed to save tokens. Use this when you need more data than what's shown in compressed tool results. The hash is provided in compression markers like [N items compressed... hash=abc123].",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "hash": {
                                "type": "string",
                                "description": "Hash key from the compression marker (e.g., 'abc123' from hash=abc123)"
                            }
                        },
                        "required": ["hash"]
                    }
                }));
                report.transforms_applied.push("ccr_tool".to_string());
                tracing::debug!(
                    event = "codex_ccr_tool",
                    "injected headroom_retrieve tool into routed-model request"
                );
            }
        }
    }

    // Output shaping: verbosity steering and effort routing. Idempotent — the
    // steering text carries a sentinel prefix — so replaying a prefix that
    // already contains it does not stack.
    if state.config.output_shaper_enabled {
        let shaped = crate::output_shaper::shape_request(
            parsed,
            true,
            state.config.verbosity_level,
            true,
            &state.config.mechanical_effort,
        );
        if shaped.changed {
            report.transforms_applied.extend(shaped.labels.clone());
            tracing::debug!(
                event = "codex_output_shaper",
                labels = ?shaped.labels,
                "shaped routed-model request"
            );
        }
    }

    // Memory: search and append recalled context to the latest user message.
    if let Some(handler) = state.memory_handler.as_ref() {
        let handler = handler.lock().await;
        if handler.is_initialized() {
            if let Some(messages) = parsed.get("messages").and_then(|v| v.as_array()).cloned() {
                let user_id = headers
                    .get("x-headroom-user-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("default");
                if let Some(context) = handler
                    .search_and_format_context(user_id, &messages, None, None, None, None)
                    .await
                {
                    let frozen = parsed
                        .get("system")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let (new_msgs, bytes) =
                        crate::memory::handler::MemoryHandler::append_to_latest_user_tail(
                            &messages, &context, PROVIDER, frozen,
                        );
                    if bytes > 0 {
                        if let Some(msgs) = parsed.get_mut("messages") {
                            *msgs = Value::Array(new_msgs);
                            report.transforms_applied.push("memory_context".to_string());
                            tracing::debug!(
                                event = "codex_memory_context",
                                bytes_appended = bytes,
                                "injected recalled memory into routed-model request"
                            );
                        }
                    }
                }
            }
        }
    }

    report
}

/// Run a `forward_http` stage that works on serialized bytes against the
/// routed path's parsed body.
///
/// The Claude path threads `Bytes` from stage to stage; this one carries a
/// `Value` because it has to hand the body to the translator at the end.
/// Rather than fork each stage, adapt around them — they stay the single
/// implementation, which is what keeps the two paths honest.
///
/// Any serialize/parse failure leaves `parsed` untouched. Every one of these
/// stages already returns its input unchanged when it cannot parse, so
/// preserving that is the same contract.
pub(crate) fn apply_bytes_stage(parsed: &mut Value, stage: impl FnOnce(bytes::Bytes) -> bytes::Bytes) {
    let Ok(body) = serde_json::to_vec(parsed) else {
        return;
    };
    let out = stage(bytes::Bytes::from(body));
    if let Ok(v) = serde_json::from_slice::<Value>(&out) {
        *parsed = v;
    }
}

/// Tool schema compaction: strips `$schema`/`title`/examples from tool
/// definitions.
///
/// Runs after compression and after every tool-injecting stage, which is where
/// the Claude path runs it — so the memory and CCR tools get compacted too
/// rather than being added behind its back.
///
/// Token counts are taken around the call rather than derived from the byte
/// deltas it reports: a bytes/4 rule of thumb is wrong by enough on JSON
/// (punctuation-dense, so tokens run well ahead of bytes/4) that the savings
/// figure would be fiction. Only the `tools` array is counted, not the whole
/// body, and only when the request actually carries tools.
pub(crate) fn apply_tool_schema_compaction(parsed: &mut Value) -> (bool, i64) {
    let tools_tokens_before = count_tools_tokens(parsed);
    let (compacted, modified, before_bytes, after_bytes) =
        crate::tool_schema_compaction::compact_tools(std::mem::take(parsed));
    *parsed = compacted;
    if !modified {
        return (false, 0);
    }
    let saved = (tools_tokens_before - count_tools_tokens(parsed)).max(0);
    tracing::debug!(
        event = "codex_tool_schema_compaction",
        tools_before_bytes = before_bytes,
        tools_after_bytes = after_bytes,
        tokens_saved = saved,
        "compacted tool schemas on routed-model request"
    );
    (true, saved)
}

/// What the compression + replay stage did, for the request outcome.
#[derive(Debug, Default)]
pub(crate) struct CompressionReport {
    pub(crate) transforms_applied: Vec<String>,
    pub(crate) tokens_saved: i64,
    /// Set when the prefix-replay stage parked this turn, so the response side
    /// knows to feed cache tokens back with [`SessionReplayStore::complete`].
    pub(crate) replay_parked: bool,
}

/// Merge routed live-zone compression into the report that is eventually
/// booked. CTX offload has its own scope and telemetry, so its saving must not
/// be carried forward as though the live-zone dispatcher produced it on this
/// turn. This replacement (rather than addition) is what prevents a prior
/// conversation-sized CTX value from being re-emitted in `tok_saved`.
pub(crate) fn merge_routed_compression_report(
    ctx_report: &mut CtxTransformReport,
    compression_report: CompressionReport,
) -> i64 {
    let ctx_tokens_saved = ctx_report.tokens_saved;
    ctx_report.tokens_saved = compression_report.tokens_saved;
    ctx_report
        .transforms_applied
        .extend(compression_report.transforms_applied);
    ctx_tokens_saved
}

/// Live-zone compression and freeze-replay for a routed request, mirroring the
/// `AnthropicMessages` arm of `forward_http`.
///
/// The routed body is still in Anthropic shape at this point — translation to
/// the OpenAI wire format happens after — so the same dispatcher applies, and
/// gating reads the same config fields rather than anything routed-specific.
/// A routed model therefore compresses exactly when a Claude model would:
/// `--compression` (implied by any `--ctx-*` flag) with a `--compression-mode`
/// other than `off`, no `x-headroom-bypass`, and a non-empty `messages`.
///
/// Replay runs after compression and is gated on `--prefix-replay`
/// independently, matching the Claude path. That ordering is the point of the
/// stage: compression rewrites bytes inside the prompt-cache prefix, and replay
/// puts the previously-forwarded bytes back so the provider's cache still hits.
/// Turning compression on without replay moves the prefix every turn — true on
/// both paths, and worth knowing before enabling one without the other.
pub(crate) fn apply_compression_and_replay(
    state: &AppState,
    parsed: &mut Value,
    headers: &HeaderMap,
    request_id: &str,
    session_key: &str,
) -> CompressionReport {
    let mut report = CompressionReport::default();

    let has_messages = parsed
        .get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|a| !a.is_empty());
    let decision = crate::compression_decision::CompressionDecision::decide(
        headers,
        state.config.compression,
        true, // license_allows — same TODO(license) stub as the Claude path
        has_messages,
    );

    // Nothing to do at all: no compression and no replay. Skip the
    // serialize/reparse round trip entirely so the flags-off path stays free.
    if !decision.should_compress && !state.config.prefix_replay {
        return report;
    }

    let body = match serde_json::to_vec(parsed) {
        Ok(b) => bytes::Bytes::from(b),
        Err(e) => {
            tracing::warn!(
                event = "routed_compression_skipped",
                request_id = %request_id,
                error = %e,
                "could not serialize routed body; skipping compression and replay"
            );
            return report;
        }
    };

    // Snapshot the messages as they stand *before* compression: they are the
    // append-only guard's comparison source and next turn's replay key. Taken
    // after the CTX transforms, which is where the Claude path takes it too —
    // `buffered` there has already been rewritten by them.
    let replay_original_messages: Option<Vec<Value>> = if state.config.prefix_replay {
        parsed.get("messages").and_then(|m| m.as_array()).cloned()
    } else {
        None
    };

    let body = if decision.should_compress {
        // PR-E3: the Phase E byte-mutating passes gate on PAYG, with the same
        // enforcement-flag override the Claude path applies.
        let auth_mode = if state.config.auth_mode_policy_enforcement.is_enabled() {
            headroom_core::auth_mode::classify(headers)
        } else {
            headroom_core::auth_mode::AuthMode::Payg
        };
        let routed_ccr_store = state.ccr_store();
        let outcome = crate::compression::compress_anthropic_request(
            &body,
            state.config.compression_mode,
            state.config.cache_control_auto_frozen,
            auth_mode,
            request_id,
            &state.config.exclude_tools,
            // This path injects headroom_retrieve and resolves it on both
            // response arms (`handle_streaming_response` through
            // `sse::ccr_stream`, `handle_buffered_response` directly), so the
            // marker points at a recovery route the model can actually take.
            routed_ccr_store.as_deref(),
        );
        let outcome = crate::compression::apply_cross_turn_dedup(
            outcome,
            &body,
            &state.config,
            "/v1/messages",
            request_id,
        );
        match outcome {
            crate::compression::Outcome::Compressed {
                body: compressed,
                tokens_before,
                tokens_after,
                strategies_applied,
                ..
            } => {
                report.tokens_saved += (tokens_before as i64 - tokens_after as i64).max(0);
                report
                    .transforms_applied
                    .extend(strategies_applied.iter().map(|s| s.to_string()));
                tracing::debug!(
                    event = "routed_compression_applied",
                    request_id = %request_id,
                    tokens_before,
                    tokens_after,
                    "compressed routed-model request"
                );
                compressed
            }
            _ => body,
        }
    } else {
        body
    };

    let body = match replay_original_messages {
        Some(original_messages) => {
            report.replay_parked = true;
            crate::proxy::apply_prefix_replay(
                &state.replay_store,
                session_key,
                request_id,
                original_messages,
                body,
                Some(&state.usage_observer),
                state.started_at.elapsed().as_secs(),
                state.config.cache_tail_breakpoints as usize,
                state.config.strip_system_cache_breakpoints,
            )
        }
        None => body,
    };

    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => *parsed = v,
        Err(e) => {
            // Leave `parsed` as it was — forwarding the pre-compression body is
            // always safe, and is what every failure arm above already does.
            tracing::warn!(
                event = "routed_compression_reparse_failed",
                request_id = %request_id,
                error = %e,
                "compressed routed body did not re-parse; forwarding uncompressed"
            );
            report.tokens_saved = 0;
            report.transforms_applied.clear();
            report.replay_parked = false;
        }
    }

    report
}
