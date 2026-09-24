//! The Anthropic Messages dispatcher: live-zone and all-messages passes,
//! per-block compression and lossless compaction, and CCR markers.
//!
//! Moved out of `live_zone.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

// ─── Public entry point ────────────────────────────────────────────────

/// Inspect a buffered Anthropic `/v1/messages` body and decide which
/// blocks (if any) to rewrite.
///
/// # Provider scope (Phase B)
///
/// This function only handles the Anthropic Messages API shape:
///
/// - `messages: [{role, content}]`, with `content` either a JSON
///   string or an array of typed blocks (`text`, `tool_result`,
///   `tool_use`, `thinking`, `image`, …).
/// - The "live zone" is the latest `role == "user"` message at or
///   above `frozen_message_count`. Earlier messages are in the
///   prompt cache hot zone and are byte-preserved.
///
/// **Other providers need their own dispatchers** because their
/// request shapes diverge:
///
/// - **OpenAI Chat Completions** (`/v1/chat/completions`) — tool
///   results live in their own `role: "tool"` messages, not nested
///   in user messages. The live zone is the trailing run of
///   `tool` messages plus the latest `user` message.
/// - **OpenAI Responses API** (`/v1/responses`) — the request is
///   keyed under `input` (not `messages`) with item types like
///   `function_call_output` and `reasoning`; live zone is the
///   trailing function-call-output items since the last `message`
///   or `reasoning` item.
/// - **Google Gemini** (`/v1beta/.../:generateContent`) — request
///   is keyed under `contents` (not `messages`), with
///   `function_response` parts (not `tool_result`). Function
///   responses can be either string or structured object.
/// - **Bedrock InvokeModel** — the embedded payload follows the
///   model's native format (Anthropic, Llama, Cohere, …); route
///   to the matching dispatcher.
///
/// Phase C (`docs/notes/realignment/05-phase-C-rust-proxy.md`) introduces the
/// per-provider dispatchers. Each will live as
/// `compress_<provider>_live_zone` and share the cache-safety
/// invariants and the per-content-type compressor backend
/// (SmartCrusher / LogCompressor / SearchCompressor /
/// DiffCompressor / Code) from this module. The
/// [`LiveZoneOutcome`], [`BlockAction`], and
/// [`CompressionManifest`] types are intentionally
/// provider-agnostic so the per-provider dispatchers can return
/// them unchanged.
///
/// # Arguments
///
/// - `body_raw`: the buffered request body as bytes. Must be valid
///   UTF-8 JSON; non-JSON returns [`LiveZoneError::BodyNotJson`].
/// - `frozen_message_count`: hot-zone floor. Indices `< floor` are
///   excluded from dispatch.
/// - `_auth_mode`: reserved for PR-F2; B3 ignores it.
/// - `model`: the upstream model name (e.g. `"claude-3-5-sonnet-20241022"`).
///   Routes the tokenizer registry to the right backend for the
///   per-block token-count check (PR-B4). Pass [`DEFAULT_MODEL`] when
///   the proxy could not extract `body["model"]`.
///
/// # Returns
///
/// - [`LiveZoneOutcome::NoChange`] when no block was rewritten
///   (either nothing was eligible, or every compressor declined /
///   failed / produced larger output).
/// - [`LiveZoneOutcome::Modified`] when at least one block was
///   rewritten — the proxy forwards the new body.
pub fn compress_anthropic_live_zone(
    body_raw: &[u8],
    frozen_message_count: usize,
    auth_mode: AuthMode,
    model: &str,
) -> Result<LiveZoneOutcome, LiveZoneError> {
    compress_anthropic_live_zone_with_ccr(
        body_raw,
        frozen_message_count,
        auth_mode,
        model,
        None,
        &DispatchConfig::default(),
    )
}

