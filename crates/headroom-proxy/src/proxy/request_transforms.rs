//! Request-side rewrites run by `forward_http` before the send: context
//! management, images, tool pruning and search deferral, history repair,
//! tool order and roster pins, cache TTL and breakpoints, system holds, and
//! the OpenAI prompt cache key.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Parse an Anthropic `/v1/messages` body, inject `context_management`
/// directives, and re-serialize. Forwards the body unchanged on any
/// parse/serialize failure or when nothing new was injected.
pub(super) fn maybe_inject_context_management(
    body: bytes::Bytes,
    config: &crate::config::Config,
    request_id: &str,
) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let changed = crate::compression::context_editing::inject_context_management(
        &mut value,
        Some(config.context_edit_keep_tool_uses),
        config.context_edit_min_messages,
        config.context_edit_trigger_tokens,
        config.context_edit_clear_at_least,
        config.context_edit_keep_thinking,
    );
    if !changed {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                request_id = %request_id,
                keep_tool_uses = config.context_edit_keep_tool_uses,
                min_messages = config.context_edit_min_messages,
                trigger_tokens = config.context_edit_trigger_tokens,
                clear_at_least = ?config.context_edit_clear_at_least,
                keep_thinking = ?config.context_edit_keep_thinking,
                "injected context_management directives"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Prune the `tools[]` array per the operator-configured policy (A4).
///
/// Deterministic + cache-safe: the same policy always removes the same tools,
/// so the emitted tools prefix stays byte-stable across turns. Reduces
/// cache_creation — the only bucket that counts toward subscription usage
/// (cache reads are free per Anthropic's rate-limit docs). No-op (returns the
/// original bytes untouched) when the body has no tools array or nothing is
/// removed, so a cache-stable request is never perturbed.
/// Resize oversized images down to Anthropic's own limits before forwarding.
///
/// Anthropic bills images by **dimensions, not bytes** — `(w * h) / 750`, capped
/// at 1568px on the long edge and 1.15MP — so re-encoding alone saves nothing
/// and only a resize moves the number. Measured over 800 live bodies: images are
/// 9.2% of the prompt at 16,228 tok/body, and 13 of 13 distinct images exceeded
/// 1.15MP at a mean 2,877 tokens each. They sit just under the 1568px edge cap,
/// so the provider does not shrink them for us.
///
/// The transform is a pure function of the source bytes and memoised on their
/// hash, so a given image forwards identically on every turn and the cached
/// prefix holds. Enabling it re-keys live conversations once, like any change to
/// content already inside a cached prefix.
pub(crate) fn maybe_optimize_images(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(messages) = value.get("messages").and_then(|m| m.as_array()) else {
        return body;
    };
    let (optimized, results) =
        crate::tile_optimizer::optimize_images_in_messages_cached(messages, "anthropic");
    if results.is_empty() {
        return body;
    }
    let saved: u32 = results
        .iter()
        .map(crate::tile_optimizer::TileOptResult::tokens_saved)
        .sum();
    if saved == 0 {
        return body;
    }
    value["messages"] = serde_json::Value::Array(optimized);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "image_optimize",
                request_id = %request_id,
                images = results.len(),
                tokens_before = results.iter().map(|r| r.tokens_before).sum::<u32>(),
                tokens_saved = saved,
                "resized oversized images"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

pub(crate) fn maybe_prune_tools(
    body: bytes::Bytes,
    policy: &crate::cache_stabilization::tool_prune::PrunePolicy,
    request_id: &str,
) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(tools) = value.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return body;
    };
    let before = tools.len();
    let removed = crate::cache_stabilization::tool_prune::prune_tools(tools, policy);
    if removed == 0 {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                request_id = %request_id,
                tools_before = before,
                tools_removed = removed,
                "pruned tools[] per policy"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Attribution recorded when the tool-search stage ran on `tools[]`.
///
/// `mode` names who deferred: `client` when the client already sent the
/// server-side shape (stand-down — the deferral is real but not ours to
/// book, so a stand-down must not read as the feature being off),
/// `headroom` only when we actually deferred something, `none` otherwise.
pub(crate) struct ToolSearchAttribution {
    pub deferred_tools: usize,
    pub deferred_tokens: i64,
    /// Tokens of deferred tools that are core under the default set
    /// (disjoint slice of `deferred_tokens`, not additive to it).
    pub core_deferred_tokens: i64,
    pub stripped_third_party: usize,
    pub mode: &'static str,
}

/// Server-side tool-search deferral (+ third-party search-tool strip) for
/// Anthropic `/v1/messages`.
///
/// Port of the tools stages in upstream `handlers/anthropic.py`: on
/// third-party Anthropic-compatible upstreams, strip client-originated
/// first-party search tools (they reject that shape); on first-party
/// Anthropic with `HEADROOM_TOOL_SEARCH` on (default), defer non-core
/// schemas behind an injected search tool so they stop billing context.
///
/// Runs after pruning (which settles the tool set); compaction and the
/// cache-control stages below then see the final array, breakpoint move
/// included. Forwards the original bytes untouched on any parse/serialize
/// failure or when nothing changed, so no-op turns stay byte-identical.
pub(crate) fn maybe_inject_tool_search(
    body: bytes::Bytes,
    upstream_base_url: &str,
    model: &str,
    request_id: &str,
    enabled: bool,
) -> (bytes::Bytes, Option<ToolSearchAttribution>) {
    use crate::tool_search_deferral as tsd;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, None),
    };
    let Some(tools) = value.get("tools").and_then(|t| t.as_array()).cloned() else {
        return (body, None);
    };
    let custom = tsd::is_custom_anthropic_base_url(Some(upstream_base_url));

    // Third-party routes reject the first-party search shape: strip
    // client-originated search tools. Not env-gated — a poisoned transcript
    // must recover even with injection off.
    let client_defers = tsd::client_uses_tool_search(&tools);
    let mut tools_vec = tools;
    let mut stripped_third_party = 0usize;
    if custom {
        let strip = tsd::strip_for_third_party_upstream(tools_vec);
        stripped_third_party = strip.removed;
        tools_vec = strip.tools;
    }

    // Deferral injection: first-party only, env-gated. The input array is
    // handed back unchanged when injection doesn't apply.
    let mut deferred_tools = 0usize;
    let mut deferred_tokens = 0i64;
    let mut core_deferred_tokens = 0i64;
    if !custom && enabled {
        let inject = tsd::inject_deferral(tools_vec);
        if inject.changed {
            deferred_tools = inject.deferred.len();
            let tokenizer = headroom_core::tokenizer::get_tokenizer(model);
            let deferred_json = serde_json::to_string(&inject.deferred).unwrap_or_default();
            deferred_tokens = tokenizer.count_text(&deferred_json) as i64;
            core_deferred_tokens = if inject.core_deferred.is_empty() {
                // An empty slice still serializes to `"[]"`, which every
                // backend prices at >= 1 token — no core savings, not one
                // phantom token (which also mistags the request downstream).
                0
            } else {
                let core_json = serde_json::to_string(&inject.core_deferred).unwrap_or_default();
                tokenizer.count_text(&core_json) as i64
            };
            tracing::info!(
                event = "tool_search_deferral",
                request_id = %request_id,
                deferred_tools = deferred_tools,
                deferred_tokens = deferred_tokens,
                core_deferred_tokens = core_deferred_tokens,
                "deferred non-core tool schemas behind the search tool"
            );
        }
        tools_vec = inject.tools;
    }

    // "headroom" only when we actually deferred something: injection also
    // declines on a small tool surface or when nothing is deferrable, and
    // calling that "headroom" would overstate our role exactly where we did
    // nothing.
    let mode = if client_defers {
        "client"
    } else if deferred_tools > 0 {
        "headroom"
    } else {
        "none"
    };
    if stripped_third_party == 0 && deferred_tools == 0 && !client_defers {
        return (body, None);
    }
    value["tools"] = serde_json::Value::Array(tools_vec);
    match serde_json::to_vec(&value) {
        Ok(bytes) => (
            bytes::Bytes::from(bytes),
            Some(ToolSearchAttribution {
                deferred_tools,
                deferred_tokens,
                core_deferred_tokens,
                stripped_third_party,
                mode,
            }),
        ),
        Err(_) => (body, None),
    }
}

