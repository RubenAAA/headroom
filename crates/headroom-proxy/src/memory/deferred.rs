//! Memory answers held back for the next request.
//!
//! When the model asks for a memory tool in the same turn as a client tool,
//! the proxy cannot finish the exchange on the spot. A continuation would have
//! to send that assistant turn upstream, and the client's `tool_use` in it has
//! no `tool_result` yet — the client has not run it. Anthropic rejects the
//! whole request with a 400, and the memory answer is lost.
//!
//! Waiting costs nothing. The client's next request carries its own
//! `tool_result`, so the turn can be completed then: the suppressed `tool_use`
//! goes back into the assistant message and its answer joins the results in the
//! message after it. One request, one cache write, and prefix replay carries
//! the repaired history forward from there.

use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How long an unclaimed answer is worth keeping. A conversation that never
/// comes back should not pin memory forever.
const TTL: Duration = Duration::from_secs(600);

/// Cap on held answers. Reached only if requests stop coming back, in which
/// case the oldest are the least likely to be claimed.
const MAX_HELD: usize = 32;

/// A memory call that ran, whose answer the model has not seen.
#[derive(Debug, Clone)]
pub struct PendingMemoryResult {
    /// The suppressed `tool_use` block, verbatim, to put back.
    pub tool_use: Value,
    /// The `tool_result` block answering it.
    pub tool_result: Value,
    /// Ids of the client tool calls that shared the turn. The next request
    /// carries them, which is how the assistant message gets found again
    /// without threading a session key through the response path.
    pub sibling_ids: Vec<String>,
    stored: Instant,
}

impl PendingMemoryResult {
    pub fn new(tool_use: Value, tool_result: Value, sibling_ids: Vec<String>) -> Self {
        Self {
            tool_use,
            tool_result,
            sibling_ids,
            stored: Instant::now(),
        }
    }
}

/// Answers waiting for the request that can carry them.
#[derive(Debug, Default)]
pub struct DeferredMemory {
    held: Vec<PendingMemoryResult>,
}

impl DeferredMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether an answer for this `tool_use` id is waiting for a later request.
    ///
    /// The stream splice asks before calling a suppressed block a lost tool
    /// call: a deferred answer is held on purpose, and the client is meant not
    /// to see the `tool_use`.
    pub fn is_held(&self, tool_use_id: &str) -> bool {
        self.held.iter().any(|p| {
            p.tool_use.get("id").and_then(Value::as_str) == Some(tool_use_id)
        })
    }

    pub fn hold(&mut self, pending: PendingMemoryResult) {
        self.expire();
        if self.held.len() >= MAX_HELD {
            self.held.remove(0);
        }
        self.held.push(pending);
    }

    fn expire(&mut self) {
        self.held.retain(|p| p.stored.elapsed() < TTL);
    }

    /// Put held answers back into `messages`, and return how many landed.
    ///
    /// Nothing is forced: an answer whose turn is not in this request stays
    /// held. An answer that cannot be placed without leaving a `tool_use`
    /// unanswered is dropped rather than sent — a malformed history costs the
    /// whole request, a missing memory answer costs one retrieval.
    pub fn apply(&mut self, messages: &mut [Value]) -> usize {
        self.expire();
        if self.held.is_empty() {
            return 0;
        }
        let mut applied = 0;
        self.held.retain(|pending| {
            match place(messages, pending) {
                Placement::Done => {
                    applied += 1;
                    false // claimed
                }
                // The turn has not come back yet; keep waiting.
                Placement::TurnNotHere => true,
                // The turn is here but unusable — drop rather than corrupt it.
                Placement::Unusable => false,
            }
        });
        applied
    }
}

enum Placement {
    Done,
    TurnNotHere,
    Unusable,
}

/// Restore one held answer into the assistant turn it belongs to.
fn place(messages: &mut [Value], pending: &PendingMemoryResult) -> Placement {
    let Some(idx) = assistant_index(messages, &pending.sibling_ids) else {
        return Placement::TurnNotHere;
    };
    let Some(tool_use_id) = pending.tool_use.get("id").and_then(Value::as_str) else {
        return Placement::Unusable;
    };
    // Already restored on an earlier pass, or replayed from forwarded history.
    if content_has_tool_id(&messages[idx], tool_use_id, "tool_use") {
        return Placement::Unusable;
    }
    // The results message has to exist, or putting the `tool_use` back would
    // leave it unanswered — the exact 400 this whole path exists to avoid.
    if idx + 1 >= messages.len() || !answers_tool_calls(&messages[idx + 1]) {
        return Placement::TurnNotHere;
    }

    let Some(content) = messages[idx]
        .get_mut("content")
        .and_then(|c| c.as_array_mut())
    else {
        return Placement::Unusable;
    };
    content.push(pending.tool_use.clone());

    let Some(results) = messages[idx + 1]
        .get_mut("content")
        .and_then(|c| c.as_array_mut())
    else {
        return Placement::Unusable;
    };
    // After the client's results, matching the order of the calls above.
    results.push(pending.tool_result.clone());
    Placement::Done
}

