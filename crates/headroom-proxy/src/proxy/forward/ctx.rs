//! CTX engine stages: inject and offload, offload records and accounting,
//! the offload boundary, the transform gate, and re-serialization.
//!
//! Moved out of `forward.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Return bundle for [`run_ctx_inject_offload`]: offload records, CCR workspace, latest query, turn number.
#[allow(clippy::type_complexity)]
pub(crate) type CtxInjectOffloadOut<'a> = (
    bool,
    Option<(
        &'a CtxOffloadRuntime,
        Vec<crate::compression::ctx_offload::OffloadRecord>,
    )>,
    Option<(String, Option<String>)>,
    String,
    u32,
    crate::injection_budget::InjectionBudget,
    String,
);

/// CTX inject-engine pass over the buffered value.
///
/// Runs the inject engine's per-request injection. Sets `changed` on
/// injection. Extracted from `run_ctx_inject_offload` without behavior change.
pub(crate) fn run_ctx_engine_inject(
    value: &mut serde_json::Value,
    state: &AppState,
    request_session_key: &str,
    ctx_project: &str,
    injection_budget: &crate::injection_budget::InjectionBudget,
    request_id: &str,
    changed: &mut bool,
) {
    if let Some(engine) = state.ctx_inject.as_ref() {
        let session_key = request_session_key;
        if engine.maybe_inject_for_request(
            value,
            session_key,
            ctx_project,
            injection_budget,
            request_id,
        ) {
            *changed = true;
        }
    }
}

/// Logs the ctx-offload accounting event for converted blocks.
///
/// Pure logging split of `run_ctx_offload_records`. No behavior change.
pub(crate) fn log_ctx_offload_accounting(
    out: &crate::compression::ctx_offload::OffloadOutcome,
    state: &AppState,
    request_id: &str,
    request_lane_key: &str,
    session_key: &str,
    rebuild_boundary: bool,
    history_rewritten: bool,
) {
    if out.blocks_offloaded > 0 || out.blocks_deferred > 0 {
        // CTX-6: offload metrics are recorded by the
        // offload-store worker after both CCR and FTS writes
        // succeed, not here —
        // see ctx/offload_store.rs.
        // Bounded hash list: first 8 offloaded hashes, so the log joins
        // offload to later retrieval (same request_id or session key)
        // without unbounded log growth on huge turns.
        let hashes: Vec<&str> = out
            .records
            .iter()
            .take(8)
            .map(|r| r.hash.as_str())
            .collect();
        tracing::info!(
            event = "ctx_offload_accounting",
            request_id = %request_id,
            session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
            blocks_offloaded = out.blocks_offloaded,
            blocks_deferred = out.blocks_deferred,
            bytes_deferred = out.bytes_deferred,
            // -1 where there is nothing to report, so the
            // field stays a number the audit scripts can
            // read; `?` would print "Some(4)" as a string.
            deferred_min_distance =
                out.deferred_min_distance.map_or(-1, |d| d as i64),
            deferred_max_distance =
                out.deferred_max_distance.map_or(-1, |d| d as i64),
            // What converting the whole backlog would cost
            // in rewritten prefix, against what it frees.
            bytes_after_deepest_deferral = out.bytes_after_deepest_deferral,
            turns_seen = state
                .replay_store
                .turns_seen(request_lane_key)
                .map_or(-1, |t| t as i64),
            window_offloads = out.window_offloads,
            tokens_saved = out.tokens_saved,
            offloaded_hashes = ?hashes,
            rebuild_boundary,
            history_rewritten,
            "ctx_offload considered tool_result blocks"
        );
    }
}

/// CTX offload-records pass over the buffered value.
///
/// Computes the boundary-gated offload policy and runs the offload.
/// Returns the `(runtime, records)` pair when conversions happened (also
/// setting `changed` and accumulating tokens). Extracted from
/// `run_ctx_inject_offload` without behavior change.
/// The identifiers a ctx stage files its records under.
///
/// A narrower set than [`RequestKeys`]: the ctx stages run below the point
/// where the conversation key and the API kind are known.
#[derive(Clone, Copy)]
pub(crate) struct CtxRequestKeys<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) lane_key: &'a str,
    pub(crate) session_key: &'a str,
}

