//! The OpenAI Responses dispatcher.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

// ─── OpenAI Responses live-zone dispatcher (Phase C PR-C3) ────────────
//
// Sibling of `compress_openai_chat_live_zone`. The Responses API
// (`/v1/responses`) keys the request under `input` rather than
// `messages`, and the array carries explicitly-typed items (not
// role-tagged messages).
//
// Live zone, per spec PR-C3 (`docs/notes/realignment/05-phase-C-rust-proxy.md`):
//
//   - latest `function_call_output.output`
//   - latest `local_shell_call_output.output`
//   - latest `apply_patch_call_output.output`
//   - latest `message` (text content) OR `user`-role message
//
// Earlier `*_output` items are FROZEN (cached prefix) — never touched.
// All other item types (`reasoning`, `compaction`, `mcp_*`,
// `computer_*`, `web_search_call`, `file_search_call`,
// `code_interpreter_call`, `image_generation_call`, `tool_search_call`,
// `custom_tool_call`, `function_call`, `local_shell_call`,
// `apply_patch_call`, future-unknown) are passthrough — the dispatcher
// records a `NoCompressionApplied` outcome but never plans a
// replacement.
//
// Output items must additionally clear a 512-byte minimum
// 167) before the per-content-type byte threshold even runs.

/// Output-item floor below which the Responses dispatcher does not
/// even attempt compression. Matches
/// `responses_items::OUTPUT_ITEM_MIN_BYTES`; pinned here too because
/// `headroom-core` is independent of the proxy crate.
pub(super) const RESPONSES_OUTPUT_MIN_BYTES: usize = 512;

/// Compress live-zone blocks of an OpenAI Responses request.
///
/// # Provider scope
///
/// `/v1/responses` only. The body shape is:
///
/// ```json
/// {
///   "model": "...",
///   "input": [
///     {"type": "message", "role": "user", "content": "..."},
///     {"type": "function_call", "call_id": "c1", "name": "...", "arguments": "..."},
///     {"type": "function_call_output", "call_id": "c1", "output": "..."},
///     {"type": "local_shell_call", ...},
///     {"type": "apply_patch_call", "operation": {...}},
///     ...
///   ]
/// }
/// ```
///
/// Live zone = every current-frame output item with a byte-safe
/// string payload (`function_call_output`, `local_shell_call_output`,
/// `apply_patch_call_output`), except CCR retrieval outputs that must
/// reach the model byte-for-byte.
/// Codex commonly batches parallel tool results in one `response.create`
/// frame; those sibling outputs are all live input for the next model
/// turn. All other item types pass through verbatim.
///
/// Cache-safety invariant matches the Anthropic / Chat dispatchers:
/// bytes outside the rewritten ranges are *literally copied* from the
/// input, never re-serialized.
pub fn compress_openai_responses_live_zone(
    body_raw: &[u8],
    _auth_mode: AuthMode,
    model: &str,
) -> Result<LiveZoneOutcome, LiveZoneError> {
    compress_openai_responses_live_zone_with_config(
        body_raw,
        _auth_mode,
        model,
        &DispatchConfig::default(),
    )
}

