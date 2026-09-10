//! Request transforms shared by the routed paths.
//!
//! These run before a routed request leaves the proxy: ctx offload, tool
//! schema compaction, prefix replay, and body compression. None of them are
//! specific to local models — codex and cursor traffic goes through the same
//! stages.

use crate::proxy::AppState;
use crate::routed::outcome::count_tools_tokens;
use axum::http::HeaderMap;
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
    /// The stream lane inside the session (session key + system digest).
    /// Same-opener streams share the session key but must not share replay
    /// trackers, drift baselines, or pins — every behavior store downstream
    /// takes this instead of `session_key`. Derived from the body as
    /// received, before any transform below mutates it.
    pub(crate) lane_key: String,
    /// The usage-observer conversation for this turn, derived from the same
    /// pre-transform body as the lane. Carried so a caller that can return
    /// early (the concurrency-cap shed) decides on the key the observer
    /// actually parked, rather than re-deriving it from mutated bytes.
    pub(crate) conversation_key: String,
}

/// Apply headroom's CTX request-side transforms to a routed model's parsed
/// Anthropic body, reusing the same flags/state as the Claude passthrough path
/// (`forward_http`). Runs the passive session capture (read-only) and, when
/// `ctx_offload` is enabled, the tool_result offload — which both feeds
/// `headroom ctx search` and shrinks the request. Mutates `parsed` in place.
///
/// Note: offload rewrites frozen history only on rebuild boundaries (the gate
/// prevents cache thrash), exactly as the Claude path does.
///
/// `identity_model` is the model the *client* asked for, passed only when the
/// cost-aware router rewrote it. Everything keyed on conversation identity —
/// the session key, and through it prefix replay, the roster pin and the tool
/// order store, plus the usage observer's prefix fingerprint — is derived from
/// it, so a rerouted turn stays on the key its conversation already has.
pub(crate) async fn apply_ctx_request_transforms(
    state: &AppState,
    parsed: &mut Value,
    headers: &HeaderMap,
    client_addr: &SocketAddr,
    request_id: &str,
    identity_model: Option<&str>,
) -> CtxTransformReport {
    let mut report = CtxTransformReport::default();
    use crate::cache_stabilization::drift_detector::{
        compute_structural_hash, derive_session_key_with_model, observe_drift, ApiKind,
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
    let session_key = derive_session_key_with_model(
        headers,
        client_addr,
        parsed,
        ApiKind::Anthropic,
        identity_model,
    );
    report.session_key = session_key.clone();

    // Observe cache-prefix drift on the incoming body (before any transform),
    // matching the Claude path's ordering. Runs unconditionally so the
    // `cache_drift_observed` signal (which axis of system/tools/early_messages
    // changed turn-to-turn) is available regardless of which CTX flags are on.
    // A drift means the codex prompt-cache prefix moved this turn.
    let hash = compute_structural_hash(parsed, ApiKind::Anthropic);
    // Per-stream lane inside the session: same-opener streams share the
    // session key but carry different systems. The drift baseline, the
    // usage conversation, and the replay tracker below take the lane so
    // sibling streams stop invalidating each other; ctx/memory stores stay
    // on the session (tenant-scoped recall is shared on purpose).
    let lane_key = crate::cache_stabilization::drift_detector::stream_lane_key(&session_key, &hash);
    report.lane_key = lane_key.clone();
    let drift_dims = observe_drift(&state.drift_state, &lane_key, hash);
    let rebuild_boundary = drift_dims.is_some();

    // CTX-7: park conversation identity + drift dims under the request id so
    // the response side can classify this turn's billed usage against the
    // conversation's previous turn. This is what feeds the re-cache watchdog
    // that `scripts/statusline-cache-health.sh` renders — without it the cache
    // segment simply has nothing to say about routed turns.
    let conversation_key =
        crate::cache_stabilization::usage_observer::conversation_key(parsed, &lane_key);
    report.conversation_key = conversation_key.clone();
    state.usage_observer.begin_request(
        request_id,
        conversation_key,
        Some(session_key.as_str()),
        drift_dims,
        Some(
            crate::cache_stabilization::usage_observer::prefix_fingerprint_with_model(
                parsed,
                identity_model,
            ),
        ),
    );
    // Same tier read as the Claude path: `parsed` is pre-transform here, so
    // this is what the client asked for, before translation reshapes it.
    state.usage_observer.note_client_cache_ttl(
        request_id,
        crate::cache_stabilization::cache_ttl::client_ttl_shape(parsed),
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
                // Volume the fallback would have to absorb: records and bytes
                // that entered the store but no tracker index.
                tracing::info!(
                    event = "codex_ccr_workspace_unresolved",
                    records_skipped = out.records.len(),
                    bytes_skipped = out
                        .records
                        .iter()
                        .map(|r| r.original.len() as u64)
                        .sum::<u64>(),
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

    // Output shaping: verbosity steering. Idempotent — the
    // steering text carries a sentinel prefix — so replaying a prefix that
    // already contains it does not stack.
    // Disabled in cache mode, as on the Claude path: this body is
    // Anthropic-shaped, so steering appends to the same system-prompt tail
    // that carries the provider prefix-cache key.
    if state.config.output_shaper_enabled {
        let shaped = crate::output_shaper::shape_request_for_mode(
            parsed,
            true,
            state.config.verbosity_level,
            &state.config.mode,
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
pub(crate) fn apply_bytes_stage(
    parsed: &mut Value,
    stage: impl FnOnce(bytes::Bytes) -> bytes::Bytes,
) {
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
///
/// `session_key` carries the stream lane (`CtxTransformReport::lane_key`),
/// not the bare session: same-opener streams must not share the replay
/// tracker. Bare session keys (tests, callers without identity) work
/// unchanged — a key with no lane suffix keys exactly one lane.
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
                // `session_key` here is really the lane: `parsed` was already
                // mutated by the CTX transforms above, so the lane cannot be
                // re-derived here and travels on the caller instead. See
                // `apply_compression_and_replay`'s `session_key` parameter.
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

#[cfg(test)]
mod routed_request_tests {
    use super::*;
    use crate::test_support::test_state;
    use axum::http::HeaderValue;

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
        apply_ctx_request_transforms(&state, &mut with_tools, &headers, &addr, "req-test", None)
            .await;
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
        apply_ctx_request_transforms(
            &state,
            &mut without_tools,
            &headers,
            &addr,
            "req-test",
            None,
        )
        .await;
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
        apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test", None).await;
        apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test", None).await;
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
            apply_ctx_request_transforms(&state, &mut body, &headers, &addr, "req-test", None)
                .await;
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
            lane_key: "sess-stale".to_string(),
            conversation_key: "conv-stale".to_string(),
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
}