/// Same as [`compress_anthropic_live_zone`] but with an optional
/// [`CcrStore`] for retrieval-marker injection (PR-B7).
///
/// When `ccr_store` is `Some(_)` and a compressor produces a strictly
/// smaller block, the dispatcher:
///
/// 1. Computes `hash = compute_key(original_bytes)` (BLAKE3 → 24 hex
///    chars).
/// 2. Stores the original block content in the backend under that hash.
/// 3. Appends the marker `<<ccr:HASH>>` to the compressed block content
///    (newline-separated) so the model can later call
///    use the `headroom_retrieve` tool to recover the original bytes.
///
/// When `ccr_store` is `None` (default for tests, default for the old
/// `compress_anthropic_live_zone` shim), the dispatcher behaves
/// identically to PR-B4 — no markers, no put.
pub fn compress_anthropic_live_zone_with_ccr(
    body_raw: &[u8],
    frozen_message_count: usize,
    _auth_mode: AuthMode,
    model: &str,
    ccr_store: Option<&dyn CcrStore>,
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
    let messages_below_frozen_floor = frozen_message_count.min(messages_total);

    // Latest user message index, restricted to the live zone (>= floor).
    let latest_user_message_index = find_latest_user_message_index(messages, frozen_message_count);

    let Some(target_idx) = latest_user_message_index else {
        return Ok(LiveZoneOutcome::NoChange {
            manifest: CompressionManifest {
                messages_total,
                messages_below_frozen_floor,
                latest_user_message_index: None,
                block_outcomes: Vec::new(),
            },
        });
    };

    // Resolve block ranges (byte offsets into `body_raw`) by walking
    // the body via `RawValue` borrowed slices. The Vec<Replacement>
    // produced here is the surgery plan; we do *not* mutate `body_raw`
    // while computing it.
    let tool_guards = collect_tool_guards(messages, &dispatch_config.exclude_tools);

    let plan = match plan_block_replacements(body_raw, target_idx, &tool_guards) {
        Ok(p) => p,
        Err(_) => {
            // Body shape doesn't match what we expect (e.g. content
            // is not a string and not an array, or messages is shaped
            // unexpectedly). Treat as no-change; the proxy forwards
            // the original bytes verbatim.
            let block_outcomes =
                inspect_latest_user_blocks_value(&messages[target_idx], target_idx)
                    .unwrap_or_default();
            return Ok(LiveZoneOutcome::NoChange {
                manifest: CompressionManifest {
                    messages_total,
                    messages_below_frozen_floor,
                    latest_user_message_index: Some(target_idx),
                    block_outcomes,
                },
            });
        }
    };

    let mut block_outcomes: Vec<BlockOutcome> = Vec::with_capacity(plan.len());
    let mut replacements: Vec<Replacement> = Vec::new();
    // One tokenizer per request — `get_tokenizer` is cheap (it
    // returns a `Box<dyn Tokenizer>` over either a tiktoken-rs handle
    // or an estimator) but counting once per block is a hot path.
    // PR-B4 only invokes the tokenizer on blocks that actually
    // produced compressed output; the byte-threshold gate filters
    // sub-threshold content first.
    let tokenizer = get_tokenizer(model);

    for slot in plan {
        let outcome = match slot.kind {
            SlotKind::Excluded { block_type, reason } => BlockOutcome {
                message_index: target_idx,
                block_index: Some(slot.block_index),
                block_type,
                action: BlockAction::Excluded { reason },
            },
            SlotKind::Compressible {
                block_type,
                content_text,
                content_byte_range,
            } => {
                let detected = detect_content_native(&content_text);
                let outcome: BlockOutcome = compress_one_block(
                    &content_text,
                    detected,
                    content_byte_range,
                    target_idx,
                    Some(slot.block_index),
                    block_type,
                    tokenizer.as_ref(),
                    &mut replacements,
                    ccr_store,
                    dispatch_config,
                );
                outcome
            }
            SlotKind::LosslessOnly {
                block_type,
                content_text,
                content_byte_range,
            } => compact_one_block_lossless(
                &content_text,
                content_byte_range,
                target_idx,
                Some(slot.block_index),
                block_type,
                tokenizer.as_ref(),
                &mut replacements,
            ),
            SlotKind::StringContent {
                content_text,
                content_byte_range,
            } => {
                let detected = detect_content_native(&content_text);
                compress_one_block(
                    &content_text,
                    detected,
                    content_byte_range,
                    target_idx,
                    None,
                    "string_content".to_string(),
                    tokenizer.as_ref(),
                    &mut replacements,
                    ccr_store,
                    dispatch_config,
                )
            }
        };
        block_outcomes.push(outcome);
    }

    let manifest = CompressionManifest {
        messages_total,
        messages_below_frozen_floor,
        latest_user_message_index: Some(target_idx),
        block_outcomes,
    };

    if !manifest.has_compressed_block() || replacements.is_empty() {
        return Ok(LiveZoneOutcome::NoChange { manifest });
    }

    // Build the new body via byte-range surgery. Replacements are
    // produced in ascending block order; sort defensively.
    let new_bytes = apply_replacements(body_raw, &mut replacements);

    // The output is always still valid JSON: every replacement is a
    // JSON string slot replaced by another JSON string slot. We could
    // round-trip-verify with `serde_json::from_slice` and bail out to
    // NoChange on failure, but that doubles parse cost on the hot
    // path. Rely on type discipline; the byte_fidelity test in
    // `live_zone_dispatch.rs` pins correctness.
    let new_body_str = match std::str::from_utf8(&new_bytes) {
        Ok(s) => s,
        Err(_) => {
            // Should be impossible: input was valid JSON (UTF-8) and
            // every replacement was a JSON-encoded string (also UTF-8).
            // Fall back rather than risk shipping malformed bytes.
            return Ok(LiveZoneOutcome::NoChange { manifest });
        }
    };
    let raw = match RawValue::from_string(new_body_str.to_string()) {
        Ok(r) => r,
        Err(_) => {
            // Same defensive bail-out; should not happen.
            return Ok(LiveZoneOutcome::NoChange { manifest });
        }
    };

    Ok(LiveZoneOutcome::Modified {
        new_body: raw,
        manifest,
    })
}

