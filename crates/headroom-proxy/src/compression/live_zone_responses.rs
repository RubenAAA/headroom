//! OpenAI Responses `/v1/responses` request compression — live-zone
//! dispatcher entry point (Phase C PR-C3).
//!
//! # Provider scope
//!
//! Sibling of [`super::live_zone_openai`] (Chat Completions) and
//! [`super::live_zone_anthropic`] (Messages). Same per-content-type
//! compressor backend, same byte-threshold gate, same
//! tokenizer-validated rejection check, same byte-range surgery.
//!
//! Differences from the Chat Completions dispatcher:
//!
//! - Request shape: items are keyed under `input` (canonical) or
//!   `messages` (legacy alias) and are explicitly typed by the
//!   `type` field, not role-tagged.
//! - Live zone: latest of each compressible kind —
//!   `function_call_output`, `local_shell_call_output`,
//!   `apply_patch_call_output`, plus the latest `message` (user role)
//!   text content. Earlier *_output items are FROZEN.
//! - Output items must clear a 2 KiB minimum BEFORE the
//!   per-content-type byte threshold even runs (per spec PR-C3
//!   §scope, line 167 of the realignment plan).
//! - Cache hot zone: every other item type passes through verbatim.
//!   This includes `reasoning.encrypted_content`, `compaction.*`,
//!   MCP / computer-use / web-search / file-search /
//!   code-interpreter / image-generation / tool-search /
//!   custom-tool calls, and any future-unknown `type` value.
//!
//! Failure-mode contract matches every other live-zone dispatcher:
//! every error path returns the original body unchanged. Per-block
//! compressor errors surface via the manifest at warn-level; only the
//! failing block reverts.

use bytes::Bytes;
use headroom_core::auth_mode::AuthMode as RequestAuthMode;
use headroom_core::transforms::live_zone::DEFAULT_MODEL;
use headroom_core::transforms::{
    LiveZoneError, LiveZoneOutcome, compress_openai_responses_live_zone_with_config,
    summarize_openai_responses_no_change_reason,
};
use serde_json::Value;

use crate::cache_stabilization::tool_def_normalize::{
    any_tool_has_cache_control, sort_schema_keys_recursive, sort_tools_deterministically,
};
use crate::compression::{Outcome, PassthroughReason};
use crate::config::CompressionMode;

/// OpenAI Responses live-zone compression entry point.
///
/// # Behaviour
///
/// - `mode == Off` → [`Outcome::Passthrough { ModeOff }`].
/// - Body parses but neither `input` nor `messages` is an array →
///   `Passthrough { NoMessages }`.
/// - Body doesn't parse → `Passthrough { NotJson }`.
/// - At least one live-zone block compressed → [`Outcome::Compressed`].
/// - Otherwise → [`Outcome::NoCompression`].
pub fn compress_openai_responses_request(
    body: &Bytes,
    mode: CompressionMode,
    auth_mode: RequestAuthMode,
    request_id: &str,
    exclude_tools: &[String],
) -> Outcome {
    if let Some(outcome) = mode_off_passthrough(&mode, body, request_id) {
        return outcome;
    }

    // Lightweight gate before the full dispatcher walk: parse only
    // enough to determine `input` (or `messages`) shape and the
    // model name. The dispatcher does its own parse — keeping this
    // gate light avoids double-walking the tree on the common
    // no-compression path.
    let parsed = match parse_responses_body(body, &mode, request_id) {
        Ok(v) => v,
        Err(outcome) => return outcome,
    };

    let has_array_field = parsed
        .get("input")
        .or_else(|| parsed.get("messages"))
        .and_then(|v| v.as_array())
        .is_some();
    if let Some(outcome) = require_array_field(has_array_field, body, &mode, request_id) {
        return outcome;
    }

    // Walk every item once for telemetry — log unknown item types at
    // warn level (no-silent-fallbacks) and redact image_data fields
    // from the logged shape (no PII / no megabytes of base64). The
    // upstream-bound bytes are NEVER mutated by this loop; the body
    // is forwarded byte-for-byte as the live-zone dispatcher decides.
    log_item_telemetry(&parsed, request_id);

    let model = parsed
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(DEFAULT_MODEL);

    // ── Phase E PR-E1: tool-array deterministic sort ────────────
    let (dispatch_body, normalization_applied) =
        normalize_tool_definitions_responses(body, &parsed, auth_mode, request_id);

    // F2.1 c2/6: forward F1's classified auth_mode into the dispatcher
    // instead of the hard-coded `Payg`. See live_zone_anthropic.rs for
    // the rationale — same wiring on the OpenAI Responses path.
    let dispatch_config = headroom_core::transforms::live_zone::DispatchConfig {
        exclude_tools: exclude_tools.to_vec(),
        ..Default::default()
    };
    match compress_openai_responses_live_zone_with_config(
        &dispatch_body,
        auth_mode.into(),
        model,
        &dispatch_config,
    ) {
        Ok(LiveZoneOutcome::NoChange { manifest }) => report_responses_no_change(
            manifest,
            normalization_applied,
            dispatch_body,
            body.len(),
            &mode,
            model,
            request_id,
        ),
        Ok(LiveZoneOutcome::Modified { new_body, manifest }) => report_responses_modified(
            manifest,
            &new_body,
            body.len(),
            &mode,
            model,
            request_id,
            normalization_applied,
        ),
        Err(err) => report_responses_dispatch_error(err, &mode, body.len(), request_id),
    }
}

