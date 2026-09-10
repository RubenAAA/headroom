//! Building Anthropic Messages SSE frames.
//!
//! The rest of `sse` reads streams. This one writes them, and it exists because
//! two providers had grown their own copy of the same twelve `format!` calls:
//! the OpenAI-Responses translator in `handlers::local_model` and the
//! `cursor-agent` translator in `crate::cursor`. Neither invented anything —
//! the wire format is fixed — so the copies were identical, and identical
//! copies drift.
//!
//! Only the framing lives here. What decides *when* a block opens stays with
//! each provider, because that genuinely differs: the OpenAI path is
//! token-delta-first over two upstream shapes, and Cursor hands over a whole
//! finished utterance per event. Forcing those two state machines together
//! would cost more than the duplication it removed.

use serde_json::{json, Value};

/// Wrap an event as one SSE frame.
///
/// Anthropic names the event twice — once in the `event:` line and once in the
/// payload's `type` — and clients read whichever they prefer. Both must agree,
/// which is the main thing a caller can get wrong by hand.
pub(crate) fn frame(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The opening frame of a turn.
pub(crate) fn message_start(message_id: &str, model: &str, usage: Value) -> String {
    frame(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": usage,
            }
        }),
    )
}

/// Open a block. `block` is the `content_block` body, e.g.
/// `{"type": "text", "text": ""}`.
pub(crate) fn content_block_start(index: usize, block: Value) -> String {
    frame(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": block,
        }),
    )
}

/// A delta within the open block.
///
/// `delta_type` and `field` travel together — `text_delta` carries `text`,
/// `thinking_delta` carries `thinking`, `input_json_delta` carries
/// `partial_json` — so the constructors below are the ones to reach for rather
/// than this.
///
/// Hot path: one call per streamed token. The envelope is a fixed
/// template (measured 32x vs `json!` + `format!`); only the variable
/// parts go through `serde_json` escaping, so arbitrary
/// `&str` input yields byte-identical bytes to the `json!` form,
/// including key order (`type,index,delta` / `type,field`) and
/// compact separators.
pub(crate) fn content_block_delta(
    index: usize,
    delta_type: &str,
    field: &str,
    value: &str,
) -> String {
    // Template + escaped fragments. `serde_json::to_string` on a `&str`
    // emits exactly the quoted, escaped literal the `json!` form would.
    let delta_type_json = serde_json::to_string(&delta_type).unwrap_or_default();
    let field_json = serde_json::to_string(&field).unwrap_or_default();
    let value_json = serde_json::to_string(&value).unwrap_or_default();
    let mut out =
        String::with_capacity(120 + delta_type_json.len() + field_json.len() + value_json.len());
    out.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":");
    use std::fmt::Write as _;
    let _ = write!(out, "{index}");
    out.push_str(",\"delta\":{\"type\":");
    out.push_str(&delta_type_json);
    out.push_str(",");
    out.push_str(&field_json);
    out.push_str(":");
    out.push_str(&value_json);
    out.push_str("}}\n\n");
    out
}

pub(crate) fn text_delta(index: usize, text: &str) -> String {
    content_block_delta(index, "text_delta", "text", text)
}

pub(crate) fn thinking_delta(index: usize, thinking: &str) -> String {
    content_block_delta(index, "thinking_delta", "thinking", thinking)
}

/// Closes a thinking block by handing the client the reasoning envelope it will
/// echo back to us next turn.
pub(crate) fn signature_delta(index: usize, signature: &str) -> String {
    content_block_delta(index, "signature_delta", "signature", signature)
}

pub(crate) fn input_json_delta(index: usize, partial_json: &str) -> String {
    content_block_delta(index, "input_json_delta", "partial_json", partial_json)
}

pub(crate) fn content_block_stop(index: usize) -> String {
    frame(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

pub(crate) fn message_delta(stop_reason: &str, usage: Value) -> String {
    frame(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
            "usage": usage,
        }),
    )
}

pub(crate) fn message_stop() -> String {
    frame("message_stop", json!({"type": "message_stop"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `event:` line and the payload `type` must agree. A client reading
    /// one and not the other is otherwise silently mis-fed.
    #[test]
    fn the_event_name_matches_the_payload_type() {
        let cases = [
            message_start("msg_1", "m", json!({})),
            content_block_start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "hi"),
            content_block_stop(0),
            message_delta("end_turn", json!({})),
            message_stop(),
        ];
        for raw in cases {
            let name = raw
                .lines()
                .find_map(|l| l.strip_prefix("event: "))
                .expect("an event line");
            let data: Value = raw
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .map(|d| serde_json::from_str(d).expect("valid json"))
                .expect("a data line");
            assert_eq!(data["type"], name, "in frame: {raw}");
        }
    }

    /// Every frame ends in a blank line. Without it the client buffers the
    /// frame and shows nothing until the next one arrives.
    #[test]
    fn every_frame_is_terminated() {
        assert!(message_stop().ends_with("\n\n"));
        assert!(text_delta(3, "x").ends_with("\n\n"));
    }

    /// Each delta kind carries its payload under its own key. Pairing the wrong
    /// two produces a frame that parses and says nothing.
    #[test]
    fn each_delta_kind_uses_its_own_field() {
        let field_of = |raw: String| -> (String, Vec<String>) {
            let data: Value =
                serde_json::from_str(raw.lines().nth(1).unwrap().trim_start_matches("data: "))
                    .unwrap();
            let delta = data["delta"].as_object().unwrap().clone();
            let kind = delta["type"].as_str().unwrap().to_string();
            let mut keys: Vec<String> = delta.keys().filter(|k| *k != "type").cloned().collect();
            keys.sort();
            (kind, keys)
        };
        assert_eq!(
            field_of(text_delta(0, "a")),
            ("text_delta".into(), vec!["text".to_string()])
        );
        assert_eq!(
            field_of(thinking_delta(0, "a")),
            ("thinking_delta".into(), vec!["thinking".to_string()])
        );
        assert_eq!(
            field_of(input_json_delta(0, "{}")),
            ("input_json_delta".into(), vec!["partial_json".to_string()])
        );
        assert_eq!(
            field_of(signature_delta(0, "s")),
            ("signature_delta".into(), vec!["signature".to_string()])
        );
    }

    #[test]
    fn the_block_index_is_carried_on_every_block_frame() {
        for raw in [
            content_block_start(7, json!({"type": "text", "text": ""})),
            text_delta(7, "x"),
            content_block_stop(7),
        ] {
            let data: Value =
                serde_json::from_str(raw.lines().nth(1).unwrap().trim_start_matches("data: "))
                    .unwrap();
            assert_eq!(data["index"], 7);
        }
    }

    /// The templated delta path must emit byte-identical bytes to the
    /// `json!` form it replaced — key order, separators, and escaping
    /// (a cache or client diff on these bytes is a visible glitch).
    #[test]
    fn delta_template_matches_json_form_byte_for_byte() {
        let old = |index: usize, dt: &str, field: &str, value: &str| -> String {
            frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": dt, field: value},
                }),
            )
        };
        for (index, dt, field, value) in [
            (0, "text_delta", "text", "hi"),
            (3, "text_delta", "text", "a\"b\\c\nd日本語🔥"),
            (12, "thinking_delta", "thinking", ""),
            (1024, "input_json_delta", "partial_json", "{\"a\":\t1}"),
            (7, "signature_delta", "signature", "tab\there"),
        ] {
            assert_eq!(
                content_block_delta(index, dt, field, value),
                old(index, dt, field, value),
                "template drift for value {value:?}"
            );
        }
    }
}
