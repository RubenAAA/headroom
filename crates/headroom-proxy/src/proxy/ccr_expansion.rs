//! CCR proactive expansion and tracking: resolve the workspace, query the
//! tracker, append expansions to the latest user turn.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Resolve the workspace used by CCR Phase 4 tracking/expansion.
///
/// Mirrors Python's tier order: `x-headroom-project-id` →
/// `x-headroom-cwd` → system-prompt `cwd:` line. Returns `None` when no
/// stable workspace is available; callers fail closed rather than tracking
/// under a shared empty workspace.
pub(crate) fn resolve_ccr_workspace(
    headers: Option<&HeaderMap>,
    body: &serde_json::Value,
    project_root_override: Option<&str>,
) -> Option<(String, Option<String>)> {
    let system_prompt = crate::memory::router::extract_system_prompt(body);
    let ctx = crate::memory::router::RequestContext {
        headers: header_map_to_lowercase_strings(headers),
        system_prompt,
        base_user_id: String::new(),
        project_root_override: project_root_override.map(str::to_string),
    };
    crate::memory::router::ProjectResolver::resolve(&ctx).map(|(key, display)| (key, Some(display)))
}

/// Resolve the project directory used to pick this request's ctx stores.
///
/// Same tier order as [`resolve_ccr_workspace`], but returns the canonical
/// directory rather than a display key, because that is what
/// `hash_project_dir_canonical` names the DB files after.
///
/// Falls back to [`crate::ctx::projects::UNRESOLVED_PROJECT`] instead of
/// failing closed: capture and recall have to go *somewhere*, and the shared
/// bucket is where every request already landed before sharding existed.
pub(crate) fn resolve_ctx_project(
    headers: Option<&HeaderMap>,
    body: &serde_json::Value,
    project_root_override: Option<&str>,
) -> String {
    let ctx = crate::memory::router::RequestContext {
        headers: header_map_to_lowercase_strings(headers),
        system_prompt: crate::memory::router::extract_system_prompt(body),
        base_user_id: String::new(),
        project_root_override: project_root_override.map(str::to_string),
    };
    crate::memory::router::ProjectResolver::resolve_project_dir(&ctx)
        .unwrap_or_else(|| crate::ctx::projects::UNRESOLVED_PROJECT.to_string())
}

pub(crate) fn latest_user_query(body: &serde_json::Value) -> String {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| {
            messages.iter().rev().find_map(|msg| {
                if msg.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                    return None;
                }
                match msg.get("content") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(serde_json::Value::Array(blocks)) => blocks.iter().find_map(|block| {
                        (block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                            .then(|| {
                                block
                                    .get("text")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .flatten()
                    }),
                    _ => None,
                }
            })
        })
        .unwrap_or_default()
}

pub(crate) fn anthropic_turn_number(body: &serde_json::Value) -> u32 {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| messages.len().min(u32::MAX as usize) as u32)
        .unwrap_or(0)
}

pub(super) fn append_context_to_latest_user_turn(
    body: &mut serde_json::Value,
    expansion_text: String,
) -> bool {
    if expansion_text.is_empty() {
        return false;
    }
    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|msg| msg.get("role").and_then(serde_json::Value::as_str) == Some("user"))
    else {
        return false;
    };

    match message.get_mut("content") {
        Some(serde_json::Value::String(s)) => {
            s.push_str("\n\n");
            s.push_str(&expansion_text);
            true
        }
        Some(serde_json::Value::Array(blocks)) => {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": expansion_text,
            }));
            true
        }
        _ => {
            message["content"] = serde_json::Value::String(expansion_text);
            true
        }
    }
}
/// CCR expansion pre-gates: non-empty query, flag on, not cache mode, and
/// not a compact-continuation summary (already context; expanding it
/// re-adds stale session state to later turns). Each gate logs its
/// distinct `ccr_expansion_evaluation` outcome.
/// Extracted from `check_ccr_expansion_prereqs` without behavior change.
pub(super) fn expansion_gate_passes(state: &AppState, user_query: &str, request_id: &str) -> bool {
    if user_query.trim().is_empty()
        || !state.config.ccr_proactive_expansion
        || crate::modes::is_cache_mode(Some(&state.config.mode))
    {
        // Shadow signal for the expansion re-enable decision: how often the
        // gate alone keeps expansion out of play. `flag` separates the
        // switched-off volume from the rest.
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "skipped_gate",
            flag = state.config.ccr_proactive_expansion,
            "ccr: expansion gated out before consulting the tracker"
        );
        return false;
    }
    // Compact-continuation summaries are already context; expanding them
    // re-adds stale session state to later turns (port of upstream skipping
    // proactive tracking for `looks_like_claude_code_compact_summary`).
    if headroom_core::ccr::context_tracker::looks_like_compact_summary(&[user_query]) {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "compact_summary",
            "ccr: skipping proactive expansion for a compact-continuation summary"
        );
        return false;
    }
    true
}