// ─── All-messages dispatcher (subscription mode) ───────────────────────

/// Compress ALL eligible blocks across ALL user messages in the request.
///
/// This is the subscription-mode variant: on a subscription with prompt
/// caching, repeated history is re-sent every turn and cached at 0.1x.
/// To actually reduce consumption the proxy must compress the SAME
/// content identically wherever it appears — not just the latest user
/// message — so Anthropic's cache forms over the compressed bytes
/// (stable, no cascade).
///
/// The legacy `compress_anthropic_live_zone` only ever rewrites the
/// latest user message, so a tool_result that has aged into history is
/// sent full, then re-compressed when newest → byte oscillation → cache
/// cascade. This function compresses every eligible block
/// deterministically so identical content always yields identical bytes.
///
/// # Arguments
///
/// - `body_raw`: the buffered request body as bytes. Must be valid JSON.
/// - `auth_mode`: reserved for future use (PR-F2).
/// - `model`: the upstream model name for tokenizer routing.
/// - `dispatch_config`: per-request dispatch knobs. Only fields that are a
///   pure function of the request (notably `exclude_tools`) may be set here —
///   anything that varies turn-to-turn for the same content would break the
///   identical-content → identical-bytes property this mode exists for.
///
/// # Returns
///
/// - [`LiveZoneOutcome::NoChange`] when no block was rewritten.
/// - [`LiveZoneOutcome::Modified`] when at least one block was rewritten.
pub fn compress_anthropic_all_messages(
    body_raw: &[u8],
    _auth_mode: AuthMode,
    model: &str,
    ccr_store: Option<&dyn CcrStore>,
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
    let tokenizer = get_tokenizer(model);
    // `--exclude-tools` guards apply here exactly as they do on the
    // live-zone path; the unconditional CCR guard rides along inside
    // `collect_tool_guards`.
    let tool_guards = collect_tool_guards(messages, &dispatch_config.exclude_tools);
    let mut block_outcomes: Vec<BlockOutcome> = Vec::new();
    let mut replacements: Vec<Replacement> = Vec::new();

    // Walk ALL user messages and compress eligible blocks in each.
    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }

        // Plan block replacements for this message.
        let plan = match plan_block_replacements(body_raw, msg_idx, &tool_guards) {
            Ok(p) => p,
            Err(_) => continue, // Skip messages with unexpected shape.
        };

        for slot in plan {
            let outcome = match slot.kind {
                SlotKind::Excluded { block_type, reason } => BlockOutcome {
                    message_index: msg_idx,
                    block_index: Some(slot.block_index),
                    block_type,
                    action: BlockAction::Excluded { reason },
                },
                SlotKind::Compressible {
                    block_type,
                    content_text,
                    content_byte_range,
                } => {
                    let detected = detect_content_native(&content_text);
                    compress_one_block(
                        &content_text,
                        detected,
                        content_byte_range,
                        msg_idx,
                        Some(slot.block_index),
                        block_type,
                        tokenizer.as_ref(),
                        &mut replacements,
                        ccr_store,
                        dispatch_config,
                    )
                }
                SlotKind::LosslessOnly {
                    block_type,
                    content_text,
                    content_byte_range,
                } => compact_one_block_lossless(
                    &content_text,
                    content_byte_range,
                    msg_idx,
                    Some(slot.block_index),
                    block_type,
                    tokenizer.as_ref(),
                    &mut replacements,
                ),
                SlotKind::StringContent {
                    content_text,
                    content_byte_range,
                } => {
                    let detected = detect_content_native(&content_text);
                    compress_one_block(
                        &content_text,
                        detected,
                        content_byte_range,
                        msg_idx,
                        None,
                        "string_content".to_string(),
                        tokenizer.as_ref(),
                        &mut replacements,
                        None,
                        dispatch_config,
                    )
                }
            };
            block_outcomes.push(outcome);
        }
    }

    // Find the latest user message index for the manifest.
    let latest_user_message_index = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, msg)| msg.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(idx, _)| idx);

    let manifest = CompressionManifest {
        messages_total,
        messages_below_frozen_floor: 0, // All messages are eligible in this mode.
        latest_user_message_index,
        block_outcomes,
    };

    if !manifest.has_compressed_block() || replacements.is_empty() {
        return Ok(LiveZoneOutcome::NoChange { manifest });
    }

    // Apply all replacements. Replacements are from different messages
    // at different byte offsets — apply_replacements sorts them.
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

