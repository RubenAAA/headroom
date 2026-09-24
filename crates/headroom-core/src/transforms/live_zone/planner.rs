//! Byte-range planning over the raw Anthropic body: borrowed views, tool
//! guards, replacement plans, and splicing them back in.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Body-shape view used to find byte ranges.
///
/// `&'a RawValue` borrows are pointer-equal to slices into the input
/// buffer; we use this to compute exact byte offsets via the
/// `bytes_offset_of` helper. The struct intentionally only captures
/// the path we need; everything else is left unparsed.
#[derive(Deserialize)]
pub(super) struct BodyView<'a> {
    #[serde(borrow)]
    pub(super) messages: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
pub(super) struct MessageView<'a> {
    #[serde(borrow, default)]
    pub(super) content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
pub(super) struct BlockHeader<'a> {
    #[serde(borrow, default)]
    pub(super) r#type: Option<&'a str>,
    #[serde(borrow, default)]
    pub(super) content: Option<&'a RawValue>,
    /// Present on `tool_result` blocks; names the assistant `tool_use`
    /// block this result answers. Used to resolve the tool's name.
    #[serde(borrow, default)]
    pub(super) tool_use_id: Option<&'a str>,
}

/// Per-block dispatch slot the planner emits.
pub(super) struct PlanSlot {
    pub(super) block_index: usize,
    pub(super) kind: SlotKind,
}

pub(super) enum SlotKind {
    /// Content is a JSON string the dispatcher may compress in place.
    Compressible {
        block_type: String,
        content_text: String,
        content_byte_range: (usize, usize),
    },
    /// String-shaped message content (Anthropic legacy shape: the
    /// whole message's `content` is a JSON string, no per-block
    /// array).
    StringContent {
        content_text: String,
        content_byte_range: (usize, usize),
    },
    /// Content is a JSON string the dispatcher may only fold
    /// losslessly — the block answers a tool the operator excluded, so
    /// no lossy compressor may see it.
    LosslessOnly {
        block_type: String,
        content_text: String,
        content_byte_range: (usize, usize),
    },
    /// Block is ineligible for compression — record but do not
    /// dispatch. Carries the reason so the manifest can report why.
    Excluded {
        block_type: String,
        reason: ExclusionReason,
    },
}

/// What the planner must do with a `tool_result` because of the tool
/// that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolGuard {
    /// Result of a CCR retrieval call — forward byte-for-byte.
    CcrRetrieve,
    /// Excluded tool whose output breaks if rewritten at all, even
    /// reversibly (see `DEFAULT_VERBATIM_EXCLUDE_TOOLS`).
    Verbatim,
    /// Excluded file-read tool whose output must skip the reversible
    /// fold (see `DEFAULT_BYTE_EXACT_EXCLUDE_TOOLS`): the model copies
    /// `Edit(old_string=…)` anchors from the bytes it is shown, so even
    /// a reversible rewrite breaks the next edit. Weaker than
    /// [`ToolGuard::Verbatim`] on purpose — dedup and the age-based
    /// fall-through still apply.
    ByteExact,
    /// Excluded tool: no lossy compressor, but a self-verified
    /// reversible fold may still shrink it.
    LosslessOnly,
    /// The tool call was a file read and `HEADROOM_PROTECT_READS` is on.
    /// Unlike the guards above this one is provisional: it is settled by the
    /// CONTENT of the result, which the planner only has further down, so it
    /// does not short-circuit here.
    ProtectedRead,
}