/// Load the CCR tracker plus the offload runtime, both of which must be
/// present. Each miss logs its distinct `ccr_expansion_evaluation` outcome.
/// Extracted from `recommend_ccr_expansions` without behavior change.
pub(super) fn ccr_tracker_and_runtime<'a>(
    state: &'a AppState,
    request_id: &str,
) -> Option<(
    &'a std::sync::Arc<std::sync::Mutex<headroom_core::ccr::context_tracker::ContextTracker>>,
    &'a CtxOffloadRuntime,
)> {
    let Some(tracker) = state.ccr_context_tracker.as_ref() else {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "no_tracker",
            "ccr: expansion has no context tracker to consult"
        );
        return None;
    };
    let Some(runtime) = state.ctx_offload.as_ref() else {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "no_runtime",
            "ccr: expansion has no offload runtime to fetch from"
        );
        return None;
    };
    Some((tracker, runtime))
}

/// Lock the tracker and analyze the query. A poisoned lock or an empty
/// recommendation set each log and decline.
/// Extracted from `recommend_ccr_expansions` without behavior change.
pub(super) fn query_ccr_tracker(
    tracker: &std::sync::Arc<std::sync::Mutex<headroom_core::ccr::context_tracker::ContextTracker>>,
    user_query: &str,
    turn_number: u32,
    workspace_key: &str,
    request_id: &str,
) -> Option<Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>> {
    let mut guard = match tracker.lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!(
                event = "ccr_tracker_poisoned_proactive",
                request_id = %request_id,
                "CCR Phase 4: tracker mutex poisoned; skipping proactive expansion"
            );
            return None;
        }
    };
    let recommendations = guard.analyze_query(user_query, Some(turn_number), workspace_key);
    if recommendations.is_empty() {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "no_match",
            "ccr: tracker consulted, nothing relevant to expand"
        );
        return None;
    }
    Some(recommendations)
}

/// Consult the CCR tracker: both the tracker and the offload runtime must
/// be present, the lock must succeed, and the query must match something.
/// Each miss logs its distinct `ccr_expansion_evaluation` outcome.
/// Extracted from `check_ccr_expansion_prereqs` without behavior change.
pub(super) fn recommend_ccr_expansions<'a>(
    state: &'a AppState,
    user_query: &str,
    turn_number: u32,
    workspace_key: &str,
    request_id: &str,
) -> Option<(
    &'a CtxOffloadRuntime,
    Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>,
)> {
    let (tracker, runtime) = ccr_tracker_and_runtime(state, request_id)?;
    let recommendations =
        query_ccr_tracker(tracker, user_query, turn_number, workspace_key, request_id)?;
    Some((runtime, recommendations))
}

/// CCR expansion gates: query/flag/cache-mode, compact-summary skip, and
/// the tracker + offload runtime both present and lockable. Each gate logs
/// its distinct `ccr_expansion_evaluation` outcome. Returns the runtime plus
/// the tracker's recommendations on success.
/// Extracted from `maybe_append_ccr_proactive_expansion` without behavior
/// change.
pub(super) fn check_ccr_expansion_prereqs<'a>(
    state: &'a AppState,
    user_query: &str,
    turn_number: u32,
    workspace_key: &str,
    request_id: &str,
) -> Option<(
    &'a CtxOffloadRuntime,
    Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>,
)> {
    if !expansion_gate_passes(state, user_query, request_id) {
        return None;
    }
    recommend_ccr_expansions(state, user_query, turn_number, workspace_key, request_id)
}