// ─── Internal helpers ──────────────────────────────────────────────────

/// Per-block dispatch shared by the array-of-blocks slot and the
/// legacy string-content slot. Encapsulates the PR-B4 sequence:
///
/// 1. Per-content-type byte threshold — sub-threshold content is
///    tagged `BelowByteThreshold` and the dispatcher does not even
///    invoke a compressor.
/// 2. Type-aware dispatch (`dispatch_compressor`).
/// 3. Tokenizer-validated rejection — if `compressed_tokens >=
///    original_tokens` keep the original and tag `RejectedNotSmaller`
///    (note: tokens, not bytes, drive the gate).
/// 4. Otherwise record the replacement and tag `Compressed`.
#[allow(clippy::too_many_arguments)]
pub(super) fn compress_one_block(
    content_text: &str,
    content_type: ContentType,
    content_byte_range: (usize, usize),
    message_index: usize,
    block_index: Option<usize>,
    block_type: String,
    tokenizer: &dyn crate::tokenizer::Tokenizer,
    replacements: &mut Vec<Replacement>,
    ccr_store: Option<&dyn CcrStore>,
    dispatch_config: &DispatchConfig,
) -> BlockOutcome {
    // 1. Byte-threshold gate. Empty content always falls through to
    //    `dispatch_compressor` (which short-circuits on empty), so
    //    only check when the slot has real bytes — this preserves
    //    the existing "tool_result with no inner content" pathway.
    if !content_text.is_empty() && content_text.len() < threshold_for(content_type) {
        return BlockOutcome {
            message_index,
            block_index,
            block_type,
            action: BlockAction::BelowByteThreshold {
                content_type: content_type.as_str(),
                byte_count: content_text.len(),
                threshold_bytes: threshold_for(content_type),
            },
        };
    }

    match dispatch_compressor_with_config(content_text, content_type, dispatch_config) {
        DispatchResult::NoOp {
            content_type,
            declined_by,
        } => BlockOutcome {
            message_index,
            block_index,
            block_type,
            action: BlockAction::NoCompressionApplied {
                content_type: content_type.to_string(),
                declined_by: declined_by.map(str::to_string),
            },
        },
        DispatchResult::Compressed {
            strategy,
            compressed,
        } => {
            let original_bytes = content_text.len();
            // PR-B7: when a CCR store is wired, persist the original
            // block content keyed by `BLAKE3(original)[..24]` and append
            // the `<<ccr:HASH>>` marker to the compressed string. The
            // marker stays on a fresh trailing line so it is easy for
            // the model to spot and so that the per-content-type
            // compressors (which already produce trailing summary
            // lines) keep their final newline before the marker.
            //
            // The token-validation gate (step 3) is computed against
            // the marker-augmented string so the saved-token check
            // stays honest — the marker costs ~6 tokens and we'd
            // rather forward the original than ship a bigger payload
            // for a 5-byte block.
            let (compressed_for_replacement, ccr_hash_emitted) =
                maybe_inject_ccr_marker(content_text, &compressed, ccr_store);
            let compressed_bytes = compressed_for_replacement.len();
            // 3. Tokenizer-validated rejection. Per PR-B4 spec we
            //    count both the original and compressed strings
            //    using the model's tokenizer; the compression is
            //    accepted only when it shrinks the token count.
            //    Bytes-shrinking-but-tokens-growing happens for
            //    pathological inputs (e.g. dense base64 → tokenizer
            //    fragments more aggressively after a transform).
            let original_tokens = tokenizer.count_text(content_text);
            let compressed_tokens = tokenizer.count_text(&compressed_for_replacement);
            if compressed_tokens >= original_tokens {
                BlockOutcome {
                    message_index,
                    block_index,
                    block_type,
                    action: BlockAction::RejectedNotSmaller {
                        strategy,
                        original_bytes,
                        compressed_bytes,
                        original_tokens,
                        compressed_tokens,
                    },
                }
            } else {
                // Only persist to the CCR store once the rejection
                // gate has admitted the compression — otherwise we
                // populate the store with hashes whose markers
                // never reach the wire (still correct, but wastes
                // storage capacity).
                if let (Some(store), Some(hash)) = (ccr_store, ccr_hash_emitted.as_deref())
                    && !store.put(hash, content_text)
                {
                    tracing::warn!(
                        event = "ccr_put_failed",
                        target = "ccr.live_zone",
                        hash = %hash,
                        "ccr_put_failed; marker will point at an unretrievable hash"
                    );
                }
                let replacement_bytes = serde_json::to_vec(&compressed_for_replacement)
                    .expect("string is always JSON-encodable");
                replacements.push(Replacement {
                    range: content_byte_range,
                    replacement: replacement_bytes,
                });
                BlockOutcome {
                    message_index,
                    block_index,
                    block_type,
                    action: BlockAction::Compressed {
                        strategy,
                        original_bytes,
                        compressed_bytes,
                        original_tokens,
                        compressed_tokens,
                    },
                }
            }
        }
        DispatchResult::Error { strategy, error } => BlockOutcome {
            message_index,
            block_index,
            block_type,
            action: BlockAction::CompressorError { strategy, error },
        },
    }
}

