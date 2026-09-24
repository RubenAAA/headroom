//! The OpenAI Chat Completions dispatcher.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

// ─── OpenAI Chat Completions live-zone dispatcher (Phase C PR-C2) ────────
//
// Sibling of `compress_anthropic_live_zone`. Same compressor backend,
// same per-content-type byte thresholds, same tokenizer-validated
// rejection gate, same byte-range-surgery rewrite strategy. The
// difference is the walker: Chat Completions defines the live zone as
// the LATEST `role: "tool"` message and the LATEST `role: "user"`
// message (separately, not as a contiguous run). All earlier `tool` /
// `user` messages are part of the cache hot zone — never touched.
//
// Tool messages have shape `{role: "tool", tool_call_id, content}`
// where `content` is either a JSON string (the common case) or an
// array of content parts (rarer; only the string shape is compressible).
// User messages have shape `{role: "user", content}` where `content`
// is either a JSON string or an array of `{type: "text", text}` /
// `{type: "image_url", ...}` blocks; only the text-blocks are eligible.
//
// `n > 1` (multiple completions) is gated *outside* this function by
// the proxy handler — we keep the dispatcher pure and unaware of
// non-determinism semantics.

/// Compress live-zone blocks of an OpenAI Chat Completions request.
///
/// # Provider scope
///
/// `/v1/chat/completions` only. The body shape is:
///
/// ```json
/// { "model": "...", "messages": [ {"role": "...", "content": "..."}, ... ] }
/// ```
///
/// Live zone = the latest `tool` role message's `content` plus the
/// latest `user` role message's text content. Earlier `tool` and
/// `user` messages are frozen (cached prefix); never rewritten.
///
/// Cache-safety invariant matches the Anthropic dispatcher: bytes
/// outside the rewritten ranges are *literally copied* from the input,
/// never re-serialized. PR-C2 integration tests pin SHA-256 byte
/// equality on the prefix and suffix.
pub fn compress_openai_chat_live_zone(
    body_raw: &[u8],
    _auth_mode: AuthMode,
    model: &str,
) -> Result<LiveZoneOutcome, LiveZoneError> {
    compress_openai_chat_live_zone_with_config(
        body_raw,
        _auth_mode,
        model,
        &DispatchConfig::default(),
    )
}