/// Index of the assistant message holding any of `ids`.
fn assistant_index(messages: &[Value], ids: &[String]) -> Option<usize> {
    messages.iter().position(|m| {
        m.get("role").and_then(Value::as_str) == Some("assistant")
            && ids.iter().any(|id| content_has_tool_id(m, id, "tool_use"))
    })
}

fn content_has_tool_id(message: &Value, id: &str, block_type: &str) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(Value::as_str) == Some(block_type)
                    && b.get("id").and_then(Value::as_str) == Some(id)
            })
        })
}

/// Whether a message carries `tool_result` blocks — the shape that answers an
/// assistant turn's tool calls.
fn answers_tool_calls(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
}

/// Process-wide holding area.
///
/// Entries carry the tool-call ids that identify their turn, so one store
/// serves every session without threading a session key through the response
/// path. Locked only for the length of a push or a scan — never across an
/// await.
pub fn store() -> &'static std::sync::Mutex<DeferredMemory> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<DeferredMemory>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(DeferredMemory::new()))
}

/// Split an Anthropic turn's tool calls into the proxy's and the client's.
///
/// The client's calls are what make a continuation impossible: they have no
/// result until the client runs them and sends the next request.
pub fn split_tool_calls(response: &Value) -> (Vec<Value>, Vec<String>) {
    let mut ours = Vec::new();
    let mut theirs = Vec::new();
    let Some(blocks) = response.get("content").and_then(Value::as_array) else {
        return (ours, theirs);
    };
    for b in blocks {
        if b.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let name = b.get("name").and_then(Value::as_str).unwrap_or("");
        if super::tool_adapter::MEMORY_TOOL_NAMES.contains(&name)
            || name == super::tool_adapter::NATIVE_MEMORY_TOOL_NAME
        {
            ours.push(b.clone());
        } else if let Some(id) = b.get("id").and_then(Value::as_str) {
            theirs.push(id.to_string());
        }
    }
    (ours, theirs)
}

/// Build the `tool_result` block that answers `tool_use_id`.
pub fn tool_result_block(tool_use_id: &str, content: Value) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str, name: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": {}})
    }

    /// The turn as the client sends it back: its own call, its own result.
    fn client_turn() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [tool_use("tu_bash", "Bash")]}),
            json!({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "tu_bash", "content": "ok"
            }]}),
        ]
    }

    fn pending() -> PendingMemoryResult {
        PendingMemoryResult::new(
            tool_use("tu_mem", "memory_search"),
            tool_result_block("tu_mem", json!("two hits")),
            vec!["tu_bash".to_string()],
        )
    }

    #[test]
    fn the_answer_rejoins_the_turn_it_belongs_to() {
        let mut d = DeferredMemory::new();
        d.hold(pending());
        let mut msgs = client_turn();
        assert_eq!(d.apply(&mut msgs), 1);

        let calls = msgs[1]["content"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "the memory call is back in the turn");
        assert_eq!(calls[1]["id"], "tu_mem");

        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2, "and its answer joins the results");
        assert_eq!(results[1]["tool_use_id"], "tu_mem");
        assert!(d.is_empty(), "a claimed answer is not held twice");
    }

    /// Every `tool_use` in the repaired turn must have a `tool_result`, or
    /// upstream rejects the request outright.
    #[test]
    fn every_call_in_the_repaired_turn_is_answered() {
        let mut d = DeferredMemory::new();
        d.hold(pending());
        let mut msgs = client_turn();
        d.apply(&mut msgs);

        let ids: Vec<_> = msgs[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect();
        let answered: Vec<_> = msgs[2]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| b["tool_use_id"].as_str().unwrap().to_string())
            .collect();
        for id in ids {
            assert!(answered.contains(&id), "{id} left unanswered");
        }
    }

    #[test]
    fn an_answer_waits_until_its_turn_comes_back() {
        let mut d = DeferredMemory::new();
        d.hold(pending());
        let mut unrelated =
            vec![json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})];
        assert_eq!(d.apply(&mut unrelated), 0);
        assert!(!d.is_empty(), "still waiting");

        let mut msgs = client_turn();
        assert_eq!(d.apply(&mut msgs), 1);
    }

    /// Prefix replay re-sends the repaired history, so a second pass must not
    /// add the same blocks again.
    #[test]
    fn a_turn_already_repaired_is_left_alone() {
        let mut d = DeferredMemory::new();
        d.hold(pending());
        let mut msgs = client_turn();
        d.apply(&mut msgs);
        let before = msgs.clone();

        d.hold(pending());
        d.apply(&mut msgs);
        assert_eq!(msgs, before, "no duplicate blocks");
    }

    /// The client's results have not arrived, so putting the call back would
    /// leave it unanswered. Wait instead.
    #[test]
    fn a_turn_without_results_is_not_touched() {
        let mut d = DeferredMemory::new();
        d.hold(pending());
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [tool_use("tu_bash", "Bash")]}),
        ];
        assert_eq!(d.apply(&mut msgs), 0);
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 1);
        assert!(!d.is_empty());
    }

    #[test]
    fn held_answers_are_bounded() {
        let mut d = DeferredMemory::new();
        for _ in 0..MAX_HELD + 10 {
            d.hold(pending());
        }
        assert_eq!(d.held.len(), MAX_HELD);
    }
}