/// The `compact_lossless` kind that fits a detected content type, or
/// `None` when no byte-reversible fold covers that shape.
///
/// Deliberately narrow: every kind listed here verifies its own inverse
/// and returns the input untouched when the round-trip fails, so a
/// mismatch costs a wasted fold rather than mangled content. Shapes with
/// no entry — source code, diffs, JSON arrays, prose, HTML — are passed
/// through whole. There is no lossy fallback: that is the point of the
/// exclusion.
pub(super) fn lossless_kind_for(content_type: ContentType) -> Option<&'static str> {
    match content_type {
        ContentType::BuildOutput => Some("log"),
        ContentType::SearchResults => Some("search"),
        ContentType::StructuredConfig => Some("config"),
        _ => None,
    }
}

/// Handle one block belonging to an excluded tool: fold it losslessly if
/// its shape allows, otherwise forward it unchanged.
///
/// No CCR marker is minted here. A marker exists so the model can ask
/// for bytes the compressor dropped; a reversible fold drops nothing, so
/// there is nothing to retrieve.
#[allow(clippy::too_many_arguments)]
pub(super) fn compact_one_block_lossless(
    content_text: &str,
    content_byte_range: (usize, usize),
    message_index: usize,
    block_index: Option<usize>,
    block_type: String,
    tokenizer: &dyn crate::tokenizer::Tokenizer,
    replacements: &mut Vec<Replacement>,
) -> BlockOutcome {
    let unchanged = |block_type: String| BlockOutcome {
        message_index,
        block_index,
        block_type,
        action: BlockAction::Excluded {
            reason: ExclusionReason::ExcludedTool,
        },
    };

    let Some(kind) = lossless_kind_for(detect_content_native(content_text)) else {
        return unchanged(block_type);
    };
    let compacted = crate::transforms::lossless_compaction::compact_lossless(content_text, kind);
    if compacted.len() >= content_text.len() {
        return unchanged(block_type);
    }

    // Same tokenizer-validated gate the lossy path uses: fewer bytes can
    // still mean more tokens, and shipping a bigger payload helps nobody.
    let original_tokens = tokenizer.count_text(content_text);
    let compressed_tokens = tokenizer.count_text(&compacted);
    if compressed_tokens >= original_tokens {
        return BlockOutcome {
            message_index,
            block_index,
            block_type,
            action: BlockAction::RejectedNotSmaller {
                strategy: STRATEGY_EXCLUDED_TOOL_LOSSLESS,
                original_bytes: content_text.len(),
                compressed_bytes: compacted.len(),
                original_tokens,
                compressed_tokens,
            },
        };
    }

    replacements.push(Replacement {
        range: content_byte_range,
        replacement: serde_json::to_vec(&compacted).expect("string is always JSON-encodable"),
    });
    BlockOutcome {
        message_index,
        block_index,
        block_type,
        action: BlockAction::Compressed {
            strategy: STRATEGY_EXCLUDED_TOOL_LOSSLESS,
            original_bytes: content_text.len(),
            compressed_bytes: compacted.len(),
            original_tokens,
            compressed_tokens,
        },
    }
}