/// Map assistant `tool_use` ids to the guard their `tool_result` needs.
///
/// Only guarded ids are inserted, so an empty `exclude_tools` yields
/// exactly the CCR-only map the planner saw before the flag existed.
///
/// A `tool_result` answering a CCR retrieval carries content the model
/// just asked to have restored from the CCR store. Compressing it
/// again writes a new `<<ccr:hash>>` marker the agent can never
/// redeem — an unresolvable retrieval loop. That check runs first and
/// unconditionally: an aged-out marker is exactly as unredeemable as a
/// fresh one, so it must never decay into compression, and no operator
/// setting can turn it off.
///
/// Known, accepted tradeoff: `is_ccr_retrieve_tool`'s alias matching
/// strips ANY `mcp__<server>__` prefix before comparing, so a
/// third-party server exposing a tool literally named
/// `headroom_retrieve` matches here too. Narrowing it to headroom's
/// own server would need a bespoke check inconsistent with every
/// other excluded-tool entry; given how specific the name is, the
/// collision risk is accepted rather than special-cased.
pub(super) fn collect_tool_guards(
    messages: &[Value],
    exclude_tools: &[String],
) -> HashMap<String, ToolGuard> {
    let mut guards = HashMap::new();
    // Read once per request rather than per block: the flag cannot change
    // mid-walk, and every `tool_use` would otherwise re-read the environment.
    let protect_reads = read_protection_enabled();
    for msg in messages {
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            let excluded = is_tool_excluded(name, exclude_tools.iter().map(String::as_str));
            let guard = if is_ccr_retrieve_tool(name) {
                ToolGuard::CcrRetrieve
            } else if excluded && is_verbatim_excluded(name) {
                ToolGuard::Verbatim
            } else if excluded && is_byte_exact_excluded(name) {
                // Ahead of `LosslessOnly`: exclusion routes the block INTO
                // the reversible fold, and the fold rewrites the file bytes
                // the model patches from. Byte-exactness is the invariant;
                // the fold is the leak.
                ToolGuard::ByteExact
            } else if protect_reads
                && block
                    .get("input")
                    .is_some_and(|input| is_read_command(&tool_call_command_text(input)))
            {
                // Ahead of `LosslessOnly`: a fold is reversible for us, but the
                // agent reads the folded form and patches from it, so a read has
                // to reach the model as the bytes the file actually holds.
                ToolGuard::ProtectedRead
            } else if excluded {
                ToolGuard::LosslessOnly
            } else {
                continue;
            };
            guards.insert(id.to_string(), guard);
        }
    }
    guards
}

/// Walk the buffered body, return one `PlanSlot` per block in the
/// latest user message. Errors out on shapes the dispatcher does not
/// support (e.g. structured-array `content` inside a tool_result —
/// rare; we degrade to NoChange in that case).
/// Whether a content block (no `type` key) carries a JSON-string `text`
/// field — the Bedrock Converse text-block shape (`{"text": "..."}`).
/// Used to route typeless Converse text through the Anthropic text path.
/// Blocks whose `text` is absent or non-string (e.g. `{"image": ...}`,
/// `{"toolUse": ...}`) return false and stay unrecognized → no-op.
pub(super) fn block_has_string_text_field(block_json: &str) -> bool {
    #[derive(Deserialize)]
    struct Probe<'a> {
        #[serde(borrow, default)]
        text: Option<&'a RawValue>,
    }
    serde_json::from_str::<Probe<'_>>(block_json)
        .ok()
        .and_then(|p| p.text)
        .is_some_and(|t| t.get().trim_start().starts_with('"'))
}