/// Tool-search history repair (upstream #2805). Drops `server_tool_use` /
/// `tool_search_tool_result` blocks the final tools array cannot support:
/// side-requests replaying a transcript against a smaller tools array, or
/// transcripts poisoned while deferral was on.
///
/// Unconditional and last: runs after turn hooks (which may rewrite tools)
/// and after every other tools/messages mutator, validating against the
/// final outbound array. Returns the neutralized-block count; zero means the
/// original bytes are forwarded untouched.
pub(crate) fn maybe_repair_tool_search_history(
    body: bytes::Bytes,
    request_id: &str,
) -> (bytes::Bytes, usize) {
    use crate::tool_search_deferral as tsd;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
    };
    let tools: Vec<serde_json::Value> = value
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let messages: Vec<serde_json::Value> = match value.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return (body, 0),
    };
    let repair = tsd::strip_unsupported_blocks(messages, &tools);
    if repair.neutralized == 0 {
        return (body, 0);
    }
    value["messages"] = serde_json::Value::Array(repair.messages);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "tool_search_history_repair",
                request_id = %request_id,
                neutralized_blocks = repair.neutralized,
                "repaired tool-search history blocks the tools array cannot support (replaced with text in place)"
            );
            (bytes::Bytes::from(bytes), repair.neutralized)
        }
        Err(_) => (body, 0),
    }
}

/// CCR retrieve history repair (upstream #2814). Neutralizes
/// `headroom_retrieve` history references the outbound tools array cannot
/// support: a side-request forwarded without declaring the tool would
/// otherwise 400 on the historical `tool_use`.
///
/// Runs beside the tool-search repair, after both normal CCR injection and
/// turn hooks, validating against the final outbound tools array.
/// Neutralize-in-place (never drops messages) so user/assistant alternation
/// survives. Returns the neutralized-block count; zero means the original
/// bytes are forwarded untouched.
pub(crate) fn maybe_repair_ccr_retrieve_history(
    body: bytes::Bytes,
    request_id: &str,
) -> (bytes::Bytes, usize) {
    use crate::ccr_retrieve_repair as ccr;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
    };
    let tools: Vec<serde_json::Value> = value
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let messages: Vec<serde_json::Value> = match value.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return (body, 0),
    };
    let repair = ccr::strip_unsupported_ccr_blocks(messages, &tools);
    if repair.neutralized == 0 {
        return (body, 0);
    }
    value["messages"] = serde_json::Value::Array(repair.messages);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "ccr_retrieve_history_repair",
                request_id = %request_id,
                neutralized_blocks = repair.neutralized,
                "neutralized headroom_retrieve history blocks the tools array does not declare"
            );
            (bytes::Bytes::from(bytes), repair.neutralized)
        }
        Err(_) => (body, 0),
    }
}