/// Mode-off short-circuit: report passthrough without parsing.
/// Extracted from `compress_openai_responses_request` without behavior change.
fn mode_off_passthrough(mode: &CompressionMode, body: &Bytes, request_id: &str) -> Option<Outcome> {
    if !matches!(mode, CompressionMode::Off) {
        return None;
    }
    tracing::info!(
        event = "compression_decision",
        request_id = %request_id,
        path = "/v1/responses",
        method = "POST",
        compression_mode = mode.as_str(),
        decision = "passthrough",
        reason = "mode_off",
        body_bytes = body.len(),
        "openai responses compression decision"
    );
    Some(Outcome::Passthrough {
        reason: PassthroughReason::ModeOff,
    })
}

/// Parse the request body, passing through on non-JSON.
/// Extracted from `compress_openai_responses_request` without behavior change.
fn parse_responses_body(
    body: &Bytes,
    mode: &CompressionMode,
    request_id: &str,
) -> Result<serde_json::Value, Outcome> {
    match serde_json::from_slice(body) {
        Ok(v) => Ok(v),
        Err(_) => {
            tracing::warn!(
                event = "compression_decision",
                request_id = %request_id,
                path = "/v1/responses",
                method = "POST",
                compression_mode = mode.as_str(),
                decision = "passthrough",
                reason = "not_json",
                body_bytes = body.len(),
                "openai responses compression decision"
            );
            Err(Outcome::Passthrough {
                reason: PassthroughReason::NotJson,
            })
        }
    }
}