/// The three boundary decisions the ctx stages branch on.
#[derive(Clone, Copy)]
pub(crate) struct CtxBoundaryFlags {
    pub(crate) rebuild_boundary: bool,
    pub(crate) history_rewritten: bool,
    pub(crate) offload_boundary: bool,
}

pub(crate) fn run_ctx_offload_records<'a>(
    value: &mut serde_json::Value,
    state: &'a AppState,
    keys: CtxRequestKeys<'_>,
    flags: CtxBoundaryFlags,
    changed: &mut bool,
    ctx_transform_tokens_saved: &mut i64,
) -> Option<(
    &'a CtxOffloadRuntime,
    Vec<crate::compression::ctx_offload::OffloadRecord>,
)> {
    let CtxRequestKeys {
        request_id,
        lane_key: request_lane_key,
        session_key: request_session_key,
    } = keys;
    let CtxBoundaryFlags {
        rebuild_boundary,
        history_rewritten,
        offload_boundary,
    } = flags;
    if let Some(runtime) = state.ctx_offload.as_ref() {
        // PR-J4: boundary-gated policy — first conversions of
        // frozen-history blocks only ride a rebuild boundary;
        // live-tail blocks and re-applications always pass.
        let session_key = request_session_key;
        let policy = crate::compression::ctx_offload::OffloadPolicy {
            gate: &runtime.gate,
            session_key,
            rebuild_boundary: offload_boundary,
        };
        let out = crate::compression::ctx_offload::offload_anthropic_request(
            value,
            &runtime.config,
            Some(&policy),
        );
        // Large `tool_use` input strings (a Write's `content`)
        // go the same way, under a stricter gate: a first
        // conversion needs the block to be provably unsent, and
        // that is what the replay store's count says.
        let out = if state.config.ctx_offload_tool_use {
            let forwarded_before = state
                .config
                .prefix_replay
                .then(|| state.replay_store.forwarded_message_count(request_lane_key));
            let ccr = runtime.store.ccr();
            let put = |record: &crate::compression::ctx_offload::OffloadRecord| {
                ccr.put(&record.hash, &record.original)
            };
            let tool_use = crate::compression::ctx_offload::offload_tool_use_inputs(
                value,
                &runtime.config,
                &policy,
                forwarded_before.flatten(),
                &put,
            );
            if tool_use.blocks_offloaded > 0 || tool_use.blocks_deferred > 0 {
                tracing::info!(
                    event = "ctx_offload_tool_use",
                    request_id = %request_id,
                    blocks_offloaded = tool_use.blocks_offloaded,
                    blocks_deferred = tool_use.blocks_deferred,
                    bytes_deferred = tool_use.bytes_deferred,
                    tokens_saved = tool_use.tokens_saved,
                    forwarded_before = ?forwarded_before.flatten(),
                    rebuild_boundary = offload_boundary,
                    "ctx_offload considered tool_use inputs"
                );
            }
            let mut out = out;
            out.blocks_offloaded += tool_use.blocks_offloaded;
            out.blocks_deferred += tool_use.blocks_deferred;
            out.bytes_deferred += tool_use.bytes_deferred;
            if let Some(d) = tool_use.deferred_min_distance {
                out.note_deferred_distance(d);
            }
            if let Some(d) = tool_use.deferred_max_distance {
                out.note_deferred_distance(d);
            }
            out.tokens_saved += tool_use.tokens_saved;
            out.records.extend(tool_use.records);
            out
        } else {
            out
        };
        // PR-J5 thrash guard: an I4 violation (frozen-history
        // conversion on a steady-state turn) is a cache-thrash
        // bug — page-worthy, per the Phase J plan §13.
        if !offload_boundary && out.frozen_new_offloads > 0 {
            tracing::warn!(
                event = "ctx_offload_thrash_guard",
                request_id = %request_id,
                frozen_new_offloads = out.frozen_new_offloads,
                "ctx_offload converted frozen history on a non-boundary turn (I4 violation)"
            );
        }
        // Logged whenever a block QUALIFIED, converted or not.
        // `changed()` is only true for conversions, and this
        // event used to sit behind it — so a turn that deferred
        // every candidate logged nothing at all, which reads
        // exactly like a turn with no candidates. Offload can
        // sit idle for an entirely healthy reason (no rebuild
        // boundary yet) and there was no way to tell that from
        // being switched off, which cost an hour of looking for
        // a fault in a working build.
        if out.blocks_offloaded > 0 || out.blocks_deferred > 0 {
            log_ctx_offload_accounting(
                &out,
                state,
                request_id,
                request_lane_key,
                session_key,
                rebuild_boundary,
                history_rewritten,
            );
        }
        if out.changed() {
            *changed = true;
            *ctx_transform_tokens_saved += out.tokens_saved;
            Some((runtime, out.records))
        } else {
            None
        }
    } else {
        None
    }
}