/// [`compress_openai_chat_live_zone`] with an operator [`DispatchConfig`].
///
/// Honors `--exclude-tools` on tool outputs with the same guard lattice as
/// the Anthropic planner ([`collect_tool_guards`] minus the CCR and
/// protected-read arms, which have no OpenAI-path equivalent): verbatim- or
/// byte-exact-listed tools pass through untouched, other excluded tools get
/// the reversible lossless fold only. Unresolvable `tool_call_id`s fail open
/// (normal compression), matching the Anthropic planner's unknown-id path.
pub fn compress_openai_chat_live_zone_with_config(
    body_raw: &[u8],
    _auth_mode: AuthMode,
    model: &str,
    dispatch_config: &DispatchConfig,
) -> Result<LiveZoneOutcome, LiveZoneError> {
    let parsed: Value = serde_json::from_slice(body_raw).map_err(LiveZoneError::BodyNotJson)?;
    let messages = parsed
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(LiveZoneError::NoMessagesArray)?;

    if messages.is_empty() {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest::empty(),
        });
    }

    let messages_total = messages.len();

    // Latest tool / user message indices in the live zone.
    let latest_tool_idx = find_latest_role_index(messages, "tool");
    let latest_user_idx = find_latest_role_index(messages, "user");

    // No live-zone candidates → NoChange.
    if latest_tool_idx.is_none() && latest_user_idx.is_none() {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest {
                messages_total,
                messages_below_frozen_floor: 0,
                latest_user_message_index: latest_user_idx,
                block_outcomes: Vec::new(),
            },
        });
    }

    // Plan replacements for both targets. Each plan returns slots for
    // its own message; we stitch them together into a single
    // replacement vec keyed by ascending byte offset (apply_replacements
    // sorts defensively too).
    let mut all_slots: Vec<(usize, OpenAiPlanSlot)> = Vec::new();
    if let Some(idx) = latest_tool_idx {
        // Body shape doesn't match what we expect → skip planning
        // for the tool message but keep going for the user message.
        if let Ok(slot) = plan_openai_tool_message(body_raw, idx) {
            all_slots.push((idx, slot));
        }
    }
    if let Some(idx) = latest_user_idx {
        if let Ok(slots) = plan_openai_user_message(body_raw, idx) {
            for s in slots {
                all_slots.push((idx, s));
            }
        }
    }

    if all_slots.is_empty() {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest {
                messages_total,
                messages_below_frozen_floor: 0,
                latest_user_message_index: latest_user_idx,
                block_outcomes: Vec::new(),
            },
        });
    }

    let tokenizer = get_tokenizer(model);
    let mut block_outcomes: Vec<BlockOutcome> = Vec::with_capacity(all_slots.len());
    let mut replacements: Vec<Replacement> = Vec::new();

    // Map assistant `tool_calls[].id` → `function.name` so a `role == "tool"`
    // message can be attributed to the tool that produced it. A byte-exact
    // file read (Read/read/read_file/Skill/skill) must reach the model as
    // the bytes the file holds — the Anthropic planner's `ByteExact` guard
    // on the same names. Operator `--exclude-tools` rides the same map with
    // the same lattice as `collect_tool_guards` (verbatim/byte-exact →
    // untouched, other excluded → lossless fold only); unresolvable ids
    // fail open, matching the Anthropic unknown-id path.
    let tool_name_by_call_id: HashMap<&str, &str> = messages
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(Value::as_str)?;
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)?;
            Some((id, name))
        })
        .collect();

    for (msg_idx, slot) in all_slots {
        // Operator + builtin guard for tool outputs only (user prose has no
        // tool attribution and always takes the normal path below). Lattice
        // mirrors `collect_tool_guards` minus the CCR/protected-read arms,
        // which have no OpenAI-path equivalent; unresolvable ids fail open,
        // matching the Anthropic unknown-id path.
        if slot.block_type == "tool_content" {
            let tool_name = messages
                .get(msg_idx)
                .and_then(|m| m.get("tool_call_id"))
                .and_then(Value::as_str)
                .and_then(|id| tool_name_by_call_id.get(id))
                .copied();
            let excluded = tool_name.is_some_and(|name| {
                is_tool_excluded(
                    name,
                    dispatch_config.exclude_tools.iter().map(String::as_str),
                )
            });
            let verbatim = tool_name.is_some_and(is_verbatim_excluded);
            let byte_exact = tool_name.is_some_and(is_byte_exact_excluded);
            // Builtin byte-exact always holds (pre-existing behavior);
            // operator exclusion adds verbatim/byte-exact passthrough.
            if byte_exact || (excluded && verbatim) {
                block_outcomes.push(BlockOutcome {
                    message_index: msg_idx,
                    block_index: slot.block_index,
                    block_type: slot.block_type,
                    action: BlockAction::Excluded {
                        reason: ExclusionReason::ExcludedTool,
                    },
                });
                continue;
            }
            // Other excluded tools: reversible lossless fold only.
            if excluded {
                block_outcomes.push(compact_one_block_lossless(
                    &slot.content_text,
                    slot.content_byte_range,
                    msg_idx,
                    slot.block_index,
                    slot.block_type,
                    tokenizer.as_ref(),
                    &mut replacements,
                ));
                continue;
            }
        }
        let detected = detect_content_type(&slot.content_text);
        let outcome = compress_one_block(
            &slot.content_text,
            detected.content_type,
            slot.content_byte_range,
            msg_idx,
            slot.block_index,
            slot.block_type,
            tokenizer.as_ref(),
            &mut replacements,
            None, // PR-C2: no CCR store yet on the OpenAI path.
            &DispatchConfig::default(),
        );
        block_outcomes.push(outcome);
    }

    let manifest = CompressionManifest {
        messages_total,
        messages_below_frozen_floor: 0,
        latest_user_message_index: latest_user_idx,
        block_outcomes,
    };

    if !manifest.has_compressed_block() || replacements.is_empty() {
        return Ok(LiveZoneOutcome::NoChange { manifest });
    }

    let new_bytes = apply_replacements(body_raw, &mut replacements);
    let new_body_str = match std::str::from_utf8(&new_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(LiveZoneOutcome::NoChange { manifest }),
    };
    let raw = match RawValue::from_string(new_body_str.to_string()) {
        Ok(r) => r,
        Err(_) => return Ok(LiveZoneOutcome::NoChange { manifest }),
    };

    Ok(LiveZoneOutcome::Modified {
        new_body: raw,
        manifest,
    })
}