pub(super) fn plan_block_replacements(
    body_raw: &[u8],
    target_msg_idx: usize,
    tool_guards: &HashMap<String, ToolGuard>,
) -> Result<Vec<PlanSlot>, PlanError> {
    // `serde_json::from_slice` requires UTF-8; we re-validate here
    // explicitly so the pointer-arithmetic helper can take a `&str`
    // without unsafe.
    let body_str = std::str::from_utf8(body_raw).map_err(|_| PlanError::ParseFailed)?;
    let body: BodyView<'_> = serde_json::from_str(body_str).map_err(|_| PlanError::ParseFailed)?;
    let target_msg_raw = body
        .messages
        .get(target_msg_idx)
        .ok_or(PlanError::TargetOutOfBounds)?;

    let msg_view: MessageView<'_> =
        serde_json::from_str(target_msg_raw.get()).map_err(|_| PlanError::ParseFailed)?;

    let Some(content_raw) = msg_view.content else {
        return Ok(Vec::new());
    };

    // Compute the byte offset of the message's `content` value into
    // `body_raw`. The target_msg_raw points into body_raw; content_raw
    // points into target_msg_raw's bytes (which are the same backing
    // memory).
    let content_offset_in_msg =
        bytes_offset_of(target_msg_raw.get(), content_raw.get()).ok_or(PlanError::OffsetMissing)?;
    let msg_offset_in_body =
        bytes_offset_of(body_str, target_msg_raw.get()).ok_or(PlanError::OffsetMissing)?;
    let content_offset_in_body = msg_offset_in_body + content_offset_in_msg;

    let content_str = content_raw.get();

    // Case 1: content is a JSON string (Anthropic legacy shape for
    // user messages).
    if content_str.starts_with('"') {
        let unescaped: String =
            serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;
        return Ok(vec![PlanSlot {
            block_index: 0,
            kind: SlotKind::StringContent {
                content_text: unescaped,
                content_byte_range: (
                    content_offset_in_body,
                    content_offset_in_body + content_str.len(),
                ),
            },
        }]);
    }

    // Case 2: content is an array of blocks. Borrow each block as a
    // &RawValue so we can compute its byte range too.
    let blocks: Vec<&RawValue> =
        serde_json::from_str(content_str).map_err(|_| PlanError::ParseFailed)?;

    let mut slots = Vec::with_capacity(blocks.len());
    for (block_idx, block_raw) in blocks.iter().enumerate() {
        let block_offset_in_content =
            bytes_offset_of(content_str, block_raw.get()).ok_or(PlanError::OffsetMissing)?;
        let block_offset_in_body = content_offset_in_body + block_offset_in_content;

        let header: BlockHeader<'_> =
            serde_json::from_str(block_raw.get()).map_err(|_| PlanError::ParseFailed)?;
        // Bedrock Converse content blocks carry no `type` discriminator —
        // the variant is the key itself (`{"text": ...}`, `{"image": ...}`,
        // `{"toolUse": ...}`). A typeless block whose `text` field is a
        // JSON string is Converse text; route it through the same surgical
        // path as an Anthropic `{"type":"text","text":...}` block so
        // Converse user-message text compresses too. Anthropic blocks
        // always carry `type`, so this never alters the Anthropic path.
        let block_type = match header.r#type {
            Some(t) => t.to_string(),
            None if block_has_string_text_field(block_raw.get()) => "text".to_string(),
            None => "unknown".to_string(),
        };

        // Ahead of every other classification: a tool_result may answer a
        // tool whose output must not be compressed. See
        // `collect_tool_guards`. The CCR case in particular is
        // already-retrieved original content and must never be rewritten.
        let tool_guard = if block_type == "tool_result" {
            header
                .tool_use_id
                .and_then(|id| tool_guards.get(id))
                .copied()
        } else {
            None
        };
        // A ctx-offload digest carries its marker in the content, not in a
        // tool_use_id, so the guard map above cannot see it. Check the raw
        // block text: offload has already shrunk this block and stored the
        // original, and a second pass would bury that pointer under one of
        // our own that redeems to the digest.
        if block_raw.get().contains(CTX_OFFLOAD_MARKER_PREFIX) {
            slots.push(PlanSlot {
                block_index: block_idx,
                kind: SlotKind::Excluded {
                    block_type,
                    reason: ExclusionReason::CtxOffloadDigest,
                },
            });
            continue;
        }

        match tool_guard {
            Some(ToolGuard::CcrRetrieve) => {
                slots.push(PlanSlot {
                    block_index: block_idx,
                    kind: SlotKind::Excluded {
                        block_type,
                        reason: ExclusionReason::CcrRetrieveResult,
                    },
                });
                continue;
            }
            Some(ToolGuard::Verbatim) | Some(ToolGuard::ByteExact) => {
                slots.push(PlanSlot {
                    block_index: block_idx,
                    kind: SlotKind::Excluded {
                        block_type,
                        reason: ExclusionReason::ExcludedTool,
                    },
                });
                continue;
            }
            Some(ToolGuard::LosslessOnly) | Some(ToolGuard::ProtectedRead) | None => {}
        }
        let lossless_only = tool_guard == Some(ToolGuard::LosslessOnly);
        let protected_read = tool_guard == Some(ToolGuard::ProtectedRead);

        if HOT_ZONE_BLOCK_TYPES.iter().any(|t| *t == block_type) {
            slots.push(PlanSlot {
                block_index: block_idx,
                kind: SlotKind::Excluded {
                    block_type,
                    reason: ExclusionReason::HotZoneBlockType,
                },
            });
            continue;
        }

        // Find the inner `content` field's byte range. For tool_result
        // blocks this is the field we'd compress. For text blocks
        // it's a `text` field — we read that instead.
        let (inner_field_str, inner_field_offset_in_block) = match block_type.as_str() {
            "tool_result" => {
                let Some(field_raw) = header.content else {
                    // tool_result with no content — skip dispatch.
                    slots.push(PlanSlot {
                        block_index: block_idx,
                        kind: SlotKind::Compressible {
                            block_type,
                            content_text: String::new(),
                            content_byte_range: (block_offset_in_body, block_offset_in_body),
                        },
                    });
                    continue;
                };
                let off = bytes_offset_of(block_raw.get(), field_raw.get())
                    .ok_or(PlanError::OffsetMissing)?;
                (field_raw.get(), off)
            }
            "text" => {
                #[derive(Deserialize)]
                struct TextHeader<'a> {
                    #[serde(borrow, default)]
                    text: Option<&'a RawValue>,
                }
                let h: TextHeader<'_> =
                    serde_json::from_str(block_raw.get()).map_err(|_| PlanError::ParseFailed)?;
                let Some(text_raw) = h.text else {
                    slots.push(PlanSlot {
                        block_index: block_idx,
                        kind: SlotKind::Compressible {
                            block_type,
                            content_text: String::new(),
                            content_byte_range: (block_offset_in_body, block_offset_in_body),
                        },
                    });
                    continue;
                };
                let off = bytes_offset_of(block_raw.get(), text_raw.get())
                    .ok_or(PlanError::OffsetMissing)?;
                (text_raw.get(), off)
            }
            _ => {
                // image, document, etc. — record as compressible
                // block-type but with empty content so no compressor
                // runs.
                slots.push(PlanSlot {
                    block_index: block_idx,
                    kind: SlotKind::Compressible {
                        block_type,
                        content_text: String::new(),
                        content_byte_range: (block_offset_in_body, block_offset_in_body),
                    },
                });
                continue;
            }
        };

        // The compressors expect a plain string, not a JSON-quoted
        // string. `tool_result.content` and `text.text` are
        // either a JSON string or a structured array; we only
        // compress the string shape (B3). Structured-array shape
        // falls through to no-op.
        if !inner_field_str.starts_with('"') {
            slots.push(PlanSlot {
                block_index: block_idx,
                kind: SlotKind::Compressible {
                    block_type,
                    content_text: String::new(),
                    content_byte_range: (block_offset_in_body, block_offset_in_body),
                },
            });
            continue;
        }
        let unescaped: String =
            serde_json::from_str(inner_field_str).map_err(|_| PlanError::ParseFailed)?;

        let inner_field_start_in_body = block_offset_in_body + inner_field_offset_in_block;
        let inner_field_end_in_body = inner_field_start_in_body + inner_field_str.len();

        // The command said this was a file read; the content decides whether it
        // is protected. Data — a JSON array, a diff, a build log — goes back to
        // its own compressor, because nobody patches those byte for byte.
        if protected_read && read_output_should_be_protected(&unescaped) {
            slots.push(PlanSlot {
                block_index: block_idx,
                kind: SlotKind::Excluded {
                    block_type,
                    reason: ExclusionReason::ProtectedRead,
                },
            });
            continue;
        }

        let content_byte_range = (inner_field_start_in_body, inner_field_end_in_body);
        slots.push(PlanSlot {
            block_index: block_idx,
            kind: if lossless_only {
                SlotKind::LosslessOnly {
                    block_type,
                    content_text: unescaped,
                    content_byte_range,
                }
            } else {
                SlotKind::Compressible {
                    block_type,
                    content_text: unescaped,
                    content_byte_range,
                }
            },
        });
    }

    Ok(slots)
}