/// CTX inject + offload head: budget, workspace, injection, offload.
///
/// First half of the ctx-gate `Ok(mut value)` arm: session key, project,
/// budget, CCR workspace/tracker, ctx-inject engine, and the offload
/// records computation. Returns `(changed, tokens_saved, proactive, records,
/// workspace, query, turn)` alongside the mutated `value`.
/// Extracted from `forward_http` without behavior change.
pub(crate) fn run_ctx_inject_offload<'a>(
    value: &mut serde_json::Value,
    state: &'a AppState,
    keys: CtxRequestKeys<'_>,
    headers_snapshot: &Option<HeaderMap>,
    flags: CtxBoundaryFlags,
    ctx_transform_tokens_saved: &mut i64,
    proactive_expansion_applied: &mut bool,
) -> CtxInjectOffloadOut<'a> {
    let CtxRequestKeys {
        request_id,
        lane_key: request_lane_key,
        session_key: request_session_key,
    } = keys;
    let CtxBoundaryFlags {
        rebuild_boundary,
        history_rewritten,
        offload_boundary,
    } = flags;
    let _ctx_session_key = request_session_key;
    let mut changed = false;
    // Which project's stores recall reads and offload writes.
    let ctx_project = resolve_ctx_project(
        headers_snapshot.as_ref(),
        value,
        state.config.memory_project_root.as_deref(),
    );
    let ccr_workspace = resolve_ccr_workspace(
        headers_snapshot.as_ref(),
        value,
        state.config.memory_project_root.as_deref(),
    );
    let latest_user_query = latest_user_query(value);
    let turn_number = anthropic_turn_number(value);

    // One ceiling for every stage that appends to this turn.
    // Drawn down in the order the stages run below; without it
    // three independently-capped appenders could inflate the
    // request while each looked small on its own counter.
    let injection_budget = crate::injection_budget::InjectionBudget::for_request(
        state.config.max_injection_bytes,
        request_id,
    );

    if let Some((workspace_key, workspace_label)) = ccr_workspace.as_ref() {
        if maybe_append_ccr_proactive_expansion(
            state,
            value,
            &latest_user_query,
            workspace_key,
            workspace_label.as_deref(),
            turn_number,
            request_id,
            &injection_budget,
        ) {
            changed = true;
            *proactive_expansion_applied = true;
        }
    } else if state.ccr_context_tracker.is_some() {
        tracing::info!(
            request_id = %request_id,
            "CCR Phase 4: workspace unresolved; proactive expansion disabled for this request"
        );
    }

    run_ctx_engine_inject(
        value,
        state,
        request_session_key,
        &ctx_project,
        &injection_budget,
        request_id,
        &mut changed,
    );

    let offload_records = run_ctx_offload_records(
        value,
        state,
        CtxRequestKeys {
            request_id,
            lane_key: request_lane_key,
            session_key: request_session_key,
        },
        CtxBoundaryFlags {
            rebuild_boundary,
            history_rewritten,
            offload_boundary,
        },
        &mut changed,
        ctx_transform_tokens_saved,
    );
    (
        changed,
        offload_records,
        ccr_workspace,
        latest_user_query,
        turn_number,
        injection_budget,
        ctx_project,
    )
}

