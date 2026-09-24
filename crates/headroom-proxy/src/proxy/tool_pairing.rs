//! Tool-use/tool-result pairing audits on the forwarded request.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Every `tool_use` block Anthropic will find unanswered in `value`.
///
/// The rule the API enforces: a `tool_use` needs a `tool_result` carrying its
/// id in the very next message. A turn that breaks it is refused whole, with
/// a 400 that names one id and no indication of who dropped it.
pub(super) fn unanswered_tool_uses(value: &serde_json::Value) -> Vec<(usize, String)> {
    let Some(messages) = value.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        let answered: std::collections::HashSet<&str> = messages
            .get(index + 1)
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .map(|next| {
                next.iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    .filter_map(|b| b.get("tool_use_id").and_then(|i| i.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            if !answered.contains(id) {
                out.push((index, id.to_string()));
            }
        }
    }
    out
}

/// Say, before the 400 arrives, whether the request left here unpaired — and
/// whether it arrived that way.
///
/// Written after 2026-09-03, when a turn was refused for an unanswered
/// `tool_use` and nothing in the logs could settle whether the client had
/// sent it broken or the proxy had broken it. The two bodies are already
/// parsed here, so the answer costs a walk of the messages array.
pub(super) fn audit_tool_pairing(
    request_id: &str,
    before: &serde_json::Value,
    after: &serde_json::Value,
) {
    let forwarded = unanswered_tool_uses(after);
    if forwarded.is_empty() {
        return;
    }
    let arrived = unanswered_tool_uses(before);
    let origin = if arrived.is_empty() {
        "proxy"
    } else {
        "client"
    };
    tracing::warn!(
        target: "headroom.proxy",
        event = "unanswered_tool_use_forwarded",
        request_id = %request_id,
        origin = origin,
        unanswered_count = forwarded.len(),
        unanswered = %forwarded
            .iter()
            .map(|(i, id)| format!("{i}:{id}"))
            .collect::<Vec<_>>()
            .join(","),
        arrived_unanswered_count = arrived.len(),
        "a tool_use is going upstream without its tool_result; the request will be refused"
    );
}

/// One `tool_use` block with no matching `tool_result` in the next message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PairingOffense {
    pub(super) message_index: usize,
    pub(super) tool_use_id: String,
}

/// Every `tool_use` in `body` that the upstream would refuse: Anthropic
/// requires each one (except in the final message, where a turn may legally
/// end on a tool call whose result arrives next turn) to have a
/// `tool_result` carrying its id in the immediately following message.
/// Blocks without a string id are skipped — a missing id is a different
/// malformation with its own error, and flagging it here would misname it.
pub(super) fn outbound_tool_pairing_offenses(body: &serde_json::Value) -> Vec<PairingOffense> {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut offenses = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if index + 1 >= messages.len() {
            break;
        }
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = block
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let answered = messages[index + 1]
                .get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|results| {
                    results.iter().any(|r| {
                        r.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                            && r.get("tool_use_id").and_then(|v| v.as_str()) == Some(id)
                    })
                });
            if !answered {
                offenses.push(PairingOffense {
                    message_index: index,
                    tool_use_id: id.to_string(),
                });
            }
        }
    }
    offenses
}

/// Boundary between "the client sent it broken" and "a pipeline stage broke
/// it", logged at the wire-footprint site on the exact bytes about to leave.
/// Runs only on Anthropic-shaped bodies: other wire formats pair calls
/// differently, and applying this rule to them would false-positive.
///
/// Log-only in both cases. An unanswerable `tool_use` cannot be repaired —
/// its result does not exist anywhere — so blocking would only swap whose
/// 400 the client sees. What this buys is attribution the `upstream_rejected`
/// line cannot give: whether to look at the client's transcript or at the
/// stages between it and the wire.
pub(super) fn check_outbound_tool_pairing(
    request_id: &str,
    session_key: &str,
    client_body: &[u8],
    wire_body: &[u8],
) {
    // Cheap gate first: no `tool_use` substring anywhere means no offense is
    // possible, and most turns carry none. Over-approximate on purpose
    // (`tool_use_id` contains it too) — the parse below decides.
    const MARKER: &[u8] = b"tool_use";
    if !wire_body
        .windows(MARKER.len())
        .any(|window| window == MARKER)
    {
        return;
    }
    let Ok(wire) = serde_json::from_slice::<serde_json::Value>(wire_body) else {
        return;
    };
    let offenses = outbound_tool_pairing_offenses(&wire);
    if offenses.is_empty() {
        return;
    }
    // Attribute: an offense already present in the client's own bytes was
    // sent broken (interrupted flow, compaction seam, concurrent writers to
    // one transcript). Anything else unpaired at the wire but paired on
    // arrival, a pipeline stage unpaired.
    let client_broken: std::collections::HashSet<String> =
        serde_json::from_slice::<serde_json::Value>(client_body)
            .map(|client| outbound_tool_pairing_offenses(&client))
            .unwrap_or_default()
            .into_iter()
            .map(|offense| offense.tool_use_id)
            .collect();
    for offense in offenses.iter().take(10) {
        tracing::warn!(
            target: "headroom.proxy",
            event = "outbound_tool_pairing_broken",
            request_id = %request_id,
            session_key_hash = %cache_stabilization::drift_detector::session_key_log_prefix(session_key),
            message_index = offense.message_index,
            tool_use_id = %offense.tool_use_id,
            offenses_total = offenses.len(),
            origin = if client_broken.contains(&offense.tool_use_id) {
                "client"
            } else {
                "proxy"
            },
            "assistant tool_use has no matching tool_result in the next message; \
             forwarding anyway — the upstream will refuse this turn"
        );
    }
}

