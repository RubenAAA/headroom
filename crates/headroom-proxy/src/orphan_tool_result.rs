//! Orphan client `tool_result` repair (Anthropic path).
//!
//! Anthropic validates every history `tool_result` against a preceding
//! `tool_use` with the same id — independently of the tools array, so
//! neither the tool-search repair (server result blocks) nor the CCR
//! sibling (which stands down when `headroom_retrieve` is declared)
//! covers it. A transcript that kept a result but lost its call (client
//! rebuild, fork, compaction) 400s the same way an orphaned
//! `tool_search_tool_result` does, and wedges the session the same way:
//! the client replays the transcript every turn.
//!
//! Runs after the other two repairs: they only ever remove calls, which
//! can only strand more results. Scope is the plain client pair
//! (`tool_use` / `tool_result`); server-side pairs belong to
//! [`crate::tool_search_deferral`]. A `tool_use` without a result is left
//! alone — pending calls in the final assistant message are valid.
//!
//! Neutralize in place, preserving the result's text (the model already
//! saw it) with a short placeholder fallback. Indexes, messages, and
//! roles never move.

use serde_json::Value;

/// Placeholder when an orphaned result carries no salvageable text.
const ORPHAN_RESULT_PLACEHOLDER_TEXT: &str = "[tool result omitted: no matching call]";

/// Outcome of [`strip_orphan_tool_results`].
pub struct OrphanRepairOutcome {
    /// Repaired messages, or the input moved back unchanged when nothing
    /// was neutralized. Callers must only re-serialize when
    /// `neutralized > 0`.
    pub messages: Vec<Value>,
    /// Number of blocks neutralized. Zero means unchanged.
    pub neutralized: usize,
}

/// Flatten a `tool_result` block's content to plain text, preserving what
/// the model already saw. Falls back to a short placeholder when there is
/// no textual content to keep.
fn result_as_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Value::Array(items)) => {
            let joined = items
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) != Some("text") {
                        return None;
                    }
                    let text = b.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.trim().is_empty() {
                ORPHAN_RESULT_PLACEHOLDER_TEXT.to_string()
            } else {
                joined
            }
        }
        _ => ORPHAN_RESULT_PLACEHOLDER_TEXT.to_string(),
    }
}

/// Neutralize `tool_result` blocks with no preceding matching `tool_use`.
///
/// No-op returning the input unchanged when every result pairs (the common
/// case) — the tools array is irrelevant here, pairing is validated
/// against history alone. A result with a missing id cannot pair by
/// construction and is neutralized like any other orphan.
pub fn strip_orphan_tool_results(messages: Vec<Value>) -> OrphanRepairOutcome {
    let unchanged = |messages: Vec<Value>| OrphanRepairOutcome {
        messages,
        neutralized: 0,
    };
    // Ids of client tool calls seen so far, in document order. A result
    // pairs only with a call before it.
    let mut seen_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut neutralized = 0usize;
    let mut changed = false;
    for mut message in messages {
        let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
            out.push(message);
            continue;
        };
        // Single pass in block order: a result pairs only with a call
        // before it, even within one message.
        for block in content.iter_mut() {
            let block_type = block.get("type").and_then(Value::as_str);
            if block_type == Some("tool_use") {
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        seen_call_ids.insert(id.to_string());
                    }
                }
                continue;
            }
            if block_type != Some("tool_result") {
                continue;
            }
            let paired = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty() && seen_call_ids.contains(id));
            if paired {
                continue;
            }
            let text = result_as_text(block);
            *block = Value::Object(
                [
                    ("type".to_string(), Value::String("text".to_string())),
                    ("text".to_string(), Value::String(text)),
                ]
                .into_iter()
                .collect(),
            );
            neutralized += 1;
            changed = true;
        }
        out.push(message);
    }
    if changed {
        OrphanRepairOutcome {
            messages: out,
            neutralized,
        }
    } else {
        unchanged(out)
    }
}