/// Offload boundary flags plus the prior-thinking drop.
///
/// Computes `history_rewritten` / `offload_boundary` / `forwarded_agreement`
/// and applies the thinking-drop pass on boundary turns. Returns
/// `(buffered, history_rewritten, offload_boundary)`.
/// Extracted from `forward_http` without behavior change.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_offload_boundary(
    buffered: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    state: &AppState,
    request_id: &str,
    request_lane_key: &str,
    replay_original_messages: &Option<Vec<serde_json::Value>>,
    rebuild_boundary: bool,
    pre_boundary_agreement: Option<usize>,
) -> (bytes::Bytes, bool, bool) {
    // History the provider cannot read back is written fresh this turn
    // whatever the proxy sends, so offloading its tool
    // result history covers every kind of history, not just the ones we
    // know how to resume. `history_rewritten` marks any session where a
    // length>1 history will be rewritten: some prefix a past turn cached
    // is garbage now, which quietly strands the turns still downstream.
    // Boundaries fire on `offload_boundary`; the replay stage below
    // checks readiness via the tracker instead. Small sessions are
    // eligible too: most sessions are small now, and 7 of 9 sessions with
    // such sessions held 5% of all cache reads over 2026-09-01/02 with
    // none of their history offloaded.
    let history_rewritten = !rebuild_boundary
        && replay_original_messages.as_deref().is_some_and(|messages| {
            messages.len() > 1
                && state
                    .replay_store
                    .history_will_be_rewritten(request_lane_key, messages)
        });
    let offload_boundary = rebuild_boundary || history_rewritten;

    // Same turns, same reason: the provider writes this prefix fresh, so
    // stripping thinking the model will never read back costs nothing a
    // verbatim copy would have kept. Off a boundary the pass must not run —
    // the replay store carries the stripped bytes forward for every
    // message it has seen. Its own block, not part of the ctx pipeline
    // below: entering that one injects the retrieval tool.
    //
    // And not on every boundary either (savings-ideas-2.md §4.2): the
    // drop rewrites the head, so it runs only where the head is
    // rewritten anyway — rebuild boundary, no tracker, or agreement
    // ending at index 0/1. A tail divergence keeps its head thinking;
    // only the tail is re-billed either way.
    // On a boundary turn this is the reading taken before the
    // invalidation; otherwise the tracker is intact and the live read is
    // the same reading. The log below publishes this one too, so the field
    // and the decision can never disagree.
    let forwarded_agreement = pre_boundary_agreement.or_else(|| {
        replay_original_messages.as_deref().and_then(|messages| {
            state
                .replay_store
                .forwarded_agreement_len(request_lane_key, messages)
        })
    });
    let buffered = if state.config.ctx_drop_prior_thinking
        && offload_boundary
        && matches!(
            endpoint,
            compression::CompressibleEndpoint::AnthropicMessages
        )
        && buffered
            .windows(b"thinking".len())
            .any(|w| w == b"thinking")
        && crate::compression::prior_thinking::thinking_drop_is_free(
            rebuild_boundary,
            forwarded_agreement,
        ) {
        match serde_json::from_slice::<serde_json::Value>(&buffered) {
            Ok(mut value) => {
                let dropped = crate::compression::prior_thinking::drop_prior_thinking(&mut value);
                if dropped.blocks_removed == 0 {
                    buffered
                } else {
                    tracing::info!(
                        event = "prior_thinking_dropped",
                        request_id = %request_id,
                        blocks_removed = dropped.blocks_removed,
                        bytes_removed = dropped.bytes_removed,
                        rebuild_boundary,
                        history_rewritten,
                        // How much of the history the client still shares
                        // with something this session stored. On a rebuild
                        // boundary the provider rewrites from message 0
                        // and this pass is free; on a history rewrite it
                        // may not be, and nothing in the log said which.
                        // Upper bound only — the agreement is on what the
                        // client sent, and the provider cached what we
                        // forwarded. `-1` where there is no tracker, which
                        // is the case with nothing cached to lose.
                        agreed_prefix_len = replay_original_messages
                            .as_deref()
                            .and_then(|m| {
                                state
                                    .replay_store
                                    .agreed_prefix_len(request_lane_key, m)
                            })
                            .map_or(-1_i64, |n| n as i64),
                        // The head as the provider actually holds it. The
                        // field above compares the client's originals; this
                        // one compares what we last sent, which is what got
                        // cached. Where the two disagree this is the one
                        // that decides whether the drop costs anything.
                        forwarded_agreement_len =
                            forwarded_agreement.map_or(-1_i64, |n| n as i64),
                        incoming_msgs = replay_original_messages
                            .as_deref()
                            .map_or(-1_i64, |m| m.len() as i64),
                        "dropped thinking from assistant turns before the last"
                    );
                    match serde_json::to_vec(&value) {
                        Ok(bytes) => axum::body::Bytes::from(bytes),
                        Err(_) => buffered,
                    }
                }
            }
            Err(_) => buffered,
        }
    } else {
        buffered
    };

    (buffered, history_rewritten, offload_boundary)
}

