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
    compress_anthropic_all_messages, AuthMode, BlockAction, LiveZoneOutcome,
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
    let out = compress_anthropic_all_messages(&body, AuthMode::Payg, DEFAULT_MODEL)
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