/// Require an `input`/`messages` array: pass through when neither exists.
/// `None` means the gate cleared — keep dispatching.
/// Extracted from `compress_openai_responses_request` without behavior change.
fn require_array_field(
    has_array_field: bool,
    body: &Bytes,
    mode: &CompressionMode,
    request_id: &str,
) -> Option<Outcome> {
    if has_array_field {
        return None;
    }
    tracing::info!(
        event = "compression_decision",
        request_id = %request_id,
        path = "/v1/responses",
        method = "POST",
        compression_mode = mode.as_str(),
        decision = "passthrough",
        reason = "no_messages",
        body_bytes = body.len(),
        "openai responses compression decision"
    );
    Some(Outcome::Passthrough {
        reason: PassthroughReason::NoMessages,
    })
}
/// Report a rewritten live zone with stitched-in normalization tags.
/// Extracted from `compress_openai_responses_request` without behavior change.
#[allow(clippy::too_many_arguments)]
fn report_responses_modified(
    manifest: headroom_core::transforms::live_zone::CompressionManifest,
    new_body: &serde_json::value::RawValue,
    body_len: usize,
    mode: &CompressionMode,
    model: &str,
    request_id: &str,
    normalization_applied: NormalizationApplied,
) -> Outcome {
    let mut totals =
        crate::compression::manifest_totals::aggregate(&manifest, request_id, "/v1/responses");
    // Stitch in PR-E1 strategy tags so dashboards see the
    // tool-array sort separately from live-zone compressors.
    for strategy in normalization_applied.strategies() {
        totals.push_strategy(strategy);
    }
    let body_bytes_in = body_len;
    let new_body_bytes = Bytes::copy_from_slice(new_body.get().as_bytes());
    let body_bytes_out = new_body_bytes.len();
    tracing::info!(
        event = "compression_decision",
        request_id = %request_id,
        path = "/v1/responses",
        method = "POST",
        compression_mode = mode.as_str(),
        decision = "compressed",
        reason = "live_zone_blocks_rewritten",
        body_bytes_in = body_bytes_in,
        body_bytes_out = body_bytes_out,
        bytes_freed = body_bytes_in.saturating_sub(body_bytes_out),
        items_total = manifest.messages_total,
        latest_user_message_index = ?manifest.latest_user_message_index,
        live_zone_blocks = manifest.block_outcomes.len(),
        live_zone_strategies = ?totals.strategies,
        live_zone_block_original_bytes = totals.original_bytes,
        live_zone_block_compressed_bytes = totals.compressed_bytes,
        live_zone_block_original_tokens = totals.original_tokens,
        live_zone_block_compressed_tokens = totals.compressed_tokens,
        had_compressor_error = totals.had_compressor_error,
        model = model,
        "openai responses live-zone dispatch"
    );
    Outcome::Compressed {
        body: new_body_bytes,
        tokens_before: totals.original_tokens,
        tokens_after: totals.compressed_tokens,
        strategies_applied: totals.strategies,
        markers_inserted: Vec::new(),
        per_strategy_tokens: totals.per_strategy_tokens,
    }
}

/// Report a dispatcher-level failure as passthrough.
/// Extracted from `compress_openai_responses_request` without behavior change.
fn report_responses_dispatch_error(
    err: LiveZoneError,
    mode: &CompressionMode,
    body_len: usize,
    request_id: &str,
) -> Outcome {
    match err {
        LiveZoneError::BodyNotJson(_) => {
            tracing::warn!(
                event = "compression_decision",
                request_id = %request_id,
                path = "/v1/responses",
                "openai responses live-zone dispatcher rejected JSON body; falling back to passthrough"
            );
            Outcome::Passthrough {
                reason: PassthroughReason::NotJson,
            }
        }
        LiveZoneError::NoMessagesArray => {
            tracing::info!(
                event = "compression_decision",
                request_id = %request_id,
                path = "/v1/responses",
                method = "POST",
                compression_mode = mode.as_str(),
                decision = "passthrough",
                reason = "no_messages",
                body_bytes = body_len,
                "openai responses compression decision"
            );
            Outcome::Passthrough {
                reason: PassthroughReason::NoMessages,
            }
        }
    }
}

/// Report a no-change dispatch, recovering Phase E byte rewrites when the
/// normalizer (but not the live zone) mutated the body.
/// Extracted from `compress_openai_responses_request` without behavior change.
#[allow(clippy::too_many_arguments)]
fn report_responses_no_change(
    manifest: headroom_core::transforms::live_zone::CompressionManifest,
    normalization_applied: NormalizationApplied,
    dispatch_body: Bytes,
    body_len: usize,
    mode: &CompressionMode,
    model: &str,
    request_id: &str,
) -> Outcome {
    let reason = summarize_openai_responses_no_change_reason(&manifest);
    tracing::info!(
        event = "compression_decision",
        request_id = %request_id,
        path = "/v1/responses",
        method = "POST",
        compression_mode = mode.as_str(),
        decision = "no_change",
        reason = reason,
        body_bytes = body_len,
        items_total = manifest.messages_total,
        latest_user_message_index = ?manifest.latest_user_message_index,
        live_zone_blocks = manifest.block_outcomes.len(),
        model = model,
        "openai responses live-zone dispatch"
    );
    if normalization_applied.any() {
        return Outcome::Compressed {
            body: dispatch_body,
            tokens_before: 0,
            tokens_after: 0,
            strategies_applied: normalization_applied.strategies(),
            markers_inserted: Vec::new(),
            per_strategy_tokens: Vec::new(),
        };
    }
    Outcome::NoCompression
}

