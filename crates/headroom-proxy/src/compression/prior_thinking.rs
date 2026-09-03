//! Drop `thinking` blocks from assistant turns the model has already acted on.
//!
//! Anthropic only reads the thinking of the LAST assistant message back (it
//! must stay in place while a tool loop is open, signature intact). Every
//! earlier one is dead weight the client re-sends on each turn. Probed
//! 2026-09-02 against `claude-haiku-4-5` with a two-step tool loop: removing
//! the first assistant message's thinking while the loop is still open is a
//! 200; removing every assistant message's thinking once a plain user text
//! closes the loop is a 200.
//!
//! Removing a block rewrites a message, and a rewritten message is a new
//! provider cache key. So this pass runs only where the prefix is being
//! rebuilt anyway — a rebuild boundary or a history arrival, the same
//! condition that lets `ctx_offload` convert frozen history — and the replay
//! store carries the stripped bytes forward on every steady turn after it.
//! The pass records nothing: it is a pure function of the originals, so a
//! rebuilt prefix comes out the same however many times it runs.

use serde_json::Value;

/// What one pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DropOutcome {
    pub blocks_removed: usize,
    pub bytes_removed: usize,
}

fn is_thinking(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking") | Some("redacted_thinking")
    )
}

/// Remove `thinking` and `redacted_thinking` blocks from every assistant
/// message before the last one. The last assistant message is never touched,
/// whatever follows it. A message that would be left with no blocks is left
/// alone too: an empty `content` is a request error.
pub fn drop_prior_thinking(parsed: &mut Value) -> DropOutcome {
    let mut outcome = DropOutcome::default();
    let Some(messages) = parsed.get_mut("messages").and_then(Value::as_array_mut) else {
        return outcome;
    };
    let Some(last_assistant) = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
    else {
        return outcome;
    };
    for message in messages.iter_mut().take(last_assistant) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let thinking = blocks.iter().filter(|b| is_thinking(b)).count();
        if thinking == 0 || thinking == blocks.len() {
            continue;
        }
        blocks.retain(|b| {
            if is_thinking(b) {
                outcome.blocks_removed += 1;
                outcome.bytes_removed += serde_json::to_vec(b).map(|v| v.len()).unwrap_or(0);
                false
            } else {
                true
            }
        });
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thought(i: usize) -> Value {
        json!({"type": "thinking", "thinking": format!("step {i}"), "signature": format!("sig{i}")})
    }

    fn loop_of(assistant_turns: usize) -> Value {
        let mut messages = vec![json!({"role": "user", "content": "what time is it"})];
        for i in 0..assistant_turns {
            messages.push(json!({"role": "assistant", "content": [
                thought(i),
                {"type": "tool_use", "id": format!("toolu_{i}"), "name": "get_time", "input": {}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": "12:00"}
            ]}));
        }
        json!({"messages": messages})
    }

    fn thinking_per_assistant(parsed: &Value) -> Vec<usize> {
        parsed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .map(|m| {
                m["content"]
                    .as_array()
                    .map_or(0, |c| c.iter().filter(|b| is_thinking(b)).count())
            })
            .collect()
    }

    #[test]
    fn every_assistant_but_the_last_loses_its_thinking() {
        let mut parsed = loop_of(3);
        let out = drop_prior_thinking(&mut parsed);
        assert_eq!(out.blocks_removed, 2);
        assert!(out.bytes_removed > 0);
        assert_eq!(thinking_per_assistant(&parsed), vec![0, 0, 1]);
    }

    #[test]
    fn the_last_assistant_keeps_its_thinking_even_after_a_closing_user_text() {
        let mut parsed = loop_of(2);
        parsed["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "user", "content": "thanks"}));
        drop_prior_thinking(&mut parsed);
        assert_eq!(thinking_per_assistant(&parsed), vec![0, 1]);
    }

    #[test]
    fn a_single_assistant_turn_is_untouched() {
        let mut parsed = loop_of(1);
        let before = parsed.clone();
        assert_eq!(drop_prior_thinking(&mut parsed), DropOutcome::default());
        assert_eq!(parsed, before);
    }

    #[test]
    fn redacted_thinking_counts_and_string_content_is_skipped() {
        let mut parsed = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "hello"}
            ]},
            {"role": "user", "content": "more"},
            {"role": "assistant", "content": "plain string reply"},
            {"role": "user", "content": "again"},
            {"role": "assistant", "content": [thought(9), {"type": "text", "text": "last"}]}
        ]});
        let out = drop_prior_thinking(&mut parsed);
        assert_eq!(out.blocks_removed, 1);
        assert_eq!(thinking_per_assistant(&parsed), vec![0, 0, 1]);
        assert_eq!(parsed["messages"][3]["content"], "plain string reply");
    }

    #[test]
    fn a_message_that_is_only_thinking_is_left_alone() {
        let mut parsed = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [thought(0)]},
            {"role": "user", "content": "go on"},
            {"role": "assistant", "content": [thought(1), {"type": "text", "text": "ok"}]}
        ]});
        assert_eq!(drop_prior_thinking(&mut parsed), DropOutcome::default());
        assert_eq!(thinking_per_assistant(&parsed), vec![1, 1]);
    }

    #[test]
    fn the_pass_is_idempotent() {
        let mut parsed = loop_of(4);
        drop_prior_thinking(&mut parsed);
        let once = parsed.clone();
        assert_eq!(drop_prior_thinking(&mut parsed), DropOutcome::default());
        assert_eq!(parsed, once);
    }
}
