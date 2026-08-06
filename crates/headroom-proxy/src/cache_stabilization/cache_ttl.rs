//! B1 — force a 1-hour prompt-cache TTL.
//!
//! # What it does
//!
//! Rewrites every `cache_control` marker in an Anthropic request body to carry
//! `"ttl": "1h"`, so the provider holds the cached prefix for an hour instead of
//! the five-minute default. Markers are found wherever Anthropic accepts them:
//! on `tools[]` entries, on `system[]` blocks, and on `messages[].content[]`
//! blocks.
//!
//! Nothing else about the marker changes — `type` stays `ephemeral`, and a
//! marker that already says `1h` is left alone so the common case moves no
//! bytes.
//!
//! # What this is worth, and when
//!
//! The win is only ever the turns that would have re-created the prefix because
//! the idle gap crossed five minutes but not an hour. Nothing else: within five
//! minutes both TTLs hit, past an hour both miss.
//!
//! Two documented facts set the economics, and they point opposite ways.
//!
//! For **rate limits** ([rate-limits]), cache writes count at their raw token
//! count with no TTL distinction — `cache_creation_input_tokens` counts toward
//! ITPM whether the entry lives 5 minutes or an hour. On that axis forcing 1h
//! costs nothing and can only help.
//!
//! For **dollars** ([prompt-caching]), a 1h write is priced at 2× base input
//! against 1.25× for 5m — 60% more per creation. So this is free on a
//! subscription, where the usage window is token-counted, and a real bill on
//! PAYG. Hence the PAYG gate at the call site.
//!
//! [rate-limits]: https://platform.claude.com/docs/en/api/rate-limits
//! [prompt-caching]: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
//!
//! # What it is worth here
//!
//! Less than it sounds, for two reasons the measurements turned up.
//!
//! The 5-minute cache **refreshes on every use, at no cost**, so during
//! sustained work it never lapses — B1 only ever rescues a gap since the *last
//! touch*, not since creation. On the 39-capture corpus no gap between
//! consecutive turns exceeded 231 seconds, so B1 would have rescued nothing.
//! That corpus is a single 13-minute burst though, which structurally cannot
//! contain the gaps B1 exists to survive; it is evidence that B1 does nothing
//! during sustained work, not evidence about resuming after lunch.
//!
//! And Claude Code already asks for `1h` itself on the main conversation. In the
//! corpus only subagent traffic used the 5m default, and subagents run to
//! completion in seconds. Against this client B1 mostly restates what the client
//! already does; it earns its keep against clients that never set a TTL.
//!
//! # One-time cost
//!
//! A 5m and a 1h breakpoint are different cache entries, so the turn that flips
//! a marker writes a fresh prefix. Turning B1 on costs one cache creation per
//! affected conversation.
//!
//! No beta header is needed: the 1-hour TTL went generally available on
//! 2025-08-13 and `extended-cache-ttl-2025-04-11` is no longer required.

use serde_json::Value;

/// The TTL string Anthropic accepts for the extended cache.
const ONE_HOUR: &str = "1h";

/// Rewrite one marker in place. Returns `true` when bytes changed.
fn pin_marker(marker: &mut Value) -> bool {
    let Some(obj) = marker.as_object_mut() else {
        return false;
    };
    // Only ephemeral markers carry a TTL. Anything else is a shape we do not
    // recognise and must not rewrite.
    if obj.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return false;
    }
    if obj.get("ttl").and_then(Value::as_str) == Some(ONE_HOUR) {
        return false;
    }
    obj.insert("ttl".to_string(), Value::String(ONE_HOUR.to_string()));
    true
}

/// Pin `cache_control.ttl` to `1h` on every marker in `body`.
///
/// Returns `true` when any marker changed. A `false` return leaves `body`
/// untouched, byte for byte.
pub fn force_1h_ttl(body: &mut Value) -> bool {
    let mut changed = false;

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(marker) = tool.get_mut("cache_control") {
                changed |= pin_marker(marker);
            }
        }
    }

    if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
        for block in blocks {
            if let Some(marker) = block.get_mut("cache_control") {
                changed |= pin_marker(marker);
            }
        }
    }

    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            // A message-level marker is legal alongside block-level ones.
            if let Some(marker) = message.get_mut("cache_control") {
                changed |= pin_marker(marker);
            }
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    if let Some(marker) = block.get_mut("cache_control") {
                        changed |= pin_marker(marker);
                    }
                }
            }
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pins_markers_in_all_three_positions() {
        let mut body = json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral"}}],
            "system": [
                {"type": "text", "text": "s"},
                {"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}]
            }]
        });
        assert!(force_1h_ttl(&mut body));
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
        // The block without a marker must not grow one — placement is the
        // client's call, we only change the duration.
        assert!(body["system"][0].get("cache_control").is_none());
    }

    /// Claude Code already sends `1h` on the main conversation. That has to be a
    /// byte-identical no-op or B1 would bust the very cache it protects.
    #[test]
    fn already_1h_is_a_no_op() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]
        });
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// An explicit 5m is the client asking for the default; B1's whole job is to
    /// override it.
    #[test]
    fn explicit_5m_is_upgraded() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]
        });
        assert!(force_1h_ttl(&mut body));
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    }

    /// A body with no markers at all is untouched — B1 never creates a
    /// breakpoint, because creating one costs a full cache write.
    #[test]
    fn no_markers_is_a_no_op() {
        let mut body = json!({
            "system": [{"type": "text", "text": "s"}],
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "a"}]
        });
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// An unrecognised marker shape is left alone rather than guessed at.
    #[test]
    fn non_ephemeral_marker_is_left_alone() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "persistent"}}]
        });
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// String content (rather than a block array) has nowhere to carry a marker.
    #[test]
    fn string_content_messages_are_skipped() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let before = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, before);
    }

    /// Running twice must not change anything the second time.
    #[test]
    fn is_idempotent() {
        let mut body = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral"}}]
        });
        assert!(force_1h_ttl(&mut body));
        let once = body.clone();
        assert!(!force_1h_ttl(&mut body));
        assert_eq!(body, once);
    }
}