/// Tracks which Phase E normalization steps mutated the dispatch
/// body for the Responses path. Sibling of the same struct in
/// `live_zone_anthropic` and `live_zone_openai`.
#[derive(Debug, Clone, Copy, Default)]
struct NormalizationApplied {
    e1_tool_sort: bool,
    e2_schema_sort: bool,
}

impl NormalizationApplied {
    fn any(self) -> bool {
        self.e1_tool_sort || self.e2_schema_sort
    }

    fn strategies(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.e1_tool_sort {
            out.push("tool_array_sort");
        }
        if self.e2_schema_sort {
            out.push("schema_key_sort");
        }
        out
    }
}

/// Apply PR-E1 (tool-array sort) and PR-E2 (schema-key sort) to a
/// Responses request body when the auth-mode + marker gates clear.
/// Mirrors the same gate logic as the Anthropic / Chat Completions
/// walkers — same skip events, same outcome shape, same byte-equal
/// passthrough on non-PAYG.
///
/// Responses tools nest the schema at `tool.function.parameters`
/// (same as OpenAI Chat Completions, distinct from Anthropic's
/// `tool.input_schema`).
fn normalize_tool_definitions_responses(
    body: &Bytes,
    parsed: &Value,
    auth_mode: RequestAuthMode,
    request_id: &str,
) -> (Bytes, NormalizationApplied) {
    if !is_payg_for_normalization_responses(auth_mode, request_id) {
        return (body.clone(), NormalizationApplied::default());
    }

    let Some(tools_in) = parsed.get("tools").and_then(Value::as_array) else {
        return (body.clone(), NormalizationApplied::default());
    };
    if tools_in.is_empty() {
        return (body.clone(), NormalizationApplied::default());
    }

    let marker_present = any_tool_has_cache_control(tools_in);

    let mut working = parsed.clone();
    let tools = working
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .expect("tools array verified above");

    let applied = NormalizationApplied {
        e1_tool_sort: sort_responses_tools_if_unmarked(tools, marker_present, request_id),
        e2_schema_sort: sort_all_responses_parameters(tools),
    };
    if applied.e2_schema_sort {
        tracing::info!(
            event = "e2_applied",
            request_id = %request_id,
            path = "/v1/responses",
            tool_count = tools.len(),
            "schema-key sort applied: function.parameters keys rewritten in alphabetic order"
        );
    }

    reserialize_responses_body(working, applied, body, request_id)
}

/// Auth-mode gate for Responses normalization: PAYG-only. Returns true when
/// normalization may proceed.
/// Extracted from `normalize_tool_definitions_responses` without behavior change.
fn is_payg_for_normalization_responses(auth_mode: RequestAuthMode, request_id: &str) -> bool {
    if matches!(auth_mode, RequestAuthMode::Payg) {
        return true;
    }
    tracing::info!(
        event = "e1_skipped",
        request_id = %request_id,
        path = "/v1/responses",
        reason = "auth_mode",
        auth_mode = auth_mode.as_str(),
        "tool-array sort skipped: non-PAYG auth mode passes through byte-equal"
    );
    tracing::info!(
        event = "e2_skipped",
        request_id = %request_id,
        path = "/v1/responses",
        reason = "auth_mode",
        auth_mode = auth_mode.as_str(),
        "schema-key sort skipped: non-PAYG auth mode passes through byte-equal"
    );
    false
}

/// PR-E1: sort tools alphabetically unless a `cache_control` marker is
/// present. Returns whether the sort moved anything.
/// Extracted from `normalize_tool_definitions_responses` without behavior change.
fn sort_responses_tools_if_unmarked(
    tools: &mut [Value],
    marker_present: bool,
    request_id: &str,
) -> bool {
    if marker_present {
        tracing::info!(
            event = "e1_skipped",
            request_id = %request_id,
            path = "/v1/responses",
            reason = "marker_present",
            tool_count = tools.len(),
            "tool-array sort skipped: customer cache_control marker present \
             on at least one tool; preserving customer-intentional order"
        );
        return false;
    }
    let sorted = sort_tools_deterministically(tools);
    if sorted {
        tracing::info!(
            event = "e1_applied",
            request_id = %request_id,
            path = "/v1/responses",
            tool_count = tools.len(),
            "tool-array sort applied: tools reordered alphabetically by name"
        );
    }
    sorted
}