#[cfg(test)]
mod outbound_tool_pairing_tests {
    use super::*;
    use serde_json::Value;
    use serde_json::json;

    fn tool_use(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "bash", "input": {}})
    }

    fn tool_result(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": "ok"})
    }

    fn user_msg(blocks: Vec<Value>) -> Value {
        json!({"role": "user", "content": blocks})
    }

    fn assistant_msg(blocks: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": blocks})
    }

    fn body(messages: Vec<Value>) -> Value {
        json!({"model": "m", "messages": messages})
    }

    #[test]
    fn paired_turn_has_no_offenses() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
            user_msg(vec![tool_result("call_1")]),
            assistant_msg(vec![json!({"type": "text", "text": "done"})]),
        ]);
        assert!(outbound_tool_pairing_offenses(&b).is_empty());
    }

    /// The incident shape: a `tool_use` whose next message carries no result
    /// for it is exactly what the upstream refuses.
    #[test]
    fn orphan_tool_use_is_named_with_index_and_id() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_9")]),
            user_msg(vec![json!({"type": "text", "text": "meanwhile"})]),
            assistant_msg(vec![json!({"type": "text", "text": "done"})]),
        ]);
        assert_eq!(
            outbound_tool_pairing_offenses(&b),
            vec![PairingOffense {
                message_index: 1,
                tool_use_id: "call_9".to_string(),
            }]
        );
    }

    /// A turn may legally end on a tool call — the result arrives next turn.
    #[test]
    fn trailing_tool_use_is_legal() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
        ]);
        assert!(outbound_tool_pairing_offenses(&b).is_empty());
    }

    #[test]
    fn wrong_id_result_is_still_an_orphan() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
            user_msg(vec![tool_result("call_other")]),
        ]);
        assert_eq!(outbound_tool_pairing_offenses(&b).len(), 1);
    }

    /// A result two messages down does not satisfy the next-message rule.
    #[test]
    fn non_immediate_result_is_still_an_orphan() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![tool_use("call_1")]),
            user_msg(vec![json!({"type": "text", "text": "chatter"})]),
            user_msg(vec![tool_result("call_1")]),
        ]);
        assert_eq!(outbound_tool_pairing_offenses(&b).len(), 1);
    }

    #[test]
    fn blocks_without_ids_and_string_content_are_skipped() {
        let b = body(vec![
            user_msg(vec![json!({"type": "text", "text": "run it"})]),
            assistant_msg(vec![json!({"type": "tool_use", "name": "bash"})]),
            user_msg(vec![json!({"type": "text", "text": "plain string"})]),
        ]);
        assert!(outbound_tool_pairing_offenses(&b).is_empty());
    }

    #[test]
    fn multiple_orphans_are_all_named() {
        let b = body(vec![
            assistant_msg(vec![tool_use("call_1"), tool_use("call_2")]),
            user_msg(vec![tool_result("call_1")]),
            assistant_msg(vec![json!({"type": "text", "text": "done"})]),
        ]);
        assert_eq!(
            outbound_tool_pairing_offenses(&b),
            vec![PairingOffense {
                message_index: 0,
                tool_use_id: "call_2".to_string(),
            }]
        );
    }

    #[test]
    fn body_without_messages_has_no_offenses() {
        assert!(outbound_tool_pairing_offenses(&json!({"model": "m"})).is_empty());
        assert!(outbound_tool_pairing_offenses(&json!({})).is_empty());
    }
}