#[derive(Debug)]
pub(super) enum PlanError {
    /// JSON parse failure on a body-shape view we expected to succeed.
    ParseFailed,
    /// Pointer-arithmetic could not locate a sub-slice's offset.
    /// Should not happen for valid JSON; surfacing rather than
    /// silently degrading.
    OffsetMissing,
    /// Latest-user-message index points past the end of `messages`.
    /// The caller already validated this — surfacing for safety.
    TargetOutOfBounds,
}

/// Compute the byte offset of `child` within `parent` when both are
/// `&str` views into the same backing memory. Returns `None` when
/// `child` does not lie strictly inside `parent`.
///
/// We rely on this trick because `serde_json` does not expose the
/// byte offset of a `&RawValue`; the `RawValue::get()` slice points
/// into the input buffer when `from_slice` / `from_str` was used,
/// so pointer arithmetic recovers it.
pub(super) fn bytes_offset_of(parent: &str, child: &str) -> Option<usize> {
    let parent_start = parent.as_ptr() as usize;
    let parent_end = parent_start + parent.len();
    let child_start = child.as_ptr() as usize;
    if child_start < parent_start || child_start + child.len() > parent_end {
        return None;
    }
    Some(child_start - parent_start)
}

/// One byte-range replacement to apply. Sorted in ascending `range.0`
/// before splicing.
pub(super) struct Replacement {
    pub(super) range: (usize, usize),
    pub(super) replacement: Vec<u8>,
}

/// Apply all `replacements` to `original`, returning the new buffer.
/// `replacements` are sorted in-place by ascending start offset; the
/// caller may inspect them post-call (they remain valid).
pub(super) fn apply_replacements(original: &[u8], replacements: &mut [Replacement]) -> Vec<u8> {
    replacements.sort_by_key(|r| r.range.0);

    // Pre-size: original_len - sum(removed) + sum(replacement_len).
    let removed: usize = replacements.iter().map(|r| r.range.1 - r.range.0).sum();
    let added: usize = replacements.iter().map(|r| r.replacement.len()).sum();
    let mut out = Vec::with_capacity(original.len().saturating_sub(removed) + added);

    let mut cursor = 0usize;
    for r in replacements.iter() {
        out.extend_from_slice(&original[cursor..r.range.0]);
        out.extend_from_slice(&r.replacement);
        cursor = r.range.1;
    }
    out.extend_from_slice(&original[cursor..]);
    out
}