/// Neutralize `tool_use` blocks with no matching `tool_result` in the
/// next message.
///
/// Mirror image of [`strip_orphan_tool_results`]: Anthropic requires every
/// `tool_use` outside the final message to have its result immediately
/// after (`tool_use ids were found without tool_result blocks immediately
/// after`), and a transcript that kept parallel calls but lost their
/// results 400s the same way an orphaned result does — wedging the
/// session the same way, since the client replays the transcript.
///
/// Calls in the final message are pending work, not orphans: the turn is
/// still being assembled and must pass through untouched. Only the plain
/// client pair is in scope (`tool_use`); server-side calls stand alone
/// and never need results.
///
/// The call becomes short prose naming the tool rather than fabricated
/// output: inventing a result would put words in the tool's mouth that
/// the model then acts on, while admitting the gap keeps the history
/// readable. Indexes, messages, and roles never move.
pub fn strip_dangling_tool_calls(messages: Vec<Value>) -> OrphanRepairOutcome {
    let unchanged = |messages: Vec<Value>| OrphanRepairOutcome {
        messages,
        neutralized: 0,
    };
    // tool_result ids per message, so each message's calls check only the
    // message immediately after (the validator's "immediately after").
    let result_ids_per_message: Vec<std::collections::HashSet<String>> = messages
        .iter()
        .map(|message| {
            message
                .get("content")
                .and_then(|c| c.as_array())
                .map(|content| {
                    content
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                        .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
                        .filter(|id| !id.is_empty())
                        .map(|id| id.to_string())
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut neutralized = 0usize;
    let mut changed = false;
    let last = messages.len().saturating_sub(1);
    for (mi, mut message) in messages.into_iter().enumerate() {
        let is_final = mi == last;
        let Some(content) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
            out.push(message);
            continue;
        };
        // Calls pair only forward into the next message; same-message
        // results do not count (a result must arrive after its call).
        let next_results = if is_final {
            None
        } else {
            result_ids_per_message.get(mi + 1)
        };
        for block in content.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let paired = match (next_results, block.get("id").and_then(Value::as_str)) {
                (Some(next), Some(id)) if !id.is_empty() => next.contains(id),
                // Pending calls in the final message, and calls without
                // ids (a different validator's problem), pass through.
                _ => true,
            };
            if paired {
                continue;
            }
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or("?");
            let text = format!("[tool call to {name} omitted: no result arrived]");
            *block = Value::Object(
                [
                    ("type".to_string(), Value::String("text".to_string())),
                    ("text".to_string(), Value::String(text)),
                ]
                .into_iter()
                .collect(),
            );
            neutralized += 1;
            changed = true;
        }
        out.push(message);
    }
    if changed {
        OrphanRepairOutcome {
            messages: out,
            neutralized,
        }
    } else {
        unchanged(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": {}})
    }

    fn result(id: &str, text: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id,
               "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn paired_history_is_untouched() {
        let messages = vec![
            json!({"role": "assistant", "content": [call("t1", "Bash")]}),
            json!({"role": "user", "content": [result("t1", "ok")]}),
            json!({"role": "assistant", "content": [call("t2", "Read")]}),
            json!({"role": "user", "content": [result("t2", "file")]}),
        ];
        let before = messages.clone();
        let out = strip_orphan_tool_results(messages);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn same_message_pair_is_untouched() {
        let messages = vec![json!({
            "role": "assistant", "content": [call("t1", "Bash"), result("t1", "ok")]
        })];
        let before = messages.clone();
        let out = strip_orphan_tool_results(messages);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn orphan_result_is_neutralized_with_text_preserved() {
        // The call was compacted out of history; the tool stays declared,
        // so no other repair fires. Forwarding would 400.
        let messages = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "user", "content": [result("gone", "the file said yes")]}),
        ];
        let out = strip_orphan_tool_results(messages);
        assert_eq!(out.neutralized, 1);
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[1]["content"][0]["type"], json!("text"));
        assert_eq!(
            out.messages[1]["content"][0]["text"],
            json!("the file said yes")
        );
    }

    #[test]
    fn missing_id_is_neutralized() {
        let messages = vec![json!({
            "role": "user", "content": [{"type": "tool_result", "content": "x"}]
        })];
        let out = strip_orphan_tool_results(messages);
        assert_eq!(out.neutralized, 1);
        assert_eq!(out.messages[0]["content"][0]["text"], json!("x"));
    }

    #[test]
    fn result_before_call_is_neutralized() {
        // Pairing is order-sensitive like the upstream validator.
        let messages = vec![
            json!({"role": "user", "content": [result("t1", "early")]}),
            json!({"role": "assistant", "content": [call("t1", "Bash")]}),
        ];
        let out = strip_orphan_tool_results(messages);
        assert_eq!(out.neutralized, 1);
        assert_eq!(out.messages[1]["content"][0]["name"], json!("Bash"));
    }

    #[test]
    fn server_blocks_are_ignored() {
        // Other repairs own server-side pairs; this sweep must not touch
        // them (nor count them as calls).
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "server_tool_use", "id": "s1", "name": "web_search"},
                {"type": "tool_search_tool_result", "tool_use_id": "s1", "content": []},
            ],
        })];
        let before = messages.clone();
        let out = strip_orphan_tool_results(messages);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    // ── strip_dangling_tool_calls ──

    #[test]
    fn parallel_calls_without_results_are_neutralized() {
        // Live 400 (2026-09-22, sonnet-5): messages.2 carried two parallel
        // tool_use blocks with no tool_result in messages.3. Each call
        // becomes prose naming the tool — never fabricated output.
        // Pairing is strictly next-message, like the validator: a result
        // in the same message as its call does not count.
        let messages = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": [call("t0", "Bash")]}),
            json!({"role": "user", "content": [result("t0", "ok")]}),
            json!({"role": "assistant", "content": [call("t1", "Read"), call("t2", "Grep")]}),
            json!({"role": "user", "content": "and then?"}),
        ];
        let out = strip_dangling_tool_calls(messages);
        assert_eq!(out.neutralized, 2);
        let content = out.messages[3]["content"].as_array().unwrap();
        assert_eq!(
            content[0]["text"],
            json!("[tool call to Read omitted: no result arrived]")
        );
        assert_eq!(
            content[1]["text"],
            json!("[tool call to Grep omitted: no result arrived]")
        );
        // Paired history and the following message untouched.
        assert_eq!(out.messages[1]["content"][0]["name"], json!("Bash"));
        assert_eq!(out.messages[4]["content"], json!("and then?"));
    }

    #[test]
    fn final_message_calls_are_pending_not_dangling() {
        // Calls in the last message are still being assembled (prefill /
        // continuation): touching them would break live turns.
        let messages = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": [call("t1", "Bash"), call("t2", "Read")]}),
        ];
        let before = messages.clone();
        let out = strip_dangling_tool_calls(messages);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn paired_calls_are_untouched() {
        let messages = vec![
            json!({"role": "assistant", "content": [call("t1", "Bash")]}),
            json!({"role": "user", "content": [result("t1", "ok")]}),
        ];
        let before = messages.clone();
        let out = strip_dangling_tool_calls(messages);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }

    #[test]
    fn dangling_sweep_ignores_server_calls() {
        // Server calls stand alone and never need results.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"type": "server_tool_use", "id": "s1", "name": "web_search"}],
            }),
            json!({"role": "user", "content": "next"}),
        ];
        let before = messages.clone();
        let out = strip_dangling_tool_calls(messages);
        assert_eq!(out.neutralized, 0);
        assert_eq!(out.messages, before);
    }
}