/// Fetch the tracker's recommended contents from the CCR store, skipping
/// hashes the store no longer has.
/// Extracted from `maybe_append_ccr_proactive_expansion` without behavior
/// change.
pub(super) fn fetch_expansion_contents(
    ccr: &std::sync::Arc<dyn headroom_core::ccr::CcrStore>,
    recommendations: Vec<headroom_core::ccr::context_tracker::ExpansionRecommendation>,
) -> Vec<headroom_core::ccr::context_tracker::ExpansionContent> {
    let mut expansions = Vec::new();
    for rec in recommendations {
        if let Some(content) = ccr.get(&rec.hash_key) {
            let item_count = content.lines().count().max(1);
            expansions.push(headroom_core::ccr::context_tracker::ExpansionContent {
                hash_key: rec.hash_key,
                content,
                reason: rec.reason,
                item_count,
            });
        }
    }
    expansions
}

// Eight parameters, one over the lint's threshold. Grouping them into a struct
// would mean a type used at exactly two call sites, both of which pass every
// field, so the indirection would cost more reading than it saves.
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_append_ccr_proactive_expansion(
    state: &AppState,
    body: &mut serde_json::Value,
    user_query: &str,
    workspace_key: &str,
    workspace_label: Option<&str>,
    turn_number: u32,
    request_id: &str,
    budget: &crate::injection_budget::InjectionBudget,
) -> bool {
    let Some((runtime, recommendations)) =
        check_ccr_expansion_prereqs(state, user_query, turn_number, workspace_key, request_id)
    else {
        return false;
    };

    let ccr = runtime.store.ccr();
    let rec_count = recommendations.len();
    let expansions = fetch_expansion_contents(&ccr, recommendations);
    if expansions.is_empty() {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "store_miss",
            recs = rec_count,
            "ccr: tracker recommended content the store no longer has"
        );
        return false;
    }

    let expansion_text =
        headroom_core::ccr::context_tracker::ContextTracker::format_expansions_for_context(
            &expansions,
            workspace_label,
        );
    // Charge the shared budget. Expansion appends to the live tail, which is
    // re-sent every turn, so clipping it here is cache-safe.
    let expansion_bytes_uncapped = expansion_text.len() as u64;
    let Some(expansion_text) = budget.take(
        crate::injection_budget::InjectionStage::ProactiveExpansion,
        expansion_text,
    ) else {
        tracing::info!(
            request_id = %request_id,
            event = "ccr_expansion_evaluation",
            outcome = "over_budget",
            bytes = expansion_bytes_uncapped,
            "ccr: expansion did not fit the injection budget"
        );
        return false;
    };
    // Measure before the move: this is what the request grows by, and it is
    // the only number that says whether expansion is worth what offload saved.
    let expansion_bytes = expansion_text.len() as u64;
    let changed = append_context_to_latest_user_turn(body, expansion_text);
    if changed {
        crate::observability::ctx_metrics::observe_proactive_expansion(expansion_bytes);
        tracing::info!(
            request_id = %request_id,
            expansions = expansions.len(),
            expansion_bytes = expansion_bytes,
            "CCR Phase 4: proactively expanded relevant offloaded context"
        );
    }
    changed
}

pub(crate) fn track_ccr_context_records(
    state: &AppState,
    records: &[crate::compression::ctx_offload::OffloadRecord],
    workspace_key: &str,
    user_query: &str,
    turn_number: u32,
    request_id: &str,
) {
    if records.is_empty() || workspace_key.is_empty() {
        return;
    }
    let Some(tracker) = state.ccr_context_tracker.as_ref() else {
        return;
    };
    let mut guard = match tracker.lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!(
                event = "ccr_tracker_poisoned_tracking",
                request_id = %request_id,
                "CCR Phase 4: tracker mutex poisoned; skipping compression tracking"
            );
            return;
        }
    };
    for record in records {
        let sample = record.original.chars().take(500).collect::<String>();
        let item_count = record.original.lines().count().max(1);
        guard.track_compression(
            &record.hash,
            turn_number,
            (!record.title.is_empty()).then_some(record.title.as_str()),
            item_count,
            1,
            workspace_key,
            user_query,
            &sample,
        );
    }
}