/// Walk `messages` from the back, returning the index of the latest
/// `role == "user"` message. Restricted to indices `>= floor`; if
/// the latest user message lies in the cache hot zone we return
/// `None` (it's out of bounds for live-zone work).
pub(super) fn find_latest_user_message_index(messages: &[Value], floor: usize) -> Option<usize> {
    let start = floor.min(messages.len());
    for (offset, msg) in messages.iter().enumerate().rev() {
        if offset < start {
            return None;
        }
        if msg.get("role").and_then(Value::as_str) == Some("user") {
            return Some(offset);
        }
    }
    None
}

/// PR-B7: append a `<<ccr:HASH>>` retrieval marker to the compressed
/// block content when a CCR store is wired. Returns the
/// (possibly-augmented) compressed string and the hash that was
/// emitted (so the caller can decide whether to put the original into
/// the store after the rejection gate). When `ccr_store` is `None`,
/// returns the input compressed string unchanged with `None`.
///
/// The marker is appended on its own line — `\n<<ccr:HASH>>` — so:
///
/// 1. The marker is unambiguously after the compressor's last byte,
///    even if that byte was a newline already (we only add one).
/// 2. Markers are easy to detect in human-readable diffs / logs.
/// 3. The Python `inject_ccr_retrieve_tool` regex in
///    `headroom/ccr/tool_injection.py` keeps working — it matches
///    `[a-f0-9]{24}` anywhere in the text.
pub(super) fn maybe_inject_ccr_marker(
    original: &str,
    compressed: &str,
    ccr_store: Option<&dyn CcrStore>,
) -> (String, Option<String>) {
    if ccr_store.is_none() {
        return (compressed.to_string(), None);
    }
    let hash = compute_key(original.as_bytes());
    let marker = marker_for(&hash);
    let augmented = if compressed.ends_with('\n') {
        format!("{compressed}{marker}")
    } else {
        format!("{compressed}\n{marker}")
    };
    (augmented, Some(hash))
}

/// Fallback when byte-range planning fails: still record per-block
/// outcomes so observability covers the request. Mirrors PR-B2's
/// observation-only path.
pub(super) fn inspect_latest_user_blocks_value(
    message: &Value,
    message_index: usize,
) -> Option<Vec<BlockOutcome>> {
    let content = message.get("content")?;

    if content.as_str().is_some() {
        return Some(vec![BlockOutcome {
            message_index,
            block_index: None,
            block_type: "string_content".to_string(),
            action: BlockAction::NoCompressionApplied {
                content_type: "text".to_string(),
                declined_by: None,
            },
        }]);
    }

    let blocks = content.as_array()?;
    let mut outcomes = Vec::with_capacity(blocks.len());
    for (idx, block) in blocks.iter().enumerate() {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let action = if HOT_ZONE_BLOCK_TYPES.iter().any(|t| *t == block_type) {
            BlockAction::Excluded {
                reason: ExclusionReason::HotZoneBlockType,
            }
        } else {
            BlockAction::NoCompressionApplied {
                content_type: "unknown".to_string(),
                declined_by: None,
            }
        };
        outcomes.push(BlockOutcome {
            message_index,
            block_index: Some(idx),
            block_type,
            action,
        });
    }
    Some(outcomes)
}
