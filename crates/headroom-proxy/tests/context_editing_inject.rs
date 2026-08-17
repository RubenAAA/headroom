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

    let changed = inject_context_management(&mut body, Some(6), 0, 60_000, None, None);

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

    let changed = inject_context_management(&mut body, Some(6), 0, 60_000, None, None);

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
fn overrides_the_keep_all_the_client_sends_for_clear_thinking() {
    // `keep: "all"` clears nothing, and Claude Code sends it on every request.
    // Treating the family as "already handled" is what made the setting inert.
    let mut body = json!({
        "messages": [],
        "context_management": {
            "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
        }
    });

    let changed = inject_context_management(&mut body, None, 0, 60_000, None, Some(1));

    assert!(changed, "overriding a no-op keep is a modification");
    let edits = body["context_management"]["edits"].as_array().unwrap();
    assert_eq!(
        edits.len(),
        1,
        "override in place, do not add a second edit"
    );
    assert_eq!(
        edits[0]["keep"],
        json!({ "type": "thinking_turns", "value": 1 })
    );
}

#[test]
fn leaves_the_clients_clear_thinking_alone_when_unset() {
    let mut body = json!({
        "messages": [],
        "context_management": {
            "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
        }
    });

    let changed = inject_context_management(&mut body, None, 0, 60_000, None, None);

    assert!(!changed);
    assert_eq!(body["context_management"]["edits"][0]["keep"], json!("all"));
}

#[test]
fn clear_thinking_is_listed_first() {
    // The API requires it; injecting both edits into a bare body must not
    // append thinking after tool_uses.
    let mut body = json!({ "messages": [] });

    let changed = inject_context_management(&mut body, Some(6), 0, 60_000, None, Some(2));

    assert!(changed);
    let edits = body["context_management"]["edits"].as_array().unwrap();
    assert_eq!(
        edits[0]["type"],
        json!("clear_thinking_20251015"),
        "clear_thinking must lead the array; got {edits:?}"
    );
    assert_eq!(edits[1]["type"], json!("clear_tool_uses_20250919"));
}

#[test]
fn clear_at_least_is_attached_only_when_set() {
    let mut with = json!({ "messages": [] });
    inject_context_management(&mut with, Some(6), 0, 60_000, Some(5_000), None);
    assert_eq!(
        with["context_management"]["edits"][0]["clear_at_least"],
        json!({ "type": "input_tokens", "value": 5_000 })
    );

    let mut without = json!({ "messages": [] });
    inject_context_management(&mut without, Some(6), 0, 60_000, None, None);
    assert!(
        without["context_management"]["edits"][0]
            .get("clear_at_least")
            .is_none(),
        "unset must stay off the wire, not go out as null"
    );
}

#[test]
fn an_already_narrowed_clear_thinking_is_left_untouched() {
    // Idempotence: re-running the same policy must report no change, or the
    // caller re-serialises a byte-identical body on every turn.
    let mut body = json!({
        "messages": [],
        "context_management": {
            "edits": [{
                "type": "clear_thinking_20251015",
                "keep": { "type": "thinking_turns", "value": 1 }
            }]
        }
    });

    let changed = inject_context_management(&mut body, None, 0, 60_000, None, Some(1));

    assert!(!changed, "same value is not a modification");
}

fn body_with_messages(n: usize) -> Value {
    let msgs: Vec<Value> = (0..n)
        .map(|i| json!({ "role": if i % 2 == 0 { "user" } else { "assistant" }, "content": "x" }))
        .collect();
    json!({ "model": "claude-opus-5", "messages": msgs })
}

#[test]
fn a_short_conversation_is_left_alone() {
    // The first clear re-creates nearly the whole history, ~60 turns to pay
    // back. A conversation this short would never recover the fee.
    let mut body = body_with_messages(10);

    let changed = inject_context_management(&mut body, Some(20), 40, 30_000, Some(5_000), None);

    assert!(!changed, "below the gate nothing may be injected");
    assert!(
        body.get("context_management").is_none()
            || body["context_management"]["edits"]
                .as_array()
                .is_none_or(Vec::is_empty),
        "no edits array should be left behind: {body}"
    );
}

#[test]
fn a_long_conversation_gets_the_edit() {
    let mut body = body_with_messages(40);

    let changed = inject_context_management(&mut body, Some(20), 40, 30_000, Some(5_000), None);

    assert!(changed, "at the gate exactly, inject");
    assert_eq!(
        body["context_management"]["edits"][0]["type"],
        json!("clear_tool_uses_20250919")
    );
}

#[test]
fn the_gate_does_not_hold_back_clear_thinking() {
    // The gate exists for the tool-use entry fee. Thinking clearing has no such
    // fee, so it must not be caught by the same test.
    let mut body = body_with_messages(4);

    let changed = inject_context_management(&mut body, Some(20), 40, 30_000, None, Some(1));

    assert!(changed);
    let edits = body["context_management"]["edits"].as_array().unwrap();
    assert_eq!(
        edits.len(),
        1,
        "only thinking, and only thinking: {edits:?}"
    );
    assert_eq!(edits[0]["type"], json!("clear_thinking_20251015"));
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

    let changed = inject_context_management(&mut body, Some(6), 0, 60_000, None, None);

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