/// PR-E2: sort each tool's `function.parameters` keys recursively. Returns
/// whether anything moved.
/// Extracted from `normalize_tool_definitions_responses` without behavior change.
fn sort_all_responses_parameters(tools: &mut [Value]) -> bool {
    // PR-E2: Responses tool schema lives at `tool.function.parameters`.
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Some(parameters) = tool
            .get_mut("function")
            .and_then(|f| f.get_mut("parameters"))
        else {
            continue;
        };
        // `sort_schema_keys_recursive` reports whether it moved anything,
        // so no before/after byte-compare is needed. `|=`, not `||`:
        // every tool must still be visited even after one changed.
        changed |= sort_schema_keys_recursive(parameters);
    }
    changed
}

/// Re-serialize the normalized body, falling back to the original bytes
/// (and a reset flag set) when serialization fails.
/// Extracted from `normalize_tool_definitions_responses` without behavior change.
fn reserialize_responses_body(
    working: Value,
    applied: NormalizationApplied,
    body: &Bytes,
    request_id: &str,
) -> (Bytes, NormalizationApplied) {
    if !applied.any() {
        return (body.clone(), applied);
    }

    match serde_json::to_vec(&working) {
        Ok(bytes) => (Bytes::from(bytes), applied),
        Err(e) => {
            tracing::warn!(
                event = "tool_def_normalize_serialize_failed",
                request_id = %request_id,
                path = "/v1/responses",
                error = %e,
                "tool-def normalization failed at re-serialize; falling back \
                 to original body bytes"
            );
            (body.clone(), NormalizationApplied::default())
        }
    }
}

/// Walk the items array once and emit per-item telemetry. Recognised
/// item types are tallied; unknown `type` values trigger a
/// `tracing::warn!` `event = responses_unknown_item_type` but never
/// alter the upstream-bound bytes. `image_generation_call.image_data`
/// is never logged verbatim — only its byte length, per spec.
fn log_item_telemetry(parsed: &serde_json::Value, request_id: &str) {
    let items = match parsed
        .get("input")
        .or_else(|| parsed.get("messages"))
        .and_then(|v| v.as_array())
    {
        Some(items) => items,
        None => return,
    };

    use crate::responses_items::classify_items;

    // Build a `RawValue` from the items array so we can use the
    // typed classifier. `to_raw_value` serializes straight into the
    // borrowed form (measured 1.76x vs `to_string` + `from_string`,
    // which serialized and then re-validated the same bytes).
    // Telemetry path, but it runs per Responses request.
    let items_raw = match serde_json::value::to_raw_value(items) {
        Ok(r) => r,
        Err(_) => return,
    };
    let classified = match classify_items(&items_raw) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                event = "responses_classify_error",
                request_id = %request_id,
                error = %e,
                "could not classify Responses items array; passthrough preserves bytes"
            );
            return;
        }
    };

    let by_type = tally_item_types(&classified, request_id);
    // TEMP-CAPTURE (slot sizing for responses-instructions-call-input-slots):
    // per-item byte totals + top-level `instructions` bytes, one line per
    // turn. Remove after measurement.
    {
        let mut bytes_by_type: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for c in classified.iter() {
            *bytes_by_type.entry(c.type_tag).or_insert(0) += c.raw.get().len();
        }
        let instructions_bytes = parsed
            .get("instructions")
            .map(|v| {
                if v.is_string() {
                    v.as_str().unwrap_or("").len()
                } else {
                    v.to_string().len()
                }
            })
            .unwrap_or(0);
        tracing::info!(
            event = "responses_slot_bytes",
            request_id = %request_id,
            instructions_bytes,
            bytes_by_type = ?bytes_by_type,
            "TEMP responses slot byte breakdown",
        );
    }
    tracing::info!(
        event = "responses_item_summary",
        request_id = %request_id,
        items_total = classified.len(),
        breakdown = ?by_type,
        "responses item type breakdown"
    );
}