/// What the ctx re-serialization step owes the offload stores.
pub(crate) struct CtxSerializeInputs<'a> {
    pub(crate) offload_records: Option<(
        &'a CtxOffloadRuntime,
        Vec<crate::compression::ctx_offload::OffloadRecord>,
    )>,
    pub(crate) ccr_workspace: Option<(String, Option<String>)>,
    pub(crate) latest_user_query: String,
    pub(crate) turn_number: u32,
    pub(crate) ctx_project: String,
}

/// Re-serializes the ctx-transformed body and records what the transform owes.
///
/// Hands back `buffered` untouched when nothing changed or the re-serialize
/// failed, so a broken write forwards the original bytes. Extracted from
/// `forward_http` without behavior change.
pub(crate) fn finalize_ctx_transformed_body(
    changed: bool,
    value: &serde_json::Value,
    buffered: bytes::Bytes,
    inputs: CtxSerializeInputs<'_>,
    state: &AppState,
    request_id: &str,
) -> bytes::Bytes {
    if !changed {
        return buffered;
    }
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(e) => {
            if let Some((runtime, records)) = &inputs.offload_records {
                runtime.gate.rollback_unstored_records(records);
            }
            tracing::warn!(
                event = "ctx_transform_reserialize_failed",
                request_id = %request_id,
                error = %e,
                "ctx transform re-serialization failed; forwarding original body"
            );
            return buffered;
        }
    };
    if let Some((runtime, records)) = &inputs.offload_records {
        if !runtime.store.persist(records, &inputs.ctx_project) {
            runtime.gate.rollback_unstored_records(records);
            tracing::warn!(
                event = "ctx_offload_backpressure_passthrough",
                request_id = %request_id,
                "forwarding the original request because CTX-3 index work could not be queued"
            );
            return buffered;
        }
        track_offloaded_ccr_records(state, records, &inputs, request_id);
    }
    axum::body::Bytes::from(bytes)
}