/// Find the highest index of a message with `role == role`. `None` if
/// no such message exists.
pub(super) fn find_latest_role_index(messages: &[Value], role: &str) -> Option<usize> {
    for (idx, msg) in messages.iter().enumerate().rev() {
        if msg.get("role").and_then(Value::as_str) == Some(role) {
            return Some(idx);
        }
    }
    None
}

/// One OpenAI live-zone plan slot. Mirrors `PlanSlot` but emits the
/// `block_index` and `block_type` shape `compress_one_block` expects.
pub(super) struct OpenAiPlanSlot {
    pub(super) block_index: Option<usize>,
    pub(super) block_type: String,
    pub(super) content_text: String,
    pub(super) content_byte_range: (usize, usize),
}

/// Plan a replacement slot for the tool message at `msg_idx`. Tool
/// messages carry `content` as either a string (compressible) or an
/// array of parts (rare; not compressed in PR-C2 — falls through).
pub(super) fn plan_openai_tool_message(
    body_raw: &[u8],
    msg_idx: usize,
) -> Result<OpenAiPlanSlot, PlanError> {
    let body_str = std::str::from_utf8(body_raw).map_err(|_| PlanError::ParseFailed)?;
    let body: BodyView<'_> = serde_json::from_str(body_str).map_err(|_| PlanError::ParseFailed)?;
    let msg_raw = body
        .messages
        .get(msg_idx)
        .ok_or(PlanError::TargetOutOfBounds)?;

    let msg_view: MessageView<'_> =
        serde_json::from_str(msg_raw.get()).map_err(|_| PlanError::ParseFailed)?;
    let content_raw = msg_view.content.ok_or(PlanError::ParseFailed)?;

    let content_offset_in_msg =
        bytes_offset_of(msg_raw.get(), content_raw.get()).ok_or(PlanError::OffsetMissing)?;
    let msg_offset_in_body =
        bytes_offset_of(body_str, msg_raw.get()).ok_or(PlanError::OffsetMissing)?;
    let content_offset_in_body = msg_offset_in_body + content_offset_in_msg;

    let content_str = content_raw.get();
    if !content_str.starts_with('"') {
        // Non-string content (array of parts). PR-C2 doesn't walk
        // these — treat as not-planned and let the dispatcher record
        // no slot. This is a planner-level skip, not a parse error.
        return Err(PlanError::ParseFailed);
    }

    let unescaped: String =
        serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;

    Ok(OpenAiPlanSlot {
        block_index: None,
        block_type: "tool_content".to_string(),
        content_text: unescaped,
        content_byte_range: (
            content_offset_in_body,
            content_offset_in_body + content_str.len(),
        ),
    })
}