/// Orphan client `tool_result` repair. Neutralizes history results with no
/// preceding matching `tool_use`, then history calls with no matching
/// result in the next message — Anthropic rejects both directions
/// independently of the tools array (so neither sibling repair covers the
/// declared-tool case). Results run first: the siblings only ever remove
/// calls, which can only strand more results, and a result neutralized
/// here strands its call for the second pass. Returns the
/// neutralized-block count; zero means the original bytes are forwarded
/// untouched.
pub(crate) fn maybe_repair_orphan_tool_results(
    body: bytes::Bytes,
    request_id: &str,
) -> (bytes::Bytes, usize) {
    use crate::orphan_tool_result as otr;

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0),
    };
    let messages: Vec<serde_json::Value> = match value.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m.clone(),
        None => return (body, 0),
    };
    let first = otr::strip_orphan_tool_results(messages);
    let second = otr::strip_dangling_tool_calls(first.messages);
    let neutralized = first.neutralized + second.neutralized;
    if neutralized == 0 {
        return (body, 0);
    }
    value["messages"] = serde_json::Value::Array(second.messages);
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::info!(
                event = "orphan_tool_repair",
                request_id = %request_id,
                neutralized_blocks = neutralized,
                "neutralized history tool blocks with no matching pair"
            );
            (bytes::Bytes::from(bytes), neutralized)
        }
        Err(_) => (body, 0),
    }
}

/// Strip annotation keys (`$schema`, `title`, `examples`, …) from `tools[]`
/// and normalise description whitespace, then re-serialize.
///
/// Mirrors the pass both Python handlers apply after tools are finalised
/// (`headroom/proxy/handlers/anthropic.py`, `.../openai.py`). Shape-agnostic:
/// it walks the whole `tools` array, so Anthropic's `input_schema` and
/// OpenAI's `function.parameters` are both covered.
///
/// Forwards the original bytes untouched when there is no `tools` array, when
/// compaction saves nothing, or on any parse/serialize failure — a
/// cache-stable request is never perturbed for zero gain.
pub(super) fn maybe_compact_tool_schemas(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let (compacted, modified, before_bytes, after_bytes) =
        crate::tool_schema_compaction::compact_tools(value);
    if !modified {
        return body;
    }
    match serde_json::to_vec(&compacted) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                tools_before_bytes = before_bytes,
                tools_after_bytes = after_bytes,
                "tool schema compaction"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// B3: put back tools the client dropped from this session's roster, so the
/// `tools` prefix stays byte-stable through a one-tool flap. Runs before B2
/// so the order replay sees a complete roster. Same passthrough rules as
/// [`maybe_stabilize_tool_order`]: no `tools`, empty `session_key`, or any
/// parse/serialize failure forwards the original bytes.
pub(crate) fn maybe_pin_tool_roster(
    body: bytes::Bytes,
    store: &cache_stabilization::tool_roster_pin::RosterPinStore,
    session_key: &str,
    request_id: &str,
) -> bytes::Bytes {
    if session_key.is_empty() {
        return body;
    }
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let model = value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(tools) = value
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return body;
    };
    let outcome = store.pin(session_key, &model, tools);
    if !outcome.changed() {
        return body;
    }
    tracing::info!(
        event = "tool_roster_pinned",
        request_id = %request_id,
        model = %model,
        reinserted = %outcome.reinserted.join(","),
        appended = %outcome.appended.join(","),
        "tool roster pinned to the session's remembered set"
    );
    match serde_json::to_vec(&value) {
        Ok(bytes) => bytes::Bytes::from(bytes),
        Err(_) => body,
    }
}