/// Hands stored offload records to CCR Phase 4 tracking, when the workspace
/// resolved.
pub(super) fn track_offloaded_ccr_records(
    state: &AppState,
    records: &[crate::compression::ctx_offload::OffloadRecord],
    inputs: &CtxSerializeInputs<'_>,
    request_id: &str,
) {
    if let Some((workspace_key, _)) = inputs.ccr_workspace.as_ref() {
        track_ccr_context_records(
            state,
            records,
            workspace_key,
            &inputs.latest_user_query,
            inputs.turn_number,
            request_id,
        );
    } else if state.ccr_context_tracker.is_some() {
        tracing::info!(
            request_id = %request_id,
            "CCR Phase 4: workspace unresolved; skipping compression tracking"
        );
    }
}

/// Inputs the ctx/offload/memory transform pass reads from `forward_http`.
pub(crate) struct CtxGateInputs<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) request_id: &'a str,
    pub(crate) request_lane_key: &'a str,
    pub(crate) request_session_key: &'a str,
    pub(crate) headers_snapshot: &'a Option<HeaderMap>,
    pub(crate) rebuild_boundary: bool,
    pub(crate) history_rewritten: bool,
    pub(crate) offload_boundary: bool,
}

/// Runs the ctx inject/offload/memory transforms over an Anthropic body.
///
/// Hands back the buffered bytes untouched when the endpoint or the config
/// puts the request out of scope, or when the body will not parse. Extracted
/// from `forward_http` without behavior change.
pub(crate) async fn run_ctx_transform_gate(
    buffered: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    inputs: CtxGateInputs<'_>,
    stage_timer: &mut crate::stage_timer::StageTimer,
    ctx_transform_tokens_saved: &mut i64,
    proactive_expansion_applied: &mut bool,
) -> bytes::Bytes {
    let CtxGateInputs {
        state,
        request_id,
        request_lane_key,
        request_session_key,
        headers_snapshot,
        rebuild_boundary,
        history_rewritten,
        offload_boundary,
    } = inputs;
    if matches!(
        endpoint,
        compression::CompressibleEndpoint::AnthropicMessages
    ) && (state.ctx_inject.is_some() || state.ctx_offload.is_some())
    {
        match serde_json::from_slice::<serde_json::Value>(&buffered) {
            Ok(mut value) => {
                let (
                    mut changed,
                    offload_records,
                    ccr_workspace,
                    latest_user_query,
                    turn_number,
                    injection_budget,
                    ctx_project,
                ) = forward::run_ctx_inject_offload(
                    &mut value,
                    state,
                    forward::CtxRequestKeys {
                        request_id,
                        lane_key: request_lane_key,
                        session_key: request_session_key,
                    },
                    headers_snapshot,
                    forward::CtxBoundaryFlags {
                        rebuild_boundary,
                        history_rewritten,
                        offload_boundary,
                    },
                    ctx_transform_tokens_saved,
                    proactive_expansion_applied,
                );

                forward::inject_memory_tool_definitions(
                    &mut value,
                    endpoint,
                    state,
                    request_id,
                    &mut changed,
                );
                forward::inject_ccr_retrieve_tool(
                    &mut value,
                    endpoint,
                    state,
                    request_id,
                    *ctx_transform_tokens_saved,
                    &mut changed,
                );

                forward::run_output_shaper(&mut value, state, request_id, &mut changed);

                forward::restore_deferred_memory_answer(&mut value, request_id, &mut changed);

                // Memory: search and inject context into user message tail.
                // Runs after output shaping, before final serialization.
                let memory_start = Instant::now();
                forward::inject_memory_context(
                    &mut value,
                    endpoint,
                    state,
                    request_id,
                    headers_snapshot,
                    &injection_budget,
                    &mut changed,
                )
                .await;

                stage_timer.record("memory", memory_start.elapsed().as_secs_f64() * 1000.0);

                forward::finalize_ctx_transformed_body(
                    changed,
                    &value,
                    buffered,
                    forward::CtxSerializeInputs {
                        offload_records,
                        ccr_workspace,
                        latest_user_query,
                        turn_number,
                        ctx_project,
                    },
                    state,
                    request_id,
                )
            }
            Err(_) => buffered,
        }
    } else {
        buffered
    }
}
