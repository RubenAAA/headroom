//! Tests for all-messages (content-keyed) compression.
//!
//! Rationale: on a subscription with prompt caching, repeated history is
//! re-sent every turn and cached at 0.1x. To actually reduce consumption the
//! proxy must compress the SAME content identically wherever it appears — not
//! just the latest user message — so Anthropic's cache forms over the
//! compressed bytes (stable, no cascade). The legacy `compress_anthropic_live_zone`
//! only ever rewrites the latest user message, so a tool_result that has aged
//! into history is sent full, then re-compressed when newest → byte oscillation
//! → cache cascade. `compress_anthropic_all_messages` compresses every eligible
//! block deterministically so identical content always yields identical bytes.

use headroom_core::transforms::live_zone::DEFAULT_MODEL;
use headroom_core::transforms::{
    compress_anthropic_all_messages, AuthMode, BlockAction, DispatchConfig, ExclusionReason,
    LiveZoneOutcome,
};
use serde_json::{json, Value};
use std::collections::HashSet;

fn body_of(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

/// 200 homogeneous dicts — SmartCrusher's bread-and-butter (proven compressible
/// by the existing dispatch tests). `salt` makes two payloads distinct content.
fn compressible_payload(salt: &str) -> String {
    let array: Vec<Value> = (0..200)
        .map(|i| {
            json!({
                "id": i,
                "status": "ok",
                "value": format!("repeat-pattern-{}-{}", salt, i % 3),
            })
        })
        .collect();
    serde_json::to_string(&array).unwrap()
}

/// A conversation: user(tool_result) → assistant → user(tool_result). The
/// FIRST user message (index 0) has aged into history; the latest is index 2.
fn two_user_messages_body() -> Vec<u8> {
    body_of(json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "system": "you are a helpful assistant",
        "messages": [
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_old",
                    "content": compressible_payload("old"),
                }],
            },
            { "role": "assistant", "content": "acknowledged" },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_new",
                    "content": compressible_payload("new"),
                }],
            },
        ],
    }))
}

#[test]
fn compresses_tool_results_in_all_user_messages_not_just_latest() {
    let body = two_user_messages_body();
    let out = compress_anthropic_all_messages(
        &body,
        AuthMode::Payg,
        DEFAULT_MODEL,
        &DispatchConfig::default(),
    )
    .expect("dispatcher returns Ok on valid bodies");

    let manifest = match &out {
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
        LiveZoneOutcome::NoChange { manifest } => {
            panic!("expected both user messages compressed; got NoChange. manifest: {manifest:?}")
        }
    };

    let compressed_msgs: HashSet<usize> = manifest
        .block_outcomes
        .iter()
        .filter(|b| matches!(b.action, BlockAction::Compressed { .. }))
        .map(|b| b.message_index)
        .collect();

    assert!(
        compressed_msgs.contains(&2),
        "latest user message (idx 2) must be compressed; got {compressed_msgs:?}"
    );
    assert!(
        compressed_msgs.contains(&0),
        "older user message (idx 0) that aged into history must ALSO be compressed; got {compressed_msgs:?}"
    );
}

/// `--exclude-tools` reaches this mode: a named tool's `tool_result` is
/// kept away from every lossy compressor, in every message it appears in.
#[test]
fn exclude_tools_keeps_named_tool_results_off_the_lossy_path() {
    let payload = compressible_payload("vault");
    let body = body_of(json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t_old", "name": "Vault", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t_old", "content": payload.clone()}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t_new", "name": "Vault", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t_new", "content": payload}
            ]},
        ],
    }));

    let config = DispatchConfig {
        exclude_tools: vec!["Vault".to_string()],
        ..DispatchConfig::default()
    };
    let out = compress_anthropic_all_messages(&body, AuthMode::Payg, DEFAULT_MODEL, &config)
        .expect("dispatcher returns Ok on valid bodies");

    let manifest = match &out {
        LiveZoneOutcome::Modified { manifest, .. } => manifest,
        LiveZoneOutcome::NoChange { manifest } => manifest,
    };

    for block in &manifest.block_outcomes {
        match block.action {
            BlockAction::Compressed { strategy, .. } => panic!(
                "excluded tool_result in message {} reached lossy strategy {strategy}",
                block.message_index
            ),
            BlockAction::Excluded {
                reason: ExclusionReason::ExcludedTool,
            } => {}
            _ => {}
        }
    }
    assert!(
        manifest.block_outcomes.iter().any(|b| matches!(
            b.action,
            BlockAction::Excluded {
                reason: ExclusionReason::ExcludedTool
            }
        )),
        "expected at least one ExcludedTool outcome; got {:?}",
        manifest.block_outcomes
    );
}

/// The property the mode exists for: identical content in two different
/// messages compresses to identical bytes, so the prompt cache forms
/// over the compressed history instead of cascading.
#[test]
fn identical_content_compresses_to_identical_bytes() {
    let payload = compressible_payload("same");
    let body = body_of(json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": payload.clone()}
            ]},
            {"role": "assistant", "content": "acknowledged"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": payload}
            ]},
        ],
    }));

    let out = compress_anthropic_all_messages(
        &body,
        AuthMode::Payg,
        DEFAULT_MODEL,
        &DispatchConfig::default(),
    )
    .expect("dispatcher returns Ok on valid bodies");

    let new_body = match &out {
        LiveZoneOutcome::Modified { new_body, .. } => new_body.get().as_bytes().to_vec(),
        LiveZoneOutcome::NoChange { manifest } => {
            panic!("expected compression; got NoChange. manifest: {manifest:?}")
        }
    };

    let parsed: Value = serde_json::from_slice(&new_body).unwrap();
    let text_at = |idx: usize| {
        parsed["messages"][idx]["content"][0]["content"]
            .as_str()
            .expect("tool_result content stays a string")
            .to_string()
    };
    assert_eq!(
        text_at(0),
        text_at(2),
        "identical content must yield identical bytes in every message"
    );
}