/// B2: reorder `tools[]` to lead with the order forwarded on this session's
/// previous turn, appending genuinely-new tools at the end.
///
/// Runs last, once tools are final — after routing, memory/CCR injection,
/// pruning and schema compaction — so the recorded order is the order the
/// provider actually caches. See
/// [`cache_stabilization::tool_order`] for the guards and the measured effect.
///
/// Forwards the original bytes untouched when there is no `tools` array, when
/// the stabilizer declines, or on any parse/serialize failure.
///
/// An empty `session_key` is also a passthrough. It should not happen on this
/// branch (the drift detector populates it for every buffered Anthropic
/// request), but the failure mode if it ever did is every conversation on the
/// box collapsing into one store slot and replaying each other's tool order.
pub(crate) fn maybe_stabilize_tool_order(
    body: bytes::Bytes,
    store: &cache_stabilization::tool_order::ToolOrderStore,
    session_key: &str,
    request_id: &str,
) -> bytes::Bytes {
    if session_key.is_empty() {
        return body;
    }
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let model = value
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let reordered = match value
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        Some(tools) => store.stabilize(session_key, &model, tools),
        None => return body,
    };
    if !reordered {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                model = %model,
                event = "cache_stable_tool_order",
                "replayed previous tool order; new tools appended at the end"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// B1: pin every `cache_control.ttl` in the body to `1h`.
///
/// Runs last, after every mutation that could add or move a marker, so what we
/// pin is what goes on the wire. See [`cache_stabilization::cache_ttl`] for the
/// economics and why this is skipped on PAYG.
///
/// Forwards the original bytes untouched when there is no marker to change or
/// on any parse/serialize failure.
/// Put the message breakpoint on the last content block. See
/// [`cache_stabilization::message_breakpoints`] for the measurement.
pub(super) fn maybe_push_tail_breakpoint(body: bytes::Bytes, request_id: &str) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    if !cache_stabilization::message_breakpoints::push_marker_to_tail(&mut value) {
        crate::observability::tail_breakpoint::observe(false);
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            crate::observability::tail_breakpoint::observe(true);
            tracing::debug!(
                request_id = %request_id,
                event = "cache_tail_breakpoint",
                "moved the message breakpoint to the tail"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

pub(super) fn maybe_pin_cache_ttl(
    body: bytes::Bytes,
    request_id: &str,
    split: bool,
) -> bytes::Bytes {
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let changed = if split {
        cache_stabilization::cache_ttl::tail_5m_prefix_1h(&mut value)
    } else {
        cache_stabilization::cache_ttl::force_1h_ttl(&mut value)
    };
    if !changed {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => {
            tracing::debug!(
                request_id = %request_id,
                event = if split { "split_cache_ttl" } else { "force_1h_cache_ttl" },
                "pinned cache_control ttl"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body,
    }
}

/// Count the image blocks in the client's own messages, and the placeholders
/// left where it has already dropped one.
///
/// Claude Code sheds an aged-out image by rewriting its `tool_result` to the
/// literal `[image]`. That edits a message deep in the prefix, so everything
/// after it re-caches: 107,003 tokens on 2026-08-24, logged as
/// `early_messages` drift. Two ways out, and the cheaper one depends on
/// numbers nobody has: holding the image costs its tokens re-read every
/// remaining turn, dropping it early costs one rebuild in sessions that might
/// never have collapsed at all.
///
/// So measure before choosing. `image_blocks` is the tax holding would carry,
/// `collapsed_blocks` marks the turn the client let go, and the gap to the
/// next rebuild boundary — already in the log as
/// `prefix_replay_invalidated_on_rebuild` and `no_previous_turn` — is how long
/// that tax would run. Counting only; nothing here changes what is forwarded.
pub(super) fn image_census(messages: &[serde_json::Value]) -> (usize, usize, usize) {
    let mut images = 0usize;
    let mut collapsed = 0usize;
    let mut b64_bytes = 0usize;
    for message in messages {
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            // An image arrives inside a `tool_result`'s own content array, or
            // on its own as a top-level block.
            let inner = block.get("content").and_then(|c| c.as_array());
            let candidates = inner
                .map(|v| v.as_slice())
                .unwrap_or(std::slice::from_ref(block));
            for candidate in candidates {
                match candidate.get("type").and_then(|t| t.as_str()) {
                    Some("image") => {
                        images += 1;
                        b64_bytes += candidate
                            .get("source")
                            .and_then(|s| s.get("data"))
                            .and_then(|d| d.as_str())
                            .map_or(0, str::len);
                    }
                    Some("text")
                        if candidate.get("text").and_then(|t| t.as_str()) == Some("[image]") =>
                    {
                        collapsed += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    (images, collapsed, b64_bytes)
}

/// Hold this conversation's working-directory line still, restating the live
/// directory at the message tail.
///
/// Byte-equal passthrough — the same cache-safety invariant the other body
/// rewrites keep — when the body is not JSON, `system` names no working
/// directory, this is the conversation's first sight, or the live directory
/// already matches the pin. See [`cache_stabilization::working_dir`].
/// Run every `system` hold this config enables, in place.
///
/// One entry point on purpose. The holds used to be applied inline in
/// `forward_http` and nowhere else, so a routed turn — anything reaching
/// an upstream through `handlers::local_model` rather than through
/// `forward_http` — was forwarded unheld, and the gate conditions had no
/// single place to be read off. Callers that hold an Anthropic body as a
/// `Value` should call this; the byte-level wrappers below exist for the
/// one caller that has bytes.
///
/// `AnthropicMessages`-shaped bodies only: the pins read `system`, which
/// is where Claude Code puts the volatile lines and is not a field the
/// other endpoints carry in that shape.
pub(crate) fn apply_system_holds(
    state: &AppState,
    value: &mut serde_json::Value,
    session_key: &str,
    request_id: &str,
) {
    // Both holds depend on `--prefix-replay`: `working_dir` restates the
    // live directory at the tail and needs replay to carry that note into
    // later turns, and without it the note would break the prefix every
    // turn — causing the churn the hold exists to stop.
    if !state.config.prefix_replay || session_key.is_empty() {
        return;
    }
    if state.config.hold_working_directory {
        hold_working_directory_value(value, &state.working_dir_pins, session_key, request_id);
    }
    if state.config.hold_role_sentence {
        hold_role_sentence_value(value, &state.role_sentence_pins, session_key, request_id);
    }
}

/// Inherit hold pins along message lineage onto a fresh lane, before the
/// preview/hold stages read them.
///
/// A lane switch (`cd`, preamble edit) mints a lane with no pins, so the
/// preview misses and the hold latches the live form — even when the new
/// lane continues another lane's history, in which case the lineage's pin is
/// still the right one and the turn replays at zero cost instead of
/// re-caching. The donor bar is the adoption bar (same floor, same head
/// match), so an unrelated stream can never donate; a lane that already
/// latched its own pin keeps it.
///
/// The probe reads the same snapshot replay does, on both paths, so there
/// is no injection skew to be best-effort about: the Claude hook runs on
/// `parsed` (client bytes — injection, CCR expansion, offload and the
/// thinking strip all mutate downstream copies), and the replay snapshot,
/// the stored histories and the adoption search all derive from those same
/// client bytes. The routed hook runs post-CTX-transforms for the same
/// reason — that path's replay snapshot is taken there too. When no donor
/// is found this is a silent no-op and the adoption gate still keeps the
/// turn honest.
pub(crate) fn inherit_lane_pins(
    state: &AppState,
    lane_key: &str,
    messages: &[serde_json::Value],
    request_id: &str,
) -> Option<String> {
    if lane_key.is_empty() {
        return None;
    }
    let donor = state.replay_store.lineage_donor_lane(lane_key, messages)?;
    let dir = state.working_dir_pins.inherit_pin(&donor, lane_key);
    let sentence = state.role_sentence_pins.inherit_pin(&donor, lane_key);
    if dir || sentence {
        tracing::info!(
            event = "lane_pins_inherited",
            request_id = %request_id,
            donor_lane_hash = %cache_stabilization::drift_detector::session_key_log_prefix(&donor),
            lane_hash = %cache_stabilization::drift_detector::session_key_log_prefix(lane_key),
            working_dir = dir,
            role_sentence = sentence,
            "a lane switch continues another lane's history; its hold pins travel with it"
        );
    }
    Some(donor)
}

/// [`apply_system_holds`] for a caller that holds bytes.
///
/// Byte-equal passthrough when nothing was held, so a turn no hold
/// touched is not re-serialized and cannot pick up a formatting
/// difference on its way through.
pub(super) fn apply_system_holds_to_bytes(
    state: &AppState,
    body: bytes::Bytes,
    session_key: &str,
    request_id: &str,
) -> bytes::Bytes {
    if !state.config.prefix_replay
        || session_key.is_empty()
        || !(state.config.hold_working_directory || state.config.hold_role_sentence)
    {
        return body;
    }
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let mut held = false;
    if state.config.hold_working_directory {
        held |= hold_working_directory_value(
            &mut value,
            &state.working_dir_pins,
            session_key,
            request_id,
        );
    }
    if state.config.hold_role_sentence {
        held |= hold_role_sentence_value(
            &mut value,
            &state.role_sentence_pins,
            session_key,
            request_id,
        );
    }
    if !held {
        return body;
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => bytes::Bytes::from(bytes),
        Err(_) => body,
    }
}

/// The hold itself. Returns whether `value` was rewritten.
pub(super) fn hold_working_directory_value(
    value: &mut serde_json::Value,
    pins: &cache_stabilization::working_dir::WorkingDirPins,
    session_key: &str,
    request_id: &str,
) -> bool {
    let outcome = pins.hold(value, session_key);
    let Some(live) = outcome.rewrote() else {
        // Every no-op reason gets a line. "Never fired" and "fired and
        // found nothing to do" are the same count of zero otherwise, and
        // only one of them means the hold is working. The proxy runs at
        // `info`, so the outcomes that mean something went wrong are
        // logged there; the healthy steady state stays at `debug` rather
        // than putting a line on every turn.
        let session_key_hash =
            cache_stabilization::drift_detector::session_key_log_prefix(session_key);
        if outcome.is_noteworthy() {
            tracing::info!(
                event = "working_directory_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "working-directory hold changed nothing"
            );
        } else {
            tracing::debug!(
                event = "working_directory_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "working-directory hold changed nothing"
            );
        }
        return false;
    };
    // The path is the operator's own filesystem, and the session key is
    // hashed for the same reason every other event here hashes it.
    tracing::info!(
        event = "working_directory_held",
        request_id = %request_id,
        session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
        live_directory = %live,
        "held the system preamble's working directory and restated the live one at the tail"
    );
    true
}

/// Hold this conversation's opening role sentence still.
///
/// Byte-equal passthrough on the same terms as [`hold_working_directory`]:
/// not JSON, no such sentence, first sight, or already matching the pin. See
/// [`cache_stabilization::role_sentence`].
/// The hold itself. Returns whether `value` was rewritten.
pub(super) fn hold_role_sentence_value(
    value: &mut serde_json::Value,
    pins: &cache_stabilization::role_sentence::RoleSentencePins,
    session_key: &str,
    request_id: &str,
) -> bool {
    let outcome = pins.hold(value, session_key);
    let Some(live) = outcome.rewrote() else {
        // Same split as the working-directory hold: an outcome that means
        // the hold wanted to act and could not is worth an `info` line at
        // the proxy's default level, the steady state is not.
        let session_key_hash =
            cache_stabilization::drift_detector::session_key_log_prefix(session_key);
        if outcome.is_noteworthy() {
            tracing::info!(
                event = "role_sentence_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "role-sentence hold changed nothing"
            );
        } else {
            tracing::debug!(
                event = "role_sentence_hold_skipped",
                request_id = %request_id,
                session_key_hash = %session_key_hash,
                outcome = outcome.label(),
                "role-sentence hold changed nothing"
            );
        }
        return false;
    };
    tracing::info!(
        event = "role_sentence_held",
        request_id = %request_id,
        live_sentence_len = live.len(),
        "held the opening role sentence to the conversation's opening form"
    );
    true
}

/// Make the outbound body satisfy Anthropic's `cache_control` TTL ordering.
///
/// A `ttl: "1h"` marker behind a 5-minute one kills the whole turn with a 400,
/// and the sections are read as one sequence — `tools`, `system`, `messages` —
/// so a violation can straddle two of them and be invisible to any stage that
/// looks at one list. See [`cache_stabilization::ttl_order`] for the two
/// repairs and which one applies when.
pub(super) fn enforce_cache_control_ttl_order(
    body_to_send: bytes::Bytes,
    original: &bytes::Bytes,
    forced_1h: bool,
    request_id: &str,
) -> bytes::Bytes {
    // Cheap gate: only a 1h marker can break the rule, and only a 1h marker
    // can have leaked in.
    const LONG_TTL: &[u8] = b"\"1h\"";
    if !body_to_send.windows(LONG_TTL.len()).any(|w| w == LONG_TTL) {
        return body_to_send;
    }
    // This repairs violations the proxy introduced. On a body no stage rewrote
    // there is nothing of ours to repair, and editing it would break the
    // passthrough guarantee, change the client's cache key, and hide a bug in
    // their request — Anthropic's own 400 is the honest answer. Ordered after
    // the marker scan so the comparison only runs on bodies that could break
    // the rule.
    if body_to_send == original {
        return body_to_send;
    }
    let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(&body_to_send) else {
        return body_to_send;
    };

    // Which lane the turn belongs to is the client's call — except when B1 is
    // pinning every marker to 1h, which is the operator asking for that lane
    // on their behalf. Reading an unparseable client body as "asked for 1h"
    // keeps a marker of theirs from being stripped on a guess.
    let client_asked_for_1h = forced_1h
        || serde_json::from_slice::<serde_json::Value>(original)
            .map(|client| cache_stabilization::ttl_order::asks_for_1h(&client))
            .unwrap_or(true);

    let repair =
        cache_stabilization::ttl_order::enforce_ttl_order(&mut parsed, client_asked_for_1h);
    if repair.is_noop() {
        return body_to_send;
    }
    match serde_json::to_vec(&parsed) {
        Ok(bytes) => {
            tracing::warn!(
                target: "headroom.proxy",
                event = "cache_control_ttl_order",
                request_id = %request_id,
                demoted = repair.demoted,
                promoted = repair.promoted,
                client_asked_for_1h,
                "repaired cache_control TTL ordering before forwarding"
            );
            bytes::Bytes::from(bytes)
        }
        Err(_) => body_to_send,
    }
}

/// Render indices for a log field, capped so one pathological turn cannot
/// write a thousand-entry line.
pub(super) fn join_indices(indices: &[usize]) -> String {
    const MAX: usize = 20;
    let head = indices
        .iter()
        .take(MAX)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if indices.len() > MAX {
        format!("{head},…+{}", indices.len() - MAX)
    } else {
        head
    }
}

/// PR-E4: OpenAI `prompt_cache_key` auto-injection helper.
///
/// Re-serialise after a key injection. If serialization fails (would be
/// very unusual — the body just parsed successfully), fall back to the
/// original bytes. No-silent-fallback rule: log it loudly so a regression
/// can't hide.
/// Extracted from `finish_key_injection` without behavior change.
pub(super) fn serialize_injected_key(
    parsed: &serde_json::Value,
    body: bytes::Bytes,
    key_prefix: &str,
    request_id: &str,
    path: &str,
) -> bytes::Bytes {
    match serde_json::to_vec(parsed) {
        Ok(buf) => {
            tracing::info!(
                event = "e4_applied",
                request_id = %request_id,
                path = %path,
                key_prefix = %key_prefix,
                body_bytes_in = body.len(),
                body_bytes_out = buf.len(),
                "PR-E4: injected prompt_cache_key"
            );
            bytes::Bytes::from(buf)
        }
        Err(e) => {
            tracing::error!(
                event = "e4_serialize_error",
                request_id = %request_id,
                path = %path,
                error = %e,
                "PR-E4: re-serialize after injection failed; forwarding original bytes"
            );
            body
        }
    }
}

/// Log a key-injection skip: the customer-visible KeyPresent skip at info;
/// the NotAnObject skip (structurally impossible past the dispatcher gate)
/// is surfaced separately for operators chasing pathological inputs.
/// Extracted from `finish_key_injection` without behavior change.
pub(super) fn note_key_injection_skip(
    reason: cache_stabilization::openai_cache_key::SkipReason,
    request_id: &str,
    path: &str,
) {
    use cache_stabilization::openai_cache_key::SkipReason;

    match reason {
        SkipReason::KeyPresent => {
            tracing::info!(
                event = "e4_skipped",
                request_id = %request_id,
                path = %path,
                reason = SkipReason::KeyPresent.as_str(),
                "PR-E4: skipped prompt_cache_key injection (customer-set value preserved)"
            );
        }
        SkipReason::NotAnObject => {
            tracing::warn!(
                event = "e4_skipped",
                request_id = %request_id,
                path = %path,
                reason = SkipReason::NotAnObject.as_str(),
                "PR-E4: body is not a JSON object; passthrough"
            );
        }
    }
}

/// Finish a key injection: re-serialise on `Applied` (loudly falling back
/// to the original bytes on failure), log the skip reason on `Skipped`.
/// Extracted from `maybe_inject_openai_prompt_cache_key` without behavior
/// change.
pub(super) fn finish_key_injection(
    outcome: cache_stabilization::openai_cache_key::InjectOutcome,
    parsed: &serde_json::Value,
    body: bytes::Bytes,
    request_id: &str,
    path: &str,
) -> bytes::Bytes {
    use cache_stabilization::openai_cache_key::InjectOutcome;

    match outcome {
        InjectOutcome::Applied { key_prefix } => {
            serialize_injected_key(parsed, body, &key_prefix, request_id, path)
        }
        InjectOutcome::Skipped { reason } => {
            note_key_injection_skip(reason, request_id, path);
            body
        }
    }
}

/// Gates on [`AuthMode::Payg`] and the in-body
/// `prompt_cache_key` skip rule, parses the body once, mutates if
/// appropriate, and re-serialises. Returns the original `body` on
/// any non-applicable path — every error / skip leaves the bytes
/// untouched (Phase A passthrough invariant).
///
/// Logs `e4_skipped` for each skip reason and `e4_applied` with
/// only the first [`KEY_PREFIX_LOG_LEN`] hex chars of the key
/// (never the full key, which is identifying material).
///
/// [`KEY_PREFIX_LOG_LEN`]: cache_stabilization::openai_cache_key::KEY_PREFIX_LOG_LEN
pub(crate) fn maybe_inject_openai_prompt_cache_key(
    body: bytes::Bytes,
    shape: cache_stabilization::openai_cache_key::OpenAiShape,
    auth_mode: AuthMode,
    request_id: &str,
    path: &str,
) -> bytes::Bytes {
    use cache_stabilization::openai_cache_key::inject_prompt_cache_key;

    // Auth-mode gate: only PAYG bodies are eligible. OAuth /
    // Subscription requests pass through byte-equal — synthesised
    // cache keys would look like cache-evasion to the upstream
    // and could void OAuth scopes pinned to `(account, model,
    // session)`.
    if !matches!(auth_mode, AuthMode::Payg) {
        tracing::info!(
            event = "e4_skipped",
            request_id = %request_id,
            path = %path,
            reason = "auth_mode",
            auth_mode = auth_mode.as_str(),
            "PR-E4: skipped prompt_cache_key injection (non-PAYG auth mode)"
        );
        return body;
    }

    // Parse for the inject step. Failure here is silent — the
    // dispatcher above already logged the parse outcome on its
    // own decision path; we don't want to double-log. The body
    // round-trips unchanged.
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return body;
        }
    };

    let outcome = inject_prompt_cache_key(&mut parsed, shape);
    finish_key_injection(outcome, &parsed, body, request_id, path)
}

#[cfg(test)]
mod tool_search_wiring_tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> serde_json::Value {
        json!({"name": name, "input_schema": {"type": "object"}})
    }

    fn fourteen_tools() -> Vec<serde_json::Value> {
        [
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "Task",
            "WebFetch",
            "Slack_post",
            "Linear_get",
            "Sentry_get",
            "Notion_read",
            "Snowflake_q",
            "PagerDuty_get",
        ]
        .iter()
        .map(|n| tool(n))
        .collect()
    }

    fn body_with(tools: Vec<serde_json::Value>, messages: serde_json::Value) -> bytes::Bytes {
        bytes::Bytes::from(
            serde_json::to_vec(&json!({
                "model": "claude-opus-5",
                "max_tokens": 64,
                "messages": messages,
                "tools": tools,
            }))
            .unwrap(),
        )
    }

    #[test]
    fn injection_fires_on_first_party_wire_bytes() {
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body,
            "https://api.anthropic.com",
            "claude-opus-5",
            "req-test",
            true,
        );
        let attr = attr.expect("14 first-party tools must defer");
        assert_eq!(attr.deferred_tools, 6);
        assert!(attr.deferred_tokens > 0);
        assert_eq!(attr.core_deferred_tokens, 0);
        assert_eq!(attr.mode, "headroom");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], json!("tool_search_tool_regex"));
        assert_eq!(tools.len(), 15);
    }

    #[test]
    fn injection_disabled_is_byte_identical() {
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body.clone(),
            "https://api.anthropic.com",
            "claude-opus-5",
            "req-test",
            false,
        );
        assert!(attr.is_none());
        assert_eq!(out, body);
    }

    #[test]
    fn tool_search_mode_names_who_deferred() {
        // headroom: we deferred.
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (_, attr) = maybe_inject_tool_search(
            body,
            "https://api.anthropic.com",
            "claude-opus-5",
            "req",
            true,
        );
        assert_eq!(attr.expect("must defer").mode, "headroom");

        // client: the array already carries the server-side shape —
        // stand down and name it, rather than reading as feature-off.
        let mut tools = fourteen_tools();
        tools.push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let body = body_with(tools, json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body.clone(),
            "https://api.anthropic.com",
            "claude-opus-5",
            "req",
            true,
        );
        let attr = attr.expect("client stand-down must still report");
        assert_eq!(attr.mode, "client");
        assert_eq!(attr.deferred_tools, 0);
        assert_eq!(out, body, "stand-down must not rewrite the array");

        // none: too few tools to defer, client not deferring either.
        let body = body_with(
            vec![tool("read"), tool("write")],
            json!([{"role": "user", "content": "hi"}]),
        );
        let (_, attr) = maybe_inject_tool_search(
            body,
            "https://api.anthropic.com",
            "claude-opus-5",
            "req",
            true,
        );
        assert!(attr.is_none(), "nothing happened, nothing to report");
    }

    #[test]
    fn small_tool_array_is_byte_identical() {
        let body = body_with(
            vec![tool("read"), tool("write")],
            json!([{"role": "user", "content": "hi"}]),
        );
        let (out, attr) = maybe_inject_tool_search(
            body.clone(),
            "https://api.anthropic.com",
            "claude-opus-5",
            "req-test",
            true,
        );
        assert!(attr.is_none());
        assert_eq!(out, body);
    }

    #[test]
    fn third_party_strips_search_tools_and_skips_injection() {
        let mut tools = fourteen_tools();
        tools.push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let body = body_with(tools, json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body,
            "https://gateway.internal/v1",
            "claude-opus-5",
            "req-test",
            true,
        );
        let attr = attr.expect("strip must report");
        assert_eq!(attr.stripped_third_party, 1);
        assert_eq!(attr.deferred_tools, 0);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tools"].as_array().unwrap().len(), 14);
    }

    /// An inference-profile ARN names a Bedrock deployment, and Bedrock
    /// rejects the first-party `tool_search_tool_*` + `defer_loading` shape.
    /// Upstream's Python gate sniffs the model id for `arn:` because its
    /// gateway hook sees nothing else; here the selected upstream decides, so
    /// an ARN rides the same third-party strip as any other gateway model.
    /// Pinned so a later change cannot hand Bedrock a shape it will reject.
    #[test]
    fn an_arn_model_on_a_gateway_never_gets_first_party_search() {
        let mut tools = fourteen_tools();
        tools.push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let body = body_with(tools, json!([{"role": "user", "content": "hi"}]));
        let (out, attr) = maybe_inject_tool_search(
            body,
            "https://bedrock-gateway.internal/v1",
            "arn:aws:bedrock:ap-southeast-1:1:application-inference-profile/x57j1es",
            "req-test",
            true,
        );
        let attr = attr.expect("strip must report");
        assert_eq!(attr.stripped_third_party, 1);
        assert_eq!(
            attr.deferred_tools, 0,
            "a Bedrock ARN must never be handed first-party deferral"
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tools"].as_array().unwrap().len(), 14);
    }

    #[test]
    fn repair_is_byte_identical_without_search_blocks() {
        let body = body_with(fourteen_tools(), json!([{"role": "user", "content": "hi"}]));
        let (out, neutralized) = maybe_repair_tool_search_history(body.clone(), "req-test");
        assert_eq!(neutralized, 0);
        assert_eq!(out, body);
    }

    #[test]
    fn repair_neutralizes_unsupported_pair_in_place() {
        let body = body_with(
            vec![tool("read")],
            json!([{
                "role": "assistant",
                "content": [
                    {"type": "server_tool_use", "id": "srv_1", "name": "tool_search_tool_regex"},
                    {"type": "tool_search_tool_result", "tool_use_id": "srv_1",
                     "content": [{"type": "tool_reference", "tool_name": "Slack_post"}]},
                ],
            }]),
        );
        let (out, neutralized) = maybe_repair_tool_search_history(body, "req-test");
        assert_eq!(neutralized, 2);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // Message slot kept, pair replaced with text — never dropped, so
        // signed thinking coordinates downstream are undisturbed.
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(content.iter().all(|b| b["type"] == json!("text")));
    }

    #[test]
    fn ccr_repair_declared_tool_is_byte_identical() {
        let mut tools = vec![tool("read")];
        tools.push(tool("headroom_retrieve"));
        let body = body_with(
            tools,
            json!([{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "r1", "name": "headroom_retrieve",
                     "input": {"hash": "abc"}},
                ],
            }]),
        );
        let (out, neutralized) = maybe_repair_ccr_retrieve_history(body.clone(), "req-test");
        assert_eq!(neutralized, 0);
        assert_eq!(out, body);
    }

    #[test]
    fn ccr_repair_neutralizes_undeclared_pair_in_place() {
        let body = body_with(
            vec![tool("read")],
            json!([
                {"role": "assistant", "content": [
                    {"type": "text", "text": "looking it up"},
                    {"type": "tool_use", "id": "r1", "name": "headroom_retrieve",
                     "input": {"hash": "abc"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "r1",
                     "content": "the original content"},
                ]},
            ]),
        );
        let (out, neutralized) = maybe_repair_ccr_retrieve_history(body, "req-test");
        assert_eq!(neutralized, 2);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // Alternation safety: same messages, same roles, text in place.
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(messages[1]["role"], json!("user"));
        assert_eq!(
            messages[0]["content"][1]["text"],
            json!("[headroom_retrieve call omitted: tool not available this turn]")
        );
        assert_eq!(
            messages[1]["content"][0]["text"],
            json!("the original content")
        );
    }
}

#[cfg(test)]
mod image_census_tests {
    use super::*;

    /// The two forms of the same screenshot: what the client sends when it
    /// still holds the image, and what it sends once it has let go. Both are
    /// counted, on their own axis, so the turn the client collapses is visible
    /// in the log without guessing from a diff.
    #[test]
    fn counts_live_images_and_the_placeholders_left_behind() {
        let messages = vec![
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_live",
                    "content": [{
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
                    }]
                }]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_collapsed",
                    "content": [{"type": "text", "text": "[image]"}]
                }]
            }),
        ];

        assert_eq!(image_census(&messages), (1, 1, 4));
    }

    /// Ordinary text must not read as either, or every turn logs a census.
    #[test]
    fn ignores_text_that_merely_mentions_an_image() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "the [image] above shows the panel"}]
        })];

        assert_eq!(image_census(&messages), (0, 0, 0));
    }

    /// A top-level image, not wrapped in a tool_result, still counts.
    #[test]
    fn counts_an_image_attached_straight_to_the_message() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image", "source": {"type": "base64", "data": "AAAAAAAA"}}
            ]
        })];

        assert_eq!(image_census(&messages), (1, 0, 8));
    }
}