/// Plan replacement slots for the user message at `msg_idx`. User
/// content can be:
///
/// - A JSON string → compressible as a single slot.
/// - An array of parts where each `{type: "text", text}` is a
///   compressible slot. `{type: "image_url", ...}` and other
///   non-text parts are skipped.
pub(super) fn plan_openai_user_message(
    body_raw: &[u8],
    msg_idx: usize,
) -> Result<Vec<OpenAiPlanSlot>, PlanError> {
    let body_str = std::str::from_utf8(body_raw).map_err(|_| PlanError::ParseFailed)?;
    let body: BodyView<'_> = serde_json::from_str(body_str).map_err(|_| PlanError::ParseFailed)?;
    let msg_raw = body
        .messages
        .get(msg_idx)
        .ok_or(PlanError::TargetOutOfBounds)?;

    let msg_view: MessageView<'_> =
        serde_json::from_str(msg_raw.get()).map_err(|_| PlanError::ParseFailed)?;
    let Some(content_raw) = msg_view.content else {
        return Ok(Vec::new());
    };

    let content_offset_in_msg =
        bytes_offset_of(msg_raw.get(), content_raw.get()).ok_or(PlanError::OffsetMissing)?;
    let msg_offset_in_body =
        bytes_offset_of(body_str, msg_raw.get()).ok_or(PlanError::OffsetMissing)?;
    let content_offset_in_body = msg_offset_in_body + content_offset_in_msg;

    let content_str = content_raw.get();

    // Case 1: content is a JSON string.
    if content_str.starts_with('"') {
        let unescaped: String =
            serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;
        return Ok(vec![OpenAiPlanSlot {
            block_index: None,
            block_type: "user_string".to_string(),
            content_text: unescaped,
            content_byte_range: (
                content_offset_in_body,
                content_offset_in_body + content_str.len(),
            ),
        }]);
    }

    // Case 2: content is an array of typed parts.
    let parts: Vec<&RawValue> =
        serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;

    let mut slots = Vec::with_capacity(parts.len());
    for (part_idx, part_raw) in parts.iter().enumerate() {
        let header: BlockHeader<'_> =
            serde_json::from_str(part_raw.get()).map_err(|_| PlanError::ParseFailed)?;
        let block_type = header.r#type.unwrap_or("unknown").to_string();
        if block_type != "text" {
            // Skip image_url / other non-text parts.
            continue;
        }

        // Extract the `text` field byte range.
        #[derive(Deserialize)]
        struct TextHeader<'a> {
            #[serde(borrow, default)]
            text: Option<&'a RawValue>,
        }
        let h: TextHeader<'_> =
            serde_json::from_str(part_raw.get()).map_err(|_| PlanError::ParseFailed)?;
        let Some(text_raw) = h.text else {
            continue;
        };

        let part_offset_in_content =
            bytes_offset_of(content_str, part_raw.get()).ok_or(PlanError::OffsetMissing)?;
        let part_offset_in_body = content_offset_in_body + part_offset_in_content;
        let text_offset_in_part =
            bytes_offset_of(part_raw.get(), text_raw.get()).ok_or(PlanError::OffsetMissing)?;

        let text_str = text_raw.get();
        if !text_str.starts_with('"') {
            continue;
        }
        let unescaped: String =
            serde_json::from_str(text_str).map_err(|_| PlanError::ParseFailed)?;

        let text_start_in_body = part_offset_in_body + text_offset_in_part;
        let text_end_in_body = text_start_in_body + text_str.len();

        slots.push(OpenAiPlanSlot {
            block_index: Some(part_idx),
            block_type: "user_text".to_string(),
            content_text: unescaped,
            content_byte_range: (text_start_in_body, text_end_in_body),
        });
    }

    Ok(slots)
}

#[cfg(test)]
mod openai_chat_tests {
    use super::*;
    use serde_json::json;

    fn body(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn empty_messages_yields_no_change() {
        let b = body(json!({"model": "gpt-4o", "messages": []}));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, DEFAULT_MODEL).unwrap();
        assert!(matches!(out, LiveZoneOutcome::NoChange { .. }));
    }

    #[test]
    fn no_messages_field_errors() {
        let b = body(json!({"model": "gpt-4o"}));
        let err = compress_openai_chat_live_zone(&b, AuthMode::Payg, DEFAULT_MODEL).unwrap_err();
        assert!(matches!(err, LiveZoneError::NoMessagesArray));
    }

    #[test]
    fn invalid_json_errors() {
        let err =
            compress_openai_chat_live_zone(b"not json", AuthMode::Payg, DEFAULT_MODEL).unwrap_err();
        assert!(matches!(err, LiveZoneError::BodyNotJson(_)));
    }