/// Count classified items by type tag, warning on unknown types (which are
/// preserved verbatim downstream) and redacting image payloads from the log.
/// Returns the per-tag counts for the summary line.
/// Extracted from `log_item_telemetry` without behavior change.
fn tally_item_types<'a>(
    classified: &'a [crate::responses_items::ClassifiedItem<'a>],
    request_id: &str,
) -> std::collections::HashMap<&'a str, usize> {
    use crate::responses_items::ResponseItem;

    let mut by_type: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for c in classified {
        match &c.typed {
            None => {
                // No-silent-fallbacks: log the unknown type at warn,
                // preserving the type tag so operators can grep for it.
                tracing::warn!(
                    event = "responses_unknown_item_type",
                    request_id = %request_id,
                    type_tag = %c.type_tag,
                    raw_bytes = c.raw.get().len(),
                    "responses item with unknown `type` — preserving verbatim"
                );
                *by_type.entry("unknown").or_insert(0) += 1;
            }
            Some(item) => {
                let tag = item.type_tag();
                *by_type.entry(tag).or_insert(0) += 1;
                // Image-generation log redaction. The upstream-bound
                // body is NOT mutated; this only keeps `image_data`
                // out of the structured-log path. We log the tag and
                // a size estimate (the raw item byte length).
                if matches!(item, ResponseItem::ImageGenerationCall { .. }) {
                    tracing::debug!(
                        event = "responses_image_generation_call",
                        request_id = %request_id,
                        item_bytes = c.raw.get().len(),
                        // image_data is intentionally omitted —
                        // base64 image payloads can be megabytes.
                        "image_generation_call seen (image bytes redacted from log)"
                    );
                }
            }
        }
    }
    by_type
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_of(value: serde_json::Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).unwrap())
    }

    #[test]
    fn mode_off_short_circuits() {
        let body = Bytes::from_static(b"not valid json");
        let out = compress_openai_responses_request(
            &body,
            CompressionMode::Off,
            RequestAuthMode::Payg,
            "req-1",
            &[],
        );
        assert!(matches!(
            out,
            Outcome::Passthrough {
                reason: PassthroughReason::ModeOff
            }
        ));
    }

    #[test]
    fn invalid_json_passthrough() {
        let body = Bytes::from_static(b"\x01\x02 not json");
        let out = compress_openai_responses_request(
            &body,
            CompressionMode::LiveZone,
            RequestAuthMode::Payg,
            "req-2",
            &[],
        );
        assert!(matches!(
            out,
            Outcome::Passthrough {
                reason: PassthroughReason::NotJson
            }
        ));
    }

    #[test]
    fn no_input_passthrough() {
        let body = body_of(json!({"model": "gpt-4o"}));
        let out = compress_openai_responses_request(
            &body,
            CompressionMode::LiveZone,
            RequestAuthMode::Payg,
            "req-3",
            &[],
        );
        assert!(matches!(
            out,
            Outcome::Passthrough {
                reason: PassthroughReason::NoMessages
            }
        ));
    }

    #[test]
    fn small_body_no_change() {
        let body = body_of(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}
            ]
        }));
        let out = compress_openai_responses_request(
            &body,
            CompressionMode::LiveZone,
            RequestAuthMode::Payg,
            "req-4",
            &[],
        );
        assert!(matches!(out, Outcome::NoCompression));
    }

    #[test]
    fn e1_sorts_tools_when_payg() {
        // PAYG, Responses-shape body, tools out of order (and using
        // OpenAI's `function`-nested name — same shape as Chat
        // Completions).
        let body = body_of(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "tools": [
                {"type": "function", "function": {"name": "zebra"}},
                {"type": "function", "function": {"name": "apple"}},
            ],
        }));
        let out = compress_openai_responses_request(
            &body,
            CompressionMode::LiveZone,
            RequestAuthMode::Payg,
            "req-e1-resp",
            &[],
        );
        match out {
            Outcome::Compressed {
                strategies_applied, ..
            } => assert!(
                strategies_applied.contains(&"tool_array_sort"),
                "expected tool_array_sort, got {strategies_applied:?}",
            ),
            other => panic!("expected Compressed, got {other:?}"),
        }
    }

    #[test]
    fn e1_passes_through_when_oauth() {
        let body = body_of(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "tools": [
                {"type": "function", "function": {"name": "zebra"}},
                {"type": "function", "function": {"name": "apple"}},
            ],
        }));
        let out = compress_openai_responses_request(
            &body,
            CompressionMode::LiveZone,
            RequestAuthMode::OAuth,
            "req-e1-oauth",
            &[],
        );
        assert!(matches!(out, Outcome::NoCompression));
    }
}