/// [`compress_openai_responses_live_zone`] with an operator [`DispatchConfig`].
///
/// Honors `--exclude-tools` on `*_output` items with the same guard lattice
/// as the Anthropic planner (minus the CCR/protected-read arms, which have
/// no Responses-path equivalent). Attribution is direct — `function_call`
/// items carry `call_id` + `name` together, so no cross-message resolution
/// is needed; outputs whose `call_id` matches no call item fail open,
/// matching the Anthropic unknown-id path.
pub fn compress_openai_responses_live_zone_with_config(
    body_raw: &[u8],
    _auth_mode: AuthMode,
    model: &str,
    dispatch_config: &DispatchConfig,
) -> Result<LiveZoneOutcome, LiveZoneError> {
    let parsed: Value = serde_json::from_slice(body_raw).map_err(LiveZoneError::BodyNotJson)?;

    // Responses uses `input`. We accept both `input` and `messages`
    // for forward-compat (some clients alias) — but `input` is the
    // canonical name. If neither field is present, surface
    // `NoMessagesArray` so the proxy can passthrough with a named
    // reason.
    let items = parsed
        .get("input")
        .or_else(|| parsed.get("messages"))
        .and_then(Value::as_array)
        .ok_or(LiveZoneError::NoMessagesArray)?;

    if items.is_empty() {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest::empty(),
        });
    }

    let items_total = items.len();

    // Output items in the current Responses frame are live deltas, not
    // cached history. Codex often sends several sibling tool outputs
    // after parallel local commands; compressing only the last one
    // leaves large same-frame payloads untouched.
    let mut headroom_retrieve_call_ids: HashSet<&str> = HashSet::new();
    // Calls whose output is a file read the agent will patch from. Codex runs
    // shell commands two ways — as a `function_call` carrying JSON arguments,
    // and as its native `local_shell_call` carrying an `action` — so both
    // shapes have to be read or protection covers only half the harness.
    let protect_reads = read_protection_enabled();
    let mut read_command_call_ids: HashSet<&str> = HashSet::new();
    let mut verbatim_tool_call_ids: HashSet<&str> = HashSet::new();
    // File reads whose output the model patches from (the Codex wire shape
    // of the Anthropic planner's `ByteExact` guard): no lossy compressor
    // and no fold may rewrite them. This path has no fold step, so one
    // passthrough covers both.
    let mut byte_exact_tool_call_ids: HashSet<&str> = HashSet::new();
    // Operator `--exclude-tools` attribution. `function_call` items carry
    // `call_id` + `name` together, so outputs resolve directly — no
    // cross-message scan needed. Outputs whose `call_id` matches no call
    // item fail open (normal compression), matching the Anthropic
    // unknown-id path.
    let mut call_name_by_id: HashMap<&str, &str> = HashMap::new();
    for item in items {
        let type_tag = item.get("type").and_then(Value::as_str).unwrap_or("");
        if type_tag == "function_call" {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                call_name_by_id.insert(call_id, name);
                if name == "headroom_retrieve" || name.ends_with("__headroom_retrieve") {
                    headroom_retrieve_call_ids.insert(call_id);
                }
                if is_verbatim_excluded(name) {
                    verbatim_tool_call_ids.insert(call_id);
                }
                if is_byte_exact_excluded(name) {
                    byte_exact_tool_call_ids.insert(call_id);
                }
            }
        }
        if !protect_reads {
            continue;
        }
        let command = match type_tag {
            "function_call" => item.get("arguments").map(tool_call_command_text),
            "local_shell_call" => item.get("action").map(tool_call_command_text),
            // NOTE: no `custom_tool_call` arm (Codex `exec`): its outputs
            // (`custom_tool_call_output`) are not compression candidates on
            // any layer, so there is nothing to protect yet. If that type
            // ever becomes a candidate, wire read protection through
            // `read_protection::custom_tool_call_commands` (which parses
            // `tools.exec_command({"cmd": …})` scripts) before compressing.
            _ => None,
        };
        if let (Some(command), Some(call_id)) =
            (command, item.get("call_id").and_then(Value::as_str))
            && is_read_command(&command)
        {
            read_command_call_ids.insert(call_id);
        }
    }

    let mut output_candidates: Vec<(usize, &str)> = Vec::new();
    // FINDING-012: previously hardcoded None, so documented latest-message
    // compression never ran. Track the last `message` item per spec PR-C3
    // (latest `message` text content); output items above stay live too.
    let mut latest_message: Option<usize> = None;

    for (idx, item) in items.iter().enumerate() {
        let type_tag = item.get("type").and_then(Value::as_str).unwrap_or("");
        match type_tag {
            "function_call_output" | "local_shell_call_output" | "apply_patch_call_output" => {
                let call_id = item.get("call_id").and_then(Value::as_str);
                if call_id.is_some_and(|id| headroom_retrieve_call_ids.contains(id)) {
                    continue;
                }
                output_candidates.push((idx, type_tag));
            }
            "message" if item.get("role").and_then(Value::as_str) == Some("user") => {
                // Only user-role messages are eligible (assistant `message`
                // items are never planned — see assistant_message_not_in_live_zone).
                latest_message = Some(idx);
            }
            "message" => {}
            _ => {}
        }
    }

    let mut candidates = output_candidates;
    if let Some(idx) = latest_message {
        candidates.push((idx, "message"));
    }

    if candidates.is_empty() {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest {
                messages_total: items_total,
                messages_below_frozen_floor: 0,
                latest_user_message_index: latest_message,
                block_outcomes: Vec::new(),
            },
        });
    }

    // Plan replacements per candidate kind. Each plan returns at most
    // one slot (output items have a single string field; messages
    // have a single text content slot).
    let mut all_slots: Vec<(usize, ResponsesPlanSlot)> = Vec::new();
    for (idx, kind_tag) in candidates {
        match plan_responses_item(body_raw, idx, kind_tag) {
            Ok(Some(slot)) => all_slots.push((idx, slot)),
            Ok(None) => {}
            Err(_) => {
                // Body shape doesn't match what we expect for this
                // item — skip it but keep going for the others.
                continue;
            }
        }
    }

    if all_slots.is_empty() {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest {
                messages_total: items_total,
                messages_below_frozen_floor: 0,
                latest_user_message_index: latest_message,
                block_outcomes: Vec::new(),
            },
        });
    }

    let tokenizer = get_tokenizer(model);
    let mut block_outcomes: Vec<BlockOutcome> = Vec::with_capacity(all_slots.len());
    let mut replacements: Vec<Replacement> = Vec::new();

    for (msg_idx, slot) in all_slots {
        // Output items must clear the response-output floor BEFORE the
        // per-content-type threshold even runs. This is on top of the
        // existing per-block byte-threshold gate.
        if slot.is_output_item && slot.content_text.len() < RESPONSES_OUTPUT_MIN_BYTES {
            block_outcomes.push(BlockOutcome {
                message_index: msg_idx,
                block_index: slot.block_index,
                block_type: slot.block_type.clone(),
                action: BlockAction::BelowByteThreshold {
                    content_type: "output_item",
                    byte_count: slot.content_text.len(),
                    threshold_bytes: RESPONSES_OUTPUT_MIN_BYTES,
                },
            });
            continue;
        }
        // The command said this was a file read; the content settles it. Data
        // goes back to its own compressor — nobody patches a build log byte
        // for byte.
        let is_protected_read = items
            .get(msg_idx)
            .and_then(|item| item.get("call_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| read_command_call_ids.contains(id))
            && read_output_should_be_protected(&slot.content_text);
        if is_protected_read {
            block_outcomes.push(BlockOutcome {
                message_index: msg_idx,
                block_index: slot.block_index,
                block_type: slot.block_type.clone(),
                action: BlockAction::Excluded {
                    reason: ExclusionReason::ProtectedRead,
                },
            });
            continue;
        }
        let is_verbatim_tool = items
            .get(msg_idx)
            .and_then(|item| item.get("call_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| verbatim_tool_call_ids.contains(id));
        if is_verbatim_tool {
            block_outcomes.push(BlockOutcome {
                message_index: msg_idx,
                block_index: slot.block_index,
                block_type: slot.block_type.clone(),
                action: BlockAction::Excluded {
                    reason: ExclusionReason::ExcludedTool,
                },
            });
            continue;
        }
        // A file read skips compression entirely: this is the Codex wire,
        // where `read` returns raw file bytes, so any rewrite here breaks
        // the next `Edit(old_string=…)`.
        let is_byte_exact_tool = items
            .get(msg_idx)
            .and_then(|item| item.get("call_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| byte_exact_tool_call_ids.contains(id));
        if is_byte_exact_tool {
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
        // Operator `--exclude-tools` on output items (message slots have no
        // tool attribution and skip this): verbatim/byte-exact members were
        // already passed through above; other excluded tools get the
        // reversible lossless fold only. Unresolvable `call_id`s fail open.
        if slot.is_output_item {
            let excluded = items
                .get(msg_idx)
                .and_then(|item| item.get("call_id"))
                .and_then(Value::as_str)
                .and_then(|id| call_name_by_id.get(id))
                .is_some_and(|name| {
                    is_tool_excluded(
                        name,
                        dispatch_config.exclude_tools.iter().map(String::as_str),
                    )
                });
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
            None, // PR-C3: no CCR store on the Responses path yet.
            &DispatchConfig::default(),
        );
        block_outcomes.push(outcome);
    }

    let manifest = CompressionManifest {
        messages_total: items_total,
        messages_below_frozen_floor: 0,
        latest_user_message_index: latest_message,
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

/// Per-kind plan slot for the Responses dispatcher. Mirrors
/// `OpenAiPlanSlot` but tracks whether the slot is an `*_output` item
/// (so the response-output floor only applies there, not to `message` text).
pub(super) struct ResponsesPlanSlot {
    pub(super) block_index: Option<usize>,
    pub(super) block_type: String,
    pub(super) content_text: String,
    pub(super) content_byte_range: (usize, usize),
    /// True when the slot is one of `function_call_output`,
    /// `local_shell_call_output`, `apply_patch_call_output`. Used to
    /// gate the response-output floor.
    pub(super) is_output_item: bool,
}

/// Body view for the Responses request; accepts both `input` (canonical)
/// and `messages` (alias).
#[derive(Deserialize)]
pub(super) struct ResponsesBodyView<'a> {
    #[serde(borrow, default)]
    pub(super) input: Option<Vec<&'a RawValue>>,
    #[serde(borrow, default)]
    pub(super) messages: Option<Vec<&'a RawValue>>,
}

impl<'a> ResponsesBodyView<'a> {
    pub(super) fn items(&self) -> Option<&Vec<&'a RawValue>> {
        self.input.as_ref().or(self.messages.as_ref())
    }
}

#[derive(Deserialize)]
pub(super) struct OutputItemView<'a> {
    #[serde(borrow, default)]
    pub(super) output: Option<&'a RawValue>,
}

#[derive(Deserialize)]
pub(super) struct MessageItemView<'a> {
    #[serde(borrow, default)]
    pub(super) content: Option<&'a RawValue>,
}

/// Plan a single replacement slot for a Responses item at index
/// `item_idx`. Returns `Ok(None)` when the item exists but has no
/// compressible payload (e.g. message with array content where every
/// part is non-text).
pub(super) fn plan_responses_item(
    body_raw: &[u8],
    item_idx: usize,
    kind_tag: &str,
) -> Result<Option<ResponsesPlanSlot>, PlanError> {
    let body_str = std::str::from_utf8(body_raw).map_err(|_| PlanError::ParseFailed)?;
    let body: ResponsesBodyView<'_> =
        serde_json::from_str(body_str).map_err(|_| PlanError::ParseFailed)?;
    let items = body.items().ok_or(PlanError::ParseFailed)?;
    let item_raw = items.get(item_idx).ok_or(PlanError::TargetOutOfBounds)?;
    let item_offset_in_body =
        bytes_offset_of(body_str, item_raw.get()).ok_or(PlanError::OffsetMissing)?;

    match kind_tag {
        "function_call_output" | "local_shell_call_output" | "apply_patch_call_output" => {
            let view: OutputItemView<'_> =
                serde_json::from_str(item_raw.get()).map_err(|_| PlanError::ParseFailed)?;
            let Some(output_raw) = view.output else {
                return Ok(None);
            };
            let output_offset_in_item = bytes_offset_of(item_raw.get(), output_raw.get())
                .ok_or(PlanError::OffsetMissing)?;
            let output_offset_in_body = item_offset_in_body + output_offset_in_item;
            let output_str = output_raw.get();
            // `output` must be a JSON string for compression to apply.
            // Nested-object `output` (rare) falls through.
            if !output_str.starts_with('"') {
                return Ok(None);
            }
            let unescaped: String =
                serde_json::from_str(output_str).map_err(|_| PlanError::ParseFailed)?;
            Ok(Some(ResponsesPlanSlot {
                block_index: None,
                block_type: kind_tag.to_string(),
                content_text: unescaped,
                content_byte_range: (
                    output_offset_in_body,
                    output_offset_in_body + output_str.len(),
                ),
                is_output_item: true,
            }))
        }
        "message" => {
            let view: MessageItemView<'_> =
                serde_json::from_str(item_raw.get()).map_err(|_| PlanError::ParseFailed)?;
            let Some(content_raw) = view.content else {
                return Ok(None);
            };
            let content_offset_in_item = bytes_offset_of(item_raw.get(), content_raw.get())
                .ok_or(PlanError::OffsetMissing)?;
            let content_offset_in_body = item_offset_in_body + content_offset_in_item;
            let content_str = content_raw.get();

            // Case A: stringly-typed content.
            if content_str.starts_with('"') {
                let unescaped: String =
                    serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;
                return Ok(Some(ResponsesPlanSlot {
                    block_index: None,
                    block_type: "message_string".to_string(),
                    content_text: unescaped,
                    content_byte_range: (
                        content_offset_in_body,
                        content_offset_in_body + content_str.len(),
                    ),
                    is_output_item: false,
                }));
            }

            // Case B: array of typed content parts. The Responses
            // spec uses `{type: "input_text", text: "..."}` and
            // `{type: "output_text", text: "..."}`. Both are
            // compressible. Anything else (image, file, etc.) is
            // skipped.
            let parts: Vec<&RawValue> =
                serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;

            // Pick the first text-shaped part for compression. The
            // common Codex shape has exactly one input_text per
            // user message; the assistant final-answer shape has
            // exactly one output_text. If a future shape carries
            // multiple, we compress the first only — the rest still
            // round-trip byte-equal because we never plan a second
            // slot.
            for (part_idx, part_raw) in parts.iter().enumerate() {
                let header: BlockHeader<'_> =
                    serde_json::from_str(part_raw.get()).map_err(|_| PlanError::ParseFailed)?;
                let block_type = header.r#type.unwrap_or("unknown");
                let is_text = block_type == "input_text"
                    || block_type == "output_text"
                    || block_type == "text";
                if !is_text {
                    continue;
                }

                #[derive(Deserialize)]
                struct TextHeader<'a> {
                    #[serde(borrow, default)]
                    text: Option<&'a RawValue>,
                }
                let h: TextHeader<'_> =
                    serde_json::from_str(part_raw.get()).map_err(|_| PlanError::ParseFailed)?;
                let Some(text_raw) = h.text else { continue };

                let part_offset_in_content =
                    bytes_offset_of(content_str, part_raw.get()).ok_or(PlanError::OffsetMissing)?;
                let part_offset_in_body = content_offset_in_body + part_offset_in_content;
                let text_offset_in_part = bytes_offset_of(part_raw.get(), text_raw.get())
                    .ok_or(PlanError::OffsetMissing)?;

                let text_str = text_raw.get();
                if !text_str.starts_with('"') {
                    continue;
                }
                let unescaped: String =
                    serde_json::from_str(text_str).map_err(|_| PlanError::ParseFailed)?;

                let text_start_in_body = part_offset_in_body + text_offset_in_part;
                let text_end_in_body = text_start_in_body + text_str.len();

                return Ok(Some(ResponsesPlanSlot {
                    block_index: Some(part_idx),
                    block_type: format!("message_{block_type}"),
                    content_text: unescaped,
                    content_byte_range: (text_start_in_body, text_end_in_body),
                    is_output_item: false,
                }));
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod openai_responses_tests {
    use super::*;
    use serde_json::json;

    fn body(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn empty_input_yields_no_change() {
        let b = body(json!({"model": "gpt-4o", "input": []}));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, DEFAULT_MODEL).unwrap();
        assert!(matches!(out, LiveZoneOutcome::NoChange { .. }));
    }

    #[test]
    fn no_input_field_errors() {
        let b = body(json!({"model": "gpt-4o"}));
        let err =
            compress_openai_responses_live_zone(&b, AuthMode::Payg, DEFAULT_MODEL).unwrap_err();
        assert!(matches!(err, LiveZoneError::NoMessagesArray));
    }

    #[test]
    fn invalid_json_errors() {
        let err = compress_openai_responses_live_zone(b"not json", AuthMode::Payg, DEFAULT_MODEL)
            .unwrap_err();
        assert!(matches!(err, LiveZoneError::BodyNotJson(_)));
    }

    #[test]
    fn output_below_512b_skipped() {
        // 256 B output → below the output-item floor.
        let small = "x".repeat(256);
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call_output", "call_id": "c1", "output": small}
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert_eq!(manifest.block_outcomes.len(), 1);
                match &manifest.block_outcomes[0].action {
                    BlockAction::BelowByteThreshold {
                        content_type,
                        byte_count,
                        threshold_bytes,
                    } => {
                        assert_eq!(*content_type, "output_item");
                        assert_eq!(*byte_count, 256);
                        assert_eq!(*threshold_bytes, RESPONSES_OUTPUT_MIN_BYTES);
                    }
                    other => panic!("expected BelowByteThreshold, got {other:?}"),
                }
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn plans_all_same_frame_function_outputs() {
        // Codex can batch parallel tool results in a single
        // response.create frame. They are all current-frame live
        // inputs, so each byte-safe output string gets a slot.
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call_output", "call_id": "c1", "output": "early"},
                {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c2", "output": "late"},
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        let manifest = match &out {
            LiveZoneOutcome::NoChange { manifest } => manifest,
            LiveZoneOutcome::Modified { manifest, .. } => manifest,
        };
        let outputs: Vec<_> = manifest
            .block_outcomes
            .iter()
            .filter(|b| b.block_type == "function_call_output")
            .collect();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].message_index, 0);
        assert_eq!(outputs[1].message_index, 2);
    }

    #[test]
    fn compresses_multiple_same_frame_outputs() {
        let mut first = String::new();
        let mut second = String::new();
        for i in 0..400 {
            first.push_str(&format!(
                "./src/foo_{i}.rs:12: error[E0308]: mismatched types in module foo_{i}\n"
            ));
            second.push_str(&format!(
                "./tests/bar_{i}.rs:44: warning: unused variable in test bar_{i}\n"
            ));
        }
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call_output", "call_id": "c1", "output": first},
                {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c2", "output": second},
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        let manifest = match &out {
            LiveZoneOutcome::NoChange { manifest } => manifest,
            LiveZoneOutcome::Modified { manifest, .. } => manifest,
        };
        let compressed_outputs = manifest
            .block_outcomes
            .iter()
            .filter(|b| {
                b.block_type == "function_call_output"
                    && matches!(b.action, BlockAction::Compressed { .. })
            })
            .count();
        assert_eq!(compressed_outputs, 2, "{manifest:?}");
    }

    /// A byte-exact file read must reach the model as the bytes the file
    /// holds: on this wire the output names its call via `call_id`, and
    /// the matching `function_call` names the tool.
    #[test]
    fn byte_exact_function_call_output_passes_through() {
        let mut payload = String::new();
        for i in 0..400 {
            payload.push_str(&format!(
                "./src/foo_{i}.rs:12: error[E0308]: mismatched types in module foo_{i}\n"
            ));
        }
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": payload},
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert_eq!(manifest.block_outcomes.len(), 1);
                assert!(
                    matches!(
                        manifest.block_outcomes[0].action,
                        BlockAction::Excluded {
                            reason: ExclusionReason::ExcludedTool
                        }
                    ),
                    "got {:?}",
                    manifest.block_outcomes[0].action
                );
            }
            LiveZoneOutcome::Modified { .. } => {
                panic!("byte-exact function_call_output must not rewrite the body")
            }
        }
        // Control: the same payload under a non-read tool still compresses.
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": payload},
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        assert!(
            matches!(out, LiveZoneOutcome::Modified { .. }),
            "control: non-read output must still compress"
        );
    }

    #[test]
    fn unknown_item_types_passthrough_no_slot() {
        // Items the dispatcher doesn't compress — no replacement.
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "reasoning", "id": "r1", "encrypted_content": "opaque"},
                {"type": "compaction", "id": "k1", "encrypted_content": "opaque"},
                {"type": "future_item_v2", "novel": true},
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert!(manifest.block_outcomes.is_empty());
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn large_log_output_compressed() {
        // Compressible build-output style log block with repeated
        // template lines. Above 2 KB so the output floor passes;
        // LogCompressor handles BuildOutput content type.
        let mut log = String::new();
        for i in 0..400 {
            log.push_str(&format!(
                "[2024-01-01 00:00:00] INFO compile.rs:42 building module foo_{i}\n"
            ));
        }
        assert!(log.len() > 2048);
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "local_shell_call_output", "call_id": "c1", "output": log}
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::Modified { new_body, manifest } => {
                let new = new_body.get();
                assert!(new.len() < b.len());
                assert!(
                    manifest
                        .block_outcomes
                        .iter()
                        .any(|b| matches!(b.action, BlockAction::Compressed { .. }))
                );
            }
            LiveZoneOutcome::NoChange { manifest } => {
                // RejectedNotSmaller is also an acceptable outcome
                // for the test fixture; what matters is that the
                // dispatcher *attempted* the compression.
                let attempted = manifest.block_outcomes.iter().any(|b| {
                    matches!(
                        b.action,
                        BlockAction::Compressed { .. } | BlockAction::RejectedNotSmaller { .. }
                    )
                });
                assert!(
                    attempted,
                    "expected dispatcher to attempt compression on a 2KB+ log fixture: {manifest:?}"
                );
            }
        }
    }

    fn excluded_responses_config(exclude: &[&str]) -> DispatchConfig {
        DispatchConfig {
            exclude_tools: exclude.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Operator `--exclude-tools` reaches the Responses path: an excluded
    /// call's output must not reach a lossy compressor. Attribution is
    /// direct (`function_call` carries `call_id` + `name` together).
    #[test]
    fn excluded_function_call_output_is_lossless_only() {
        let mut log = String::new();
        for i in 0..400 {
            log.push_str(&format!(
                "[2024-01-01 00:00:00] INFO compile.rs:42 building module foo_{i}\n"
            ));
        }
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "Bash", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": log}
            ]
        }));
        let out = compress_openai_responses_live_zone_with_config(
            &b,
            AuthMode::Payg,
            "gpt-4o",
            &excluded_responses_config(&["Bash"]),
        )
        .unwrap();
        let manifest = match &out {
            LiveZoneOutcome::Modified { manifest, .. } | LiveZoneOutcome::NoChange { manifest } => {
                manifest
            }
        };
        assert!(
            manifest.block_outcomes.iter().all(|o| !matches!(
                o.action,
                BlockAction::Compressed { strategy, .. } if strategy != STRATEGY_EXCLUDED_TOOL_LOSSLESS
            )),
            "lossy compressor must not touch excluded output: {:?}",
            manifest.block_outcomes.iter().map(|o| &o.action).collect::<Vec<_>>()
        );
    }

    /// Outputs whose `call_id` matches no `function_call` item fail open
    /// (normal compression), matching the Anthropic unknown-id path.
    #[test]
    fn orphan_output_call_id_fails_open() {
        let mut log = String::new();
        for i in 0..400 {
            log.push_str(&format!(
                "[2024-01-01 00:00:00] INFO compile.rs:42 building module foo_{i}\n"
            ));
        }
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "function_call_output", "call_id": "ghost", "output": log}
            ]
        }));
        let out = compress_openai_responses_live_zone_with_config(
            &b,
            AuthMode::Payg,
            "gpt-4o",
            &excluded_responses_config(&["Bash"]),
        )
        .unwrap();
        let manifest = match &out {
            LiveZoneOutcome::Modified { manifest, .. } | LiveZoneOutcome::NoChange { manifest } => {
                manifest
            }
        };
        assert!(
            manifest.block_outcomes.iter().any(|o| matches!(
                o.action,
                BlockAction::Compressed { .. } | BlockAction::RejectedNotSmaller { .. }
            )),
            "orphan output must still compress: {:?}",
            manifest
                .block_outcomes
                .iter()
                .map(|o| &o.action)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn message_user_tiny_content_below_threshold() {
        // FINDING-012: the latest user `message` IS a live-zone candidate
        // (spec PR-C3); tiny content simply yields a BelowByteThreshold
        // NoChange rather than compression.
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "describe this"}]}
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert_eq!(manifest.latest_user_message_index, Some(0));
                assert!(manifest.block_outcomes.iter().all(|o| matches!(
                    o.action,
                    BlockAction::BelowByteThreshold { .. }
                        | BlockAction::RejectedNotSmaller { .. }
                        | BlockAction::NoCompressionApplied { .. }
                )));
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn headroom_retrieve_output_not_in_live_zone() {
        let retrieved = "retrieved original content ".repeat(100);
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_retrieve",
                    "name": "mcp__headroom__headroom_retrieve",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_retrieve",
                    "output": retrieved
                }
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert!(manifest.block_outcomes.is_empty());
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn assistant_message_not_in_live_zone() {
        // Only user messages are eligible. An assistant `message`
        // item is never planned.
        let b = body(json!({
            "model": "gpt-4o",
            "input": [
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "answer"}]}
            ]
        }));
        let out = compress_openai_responses_live_zone(&b, AuthMode::Payg, "gpt-4o").unwrap();
        match &out {
            LiveZoneOutcome::NoChange { manifest } => {
                assert!(manifest.block_outcomes.is_empty());
            }
            _ => panic!("expected NoChange"),
        }
    }

    #[test]
    fn no_change_reason_empty_input_is_no_eligible_items() {
        let manifest = CompressionManifest::empty();
        assert_eq!(
            summarize_openai_responses_no_change_reason(&manifest),
            "no_eligible_items"
        );
    }

    #[test]
    fn no_change_reason_prefers_output_floor() {
        let manifest = CompressionManifest {
            messages_total: 1,
            messages_below_frozen_floor: 0,
            latest_user_message_index: Some(0),
            block_outcomes: vec![BlockOutcome {
                message_index: 0,
                block_index: None,
                block_type: "function_call_output".to_string(),
                action: BlockAction::BelowByteThreshold {
                    content_type: "output_item",
                    byte_count: 1024,
                    threshold_bytes: RESPONSES_OUTPUT_MIN_BYTES,
                },
            }],
        };
        assert_eq!(
            summarize_openai_responses_no_change_reason(&manifest),
            "below_output_floor"
        );
    }
}