    #[test]
    fn no_user_or_tool_yields_no_change() {
        let b = body(json!({
            "messages": [{"role": "system", "content": "you are helpful"}]
        }));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        assert!(matches!(out, LiveZoneOutcome::NoChange { .. }));
    }

    #[test]
    fn tiny_tool_content_below_threshold_no_change() {
        let b = body(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "doing tool"},
                {"role": "tool", "tool_call_id": "t1", "content": "ok"},
            ]
        }));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                // Both latest tool (idx 2) and latest user (idx 0)
                // contributed a slot; both below threshold.
                assert!(manifest
                    .block_outcomes
                    .iter()
                    .all(|b| matches!(b.action, BlockAction::BelowByteThreshold { .. })));
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn user_array_text_parts_planned() {
        // User content as array of {type: text} + {type: image_url}.
        // Only the text part is planned.
        let b = body(json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe this"},
                    {"type": "image_url", "image_url": {"url": "data:..."}},
                ]
            }]
        }));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert_eq!(manifest.block_outcomes.len(), 1);
                assert_eq!(manifest.block_outcomes[0].block_type, "user_text");
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn picks_latest_tool_only() {
        // Two tool messages; only the latest is in the live zone.
        let b = body(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "tool", "tool_call_id": "t1", "content": "early"},
                {"role": "user", "content": "again"},
                {"role": "tool", "tool_call_id": "t2", "content": "late"},
            ]
        }));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        let manifest = match &out {
            LiveZoneOutcome::NoChange { manifest } => manifest,
            LiveZoneOutcome::Modified { manifest, .. } => manifest,
        };
        // Tool block should reference message index 3 (latest tool),
        // user block index 2 (latest user).
        let tool_block = manifest
            .block_outcomes
            .iter()
            .find(|b| b.block_type == "tool_content")
            .expect("tool block recorded");
        assert_eq!(tool_block.message_index, 3);
        let user_block = manifest
            .block_outcomes
            .iter()
            .find(|b| b.block_type == "user_string")
            .expect("user block recorded");
        assert_eq!(user_block.message_index, 2);
    }

    /// Log-shaped filler big enough to compress on this path.
    fn bulky_log_payload() -> String {
        format!(
            "2024-01-01 12:00:00 INFO  worker: starting up\n{}",
            "2024-01-01 12:00:01 WARN  worker: retrying connection\n".repeat(200)
        )
    }

    /// A byte-exact file read must reach the model as the bytes the file
    /// holds: the tool name resolves through the assistant `tool_calls`,
    /// and the `role == "tool"` message passes through untouched.
    #[test]
    fn byte_exact_tool_message_passes_through() {
        let payload = bulky_log_payload();
        let b = body(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "reading",
                 "tool_calls": [{"id": "t1", "type": "function",
                                "function": {"name": "read", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": payload},
            ]
        }));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                let tool_block = manifest
                    .block_outcomes
                    .iter()
                    .find(|b| b.block_type == "tool_content")
                    .expect("tool block recorded");
                assert!(
                    matches!(
                        tool_block.action,
                        BlockAction::Excluded {
                            reason: ExclusionReason::ExcludedTool
                        }
                    ),
                    "got {:?}",
                    tool_block.action
                );
            }
            LiveZoneOutcome::Modified { .. } => {
                panic!("byte-exact tool message must not rewrite the body")
            }
        }
        // Control: the same payload with no tool_calls attribution still
        // compresses — the gate is name-based, not a blanket tool skip.
        let b = body(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "tool", "tool_call_id": "t9", "content": payload},
            ]
        }));
        let out = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::Modified { manifest, .. } => {
                assert!(
                    manifest
                        .block_outcomes
                        .iter()
                        .any(|b| matches!(b.action, BlockAction::Compressed { .. })),
                    "control: got {:?}",
                    manifest
                        .block_outcomes
                        .iter()
                        .map(|b| &b.action)
                        .collect::<Vec<_>>()
                );
            }
            LiveZoneOutcome::NoChange { .. } => {
                panic!("control: unattributed tool payload must still compress")
            }
        }
    }

    fn excluded_config(exclude: &[&str]) -> DispatchConfig {
        DispatchConfig {
            exclude_tools: exclude.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Operator `--exclude-tools` reaches the OpenAI chat path: an excluded
    /// tool's output must not reach a lossy compressor (lossless fold only),
    /// while the same payload without the exclusion compresses lossily.
    #[test]
    fn excluded_tool_output_is_lossless_only() {
        let payload = format!(
            "2024-01-01 12:00:00 INFO  worker: starting up\n{}",
            "2024-01-01 12:00:01 WARN  worker: retrying connection\n".repeat(200)
        );
        let b = body(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "running",
                 "tool_calls": [{"id": "t1", "type": "function",
                                "function": {"name": "Bash", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": payload},
            ]
        }));
        // Control first: no exclusion → lossy log compressor.
        let lossy = compress_openai_chat_live_zone_with_config(
            &b,
            AuthMode::Payg,
            "gpt-4o",
            &excluded_config(&[]),
        )
        .unwrap();
        assert!(
            matches!(
                &lossy,
                LiveZoneOutcome::Modified { manifest, .. }
                    if manifest.block_outcomes.iter().any(|o| matches!(
                        o.action,
                        BlockAction::Compressed {
                            strategy: STRATEGY_LOG_COMPRESSOR,
                            ..
                        }
                    ))
            ),
            "control: got {:?}",
            lossy
        );
        let out = compress_openai_chat_live_zone_with_config(
            &b,
            AuthMode::Payg,
            "gpt-4o",
            &excluded_config(&["Bash"]),
        )
        .unwrap();
        match &out {
            LiveZoneOutcome::Modified { manifest, .. } => {
                assert!(
                    manifest.block_outcomes.iter().any(|o| matches!(
                        o.action,
                        BlockAction::Compressed {
                            strategy: STRATEGY_EXCLUDED_TOOL_LOSSLESS,
                            ..
                        }
                    )),
                    "got {:?}",
                    manifest
                        .block_outcomes
                        .iter()
                        .map(|o| &o.action)
                        .collect::<Vec<_>>()
                );
                assert!(
                    manifest.block_outcomes.iter().all(|o| !matches!(
                        o.action,
                        BlockAction::Compressed {
                            strategy: STRATEGY_LOG_COMPRESSOR,
                            ..
                        }
                    )),
                    "lossy compressor must not touch excluded output: {:?}",
                    manifest
                        .block_outcomes
                        .iter()
                        .map(|o| &o.action)
                        .collect::<Vec<_>>()
                );
            }
            LiveZoneOutcome::NoChange { manifest } => {
                // Also acceptable: nothing lossy ran. The fold may decline
                // (e.g. tokenizer gate) while still recording Excluded.
                assert!(
                    manifest.block_outcomes.iter().all(|o| !matches!(
                        o.action,
                        BlockAction::Compressed {
                            strategy: STRATEGY_LOG_COMPRESSOR,
                            ..
                        }
                    )),
                    "got {:?}",
                    manifest
                        .block_outcomes
                        .iter()
                        .map(|o| &o.action)
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    /// Empty exclusion list is byte-identical to the default path (the
    /// pre-existing `compress_openai_chat_live_zone` contract).
    #[test]
    fn empty_exclude_list_matches_default_path() {
        let payload = format!(
            "2024-01-01 12:00:00 INFO  worker: starting up\n{}",
            "2024-01-01 12:00:01 WARN  worker: retrying connection\n".repeat(200)
        );
        let b = body(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "running",
                 "tool_calls": [{"id": "t1", "type": "function",
                                "function": {"name": "Bash", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": payload},
            ]
        }));
        let a = compress_openai_chat_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        let c = compress_openai_chat_live_zone_with_config(
            &b,
            AuthMode::Payg,
            "gpt-4o",
            &excluded_config(&[]),
        )
        .unwrap();
        assert_eq!(
            format!("{a:?}"),
            format!("{c:?}"),
            "empty config must match the default path"
        );
    }
}
