//! Tests for context-editing injection: the proxy adds Anthropic's native
//! `clear_tool_uses_20250919` (and optionally `clear_thinking_20251015`)
//! directives so subscription users get the server-side context GC that
//! Claude Code gates behind ant-only flags. Must MERGE into any existing
//! `context_management.edits` (Claude Code already sends `clear_thinking`),
//! never clobber, and never duplicate an edit type already present.

use headroom_proxy::compression::context_editing::inject_context_management;
use serde_json::{json, Value};

#[test]
fn injects_clear_tool_uses_into_body_without_context_management() {
    let mut body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{ "role": "user", "content": "hi" }],
    });

    let changed = inject_context_management(&mut body, Some(6), 60_000, None);

    assert!(changed, "injection must report a modification");
    let edits = body["context_management"]["edits"]
        .as_array()
        .expect("edits array present");
    let tu = edits
        .iter()
        .find(|e| e["type"] == json!("clear_tool_uses_20250919"))
        .expect("clear_tool_uses edit present");
    assert_eq!(
        tu["trigger"],
        json!({ "type": "input_tokens", "value": 60_000 })
    );
    assert_eq!(tu["keep"], json!({ "type": "tool_uses", "value": 6 }));
}

#[test]
fn merges_without_clobbering_existing_clear_thinking() {
    // Claude Code already sent a clear_thinking edit — it must survive.
    let mut body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{ "role": "user", "content": "hi" }],
        "context_management": {
            "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
        }
    });

    let changed = inject_context_management(&mut body, Some(6), 60_000, None);

    assert!(changed);
    let edits = body["context_management"]["edits"].as_array().unwrap();
    let types: Vec<&Value> = edits.iter().map(|e| &e["type"]).collect();
    assert!(
        types.contains(&&json!("clear_thinking_20251015")),
        "pre-existing clear_thinking must be preserved; got {types:?}"
    );
    assert!(
        types.contains(&&json!("clear_tool_uses_20250919")),
        "clear_tool_uses must be appended; got {types:?}"
    );
}

#[test]
fn does_not_duplicate_an_edit_type_already_present() {
    // If a clear_tool_uses edit is already present, don't add a second one.
    let mut body = json!({
        "messages": [],
        "context_management": {
            "edits": [{ "type": "clear_tool_uses_20250919", "keep": { "type": "tool_uses", "value": 3 } }]
        }
    });

    let changed = inject_context_management(&mut body, Some(6), 60_000, None);

    let edits = body["context_management"]["edits"].as_array().unwrap();
    let count = edits
        .iter()
        .filter(|e| e["type"] == json!("clear_tool_uses_20250919"))
        .count();
    assert_eq!(
        count, 1,
        "must not duplicate an existing clear_tool_uses edit"
    );
    assert!(
        !changed,
        "no modification when the edit type is already present"
    );
}
