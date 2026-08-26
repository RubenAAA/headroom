//! `cursor-agent --output-format stream-json` → Anthropic Messages SSE.
//!
//! The vocabulary was read off a live agent rather than a spec, on 2026-08-26:
//! `system/init`, `user`, `thinking/{delta,completed}`, `assistant`,
//! `tool_call/{started,completed}`, `result/{success,error}`. Fixtures for each
//! live under `tests/fixtures/cursor/`.
//!
//! Two things about the source shape drive the design here.
//!
//! It is not delta-first. `assistant` carries a whole finished sentence, not a
//! token; only `thinking` arrives in pieces. So a text block opens and closes
//! around each `assistant` event instead of streaming into one long block, and
//! a turn that speaks twice around a tool call produces two text blocks — which
//! is what Anthropic's own wire format does anyway.
//!
//! And `tool_call` events are *reports*, not requests. Cursor's agent has
//! already run the tool by the time the event is written; `completed` carries
//! the result. Forwarding one as an Anthropic `tool_use` would ask the caller
//! to run something that has already run. They are surfaced as thinking text so
//! the work is visible, and the only blocks that become real `tool_use` are the
//! ones the bridge parks (see `super::park`).

use serde_json::{json, Value};

use crate::sse::outbound;

/// The Anthropic-side block currently open, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    None,
    Text,
    Thinking,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// Cursor reports the same four numbers Anthropic does, under its own
    /// names. Measured against a real turn: `inputTokens` 19,707 /
    /// `cacheReadTokens` 9,728 lines up with what the Anthropic path bills.
    fn from_cursor(usage: &Value) -> Self {
        let get = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input_tokens: get("inputTokens"),
            output_tokens: get("outputTokens"),
            cache_read_input_tokens: get("cacheReadTokens"),
            cache_creation_input_tokens: get("cacheWriteTokens"),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_input_tokens": self.cache_read_input_tokens,
            "cache_creation_input_tokens": self.cache_creation_input_tokens,
        })
    }
}

/// How the turn ended, once `result` has been seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The agent finished speaking.
    EndTurn,
    /// The agent failed. Carries whatever it said about it.
    Error(String),
}

#[derive(Debug)]
pub(crate) struct Translator {
    model: String,
    message_id: String,
    /// Cursor's own id for the conversation, from `system/init`. This is what
    /// `--resume` takes, so it is the thing worth keeping.
    pub(crate) session_id: Option<String>,
    started: bool,
    open: OpenBlock,
    block_index: usize,
    pub(crate) usage: Usage,
    pub(crate) outcome: Option<Outcome>,
    /// Whether tool-call reporting is surfaced to the caller as thinking.
    surface_tool_calls: bool,
}

impl Translator {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            // Anthropic ids are `msg_` + opaque. Callers key nothing off this,
            // but Claude Code logs it, so it should at least look right.
            message_id: format!("msg_cursor_{:016x}", rand_u64()),
            session_id: None,
            started: false,
            open: OpenBlock::None,
            block_index: 0,
            usage: Usage::default(),
            outcome: None,
            surface_tool_calls: true,
        }
    }

    #[cfg(test)]
    fn with_fixed_id(mut self, id: &str) -> Self {
        self.message_id = id.to_string();
        self
    }

    /// Feed one line of `stream-json`. Returns the SSE frames it produces.
    ///
    /// A line that does not parse, or that carries a `type` we have no mapping
    /// for, yields nothing. Cursor adds event kinds between releases and a new
    /// one is not a reason to fail a turn that is otherwise fine.
    pub(crate) fn push_line(&mut self, line: &str) -> Vec<String> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(line) {
            Ok(event) => self.push_event(&event),
            Err(_) => Vec::new(),
        }
    }

    pub(crate) fn push_event(&mut self, event: &Value) -> Vec<String> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = event.get("subtype").and_then(Value::as_str);
        let mut out = Vec::new();

        match (kind, subtype) {
            ("system", Some("init")) => {
                if let Some(id) = event.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_string());
                }
                out.extend(self.ensure_started());
            }
            ("thinking", Some("delta")) => {
                let text = event.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    return out;
                }
                out.extend(self.ensure_started());
                out.extend(self.open_block(OpenBlock::Thinking));
                out.push(self.delta_frame("thinking_delta", "thinking", text));
            }
            ("thinking", Some("completed")) => {
                out.extend(self.close_block());
            }
            ("assistant", _) => {
                let text = assistant_text(event);
                if text.is_empty() {
                    return out;
                }
                out.extend(self.ensure_started());
                // Close whatever is open: an `assistant` event is a complete
                // utterance, so it gets a block of its own rather than being
                // appended to a thinking block that never said it was done.
                out.extend(self.close_block());
                out.extend(self.open_block(OpenBlock::Text));
                out.push(self.delta_frame("text_delta", "text", &text));
                out.extend(self.close_block());
            }
            ("tool_call", Some("started")) if self.surface_tool_calls => {
                if let Some(note) = describe_tool_call(event) {
                    out.extend(self.ensure_started());
                    out.extend(self.open_block(OpenBlock::Thinking));
                    out.push(self.delta_frame("thinking_delta", "thinking", &note));
                }
            }
            ("result", _) => {
                out.extend(self.ensure_started());
                out.extend(self.close_block());
                if let Some(usage) = event.get("usage") {
                    self.usage = Usage::from_cursor(usage);
                }
                let failed = event.get("is_error").and_then(Value::as_bool) == Some(true)
                    || subtype == Some("error");
                self.outcome = Some(if failed {
                    Outcome::Error(
                        event
                            .get("result")
                            .and_then(Value::as_str)
                            .unwrap_or("cursor-agent reported an error")
                            .to_string(),
                    )
                } else {
                    Outcome::EndTurn
                });
                out.push(self.message_delta_frame("end_turn"));
                out.push(outbound::message_stop());
            }
            _ => {}
        }
        out
    }

    /// Open, fill and close a `tool_use` block for a call the bridge parked.
    ///
    /// This is the one place a `tool_use` is minted. Cursor's own `tool_call`
    /// events never become one — see the module header.
    pub(crate) fn emit_parked_tool_use(&mut self, id: &str, name: &str, args: &Value) -> Vec<String> {
        let mut out = self.ensure_started();
        out.extend(self.close_block());
        out.push(outbound::content_block_start(
            self.block_index,
            json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        ));
        // Anthropic streams tool arguments as partial JSON. The whole object is
        // already in hand, so it goes out in one delta rather than in pieces.
        out.push(self.delta_frame("input_json_delta", "partial_json", &args.to_string()));
        out.push(outbound::content_block_stop(self.block_index));
        self.block_index += 1;
        out
    }

    /// End a response that is pausing for a tool rather than finishing.
    ///
    /// `stop_reason: "tool_use"` is what tells Claude Code to run the tool and
    /// come back. The agent process stays alive and blocked meanwhile, so no
    /// outcome is recorded here — the turn is not over.
    pub(crate) fn pause_for_tool(&mut self) -> Vec<String> {
        let mut out = self.close_block();
        out.push(self.message_delta_frame("tool_use"));
        out.push(outbound::message_stop());
        out
    }

    /// Frames for a turn whose process died without writing `result`.
    ///
    /// Without this the caller sees a stream that stops mid-block and waits for
    /// a `message_stop` that is never coming.
    pub(crate) fn finish_unterminated(&mut self) -> Vec<String> {
        if self.outcome.is_some() {
            return Vec::new();
        }
        let mut out = self.ensure_started();
        out.extend(self.close_block());
        self.outcome = Some(Outcome::Error("cursor-agent ended without a result".into()));
        out.push(self.message_delta_frame("end_turn"));
        out.push(outbound::message_stop());
        out
    }

    fn ensure_started(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![outbound::message_start(
            &self.message_id,
            &self.model,
            Usage::default().to_json(),
        )]
    }

    fn open_block(&mut self, want: OpenBlock) -> Vec<String> {
        if self.open == want {
            return Vec::new();
        }
        let mut out = self.close_block();
        self.open = want;
        let block = match want {
            OpenBlock::Text => json!({"type": "text", "text": ""}),
            OpenBlock::Thinking => json!({"type": "thinking", "thinking": "", "signature": ""}),
            OpenBlock::None => return out,
        };
        out.push(outbound::content_block_start(self.block_index, block));
        out
    }

    fn close_block(&mut self) -> Vec<String> {
        if self.open == OpenBlock::None {
            return Vec::new();
        }
        self.open = OpenBlock::None;
        let out = vec![outbound::content_block_stop(self.block_index)];
        self.block_index += 1;
        out
    }

    fn delta_frame(&self, delta_type: &str, field: &str, text: &str) -> String {
        outbound::content_block_delta(self.block_index, delta_type, field, text)
    }

    fn message_delta_frame(&self, stop_reason: &str) -> String {
        outbound::message_delta(stop_reason, self.usage.to_json())
    }
}

/// The text of an `assistant` event, joining any `text` blocks it carries.
fn assistant_text(event: &Value) -> String {
    let Some(content) = event.pointer("/message/content").and_then(Value::as_array) else {
        return String::new();
    };
    content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

/// A one-line note for a tool the agent ran itself.
///
/// The event names the tool by the *key* it hangs the payload off —
/// `readToolCall`, `shellToolCall`, `mcpToolCall` — rather than in a field, so
/// the key is the name. Anything ending `ToolCall` is one of these.
fn describe_tool_call(event: &Value) -> Option<String> {
    let obj = event.get("tool_call")?.as_object()?;
    let (key, payload) = obj.iter().find(|(k, _)| k.ends_with("ToolCall"))?;
    let name = key.trim_end_matches("ToolCall");
    // An MCP call names the real tool inside its args; the outer key is always
    // `mcpToolCall` and would tell the reader nothing.
    let name = if name == "mcp" {
        payload
            .pointer("/args/name")
            .and_then(Value::as_str)
            .unwrap_or("mcp")
            .to_string()
    } else {
        name.to_string()
    };
    Some(format!("[cursor ran {name}]\n"))
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Not cryptographic and does not need to be: this only has to be distinct
    // between concurrent turns in one process.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (std::process::id() as u64).rotate_left(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse SSE frames back into (event, data) so tests assert on meaning
    /// rather than on whitespace.
    fn parse(frames: &[String]) -> Vec<(String, Value)> {
        frames
            .iter()
            .map(|f| {
                let mut event = None;
                let mut data = None;
                for line in f.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        event = Some(rest.to_string());
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        data = Some(serde_json::from_str::<Value>(rest).expect("frame data"));
                    }
                }
                (event.expect("event line"), data.expect("data line"))
            })
            .collect()
    }

    fn run(lines: &[&str]) -> Vec<(String, Value)> {
        let mut t = Translator::new("cursor-grok-4.6-high").with_fixed_id("msg_test");
        let mut frames = Vec::new();
        for line in lines {
            frames.extend(t.push_line(line));
        }
        frames.extend(t.finish_unterminated());
        parse(&frames)
    }

    #[test]
    fn a_plain_answer_becomes_a_well_formed_anthropic_turn() {
        let events = run(&[
            r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"Cursor Grok 4.6 High"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","usage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":3,"cacheWriteTokens":1}}"#,
        ]);
        let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[2].1["delta"]["text"], "hello");
        assert_eq!(events[4].1["delta"]["stop_reason"], "end_turn");
    }

    /// The four token counts are the whole reason to read `result`, and they
    /// are the numbers the proxy bills and caches on. A rename upstream must
    /// fail here rather than silently zero the accounting.
    #[test]
    fn cursor_usage_maps_onto_the_anthropic_names() {
        let events = run(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"inputTokens":19707,"outputTokens":118,"cacheReadTokens":9728,"cacheWriteTokens":42}}"#,
        ]);
        let usage = &events.iter().find(|(e, _)| e == "message_delta").unwrap().1["usage"];
        assert_eq!(usage["input_tokens"], 19707);
        assert_eq!(usage["output_tokens"], 118);
        assert_eq!(usage["cache_read_input_tokens"], 9728);
        assert_eq!(usage["cache_creation_input_tokens"], 42);
    }

    #[test]
    fn thinking_deltas_accumulate_in_one_block_and_close_on_completed() {
        let events = run(&[
            r#"{"type":"thinking","subtype":"delta","text":"Reading "}"#,
            r#"{"type":"thinking","subtype":"delta","text":"the file"}"#,
            r#"{"type":"thinking","subtype":"completed"}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ]);
        let starts = events.iter().filter(|(e, _)| e == "content_block_start").count();
        assert_eq!(starts, 1, "both deltas belong to one thinking block");
        let deltas: Vec<&Value> = events
            .iter()
            .filter(|(e, _)| e == "content_block_delta")
            .map(|(_, d)| d)
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0]["delta"]["type"], "thinking_delta");
        assert_eq!(deltas[0]["delta"]["thinking"], "Reading ");
    }

    /// Block indices have to advance, and every `content_block_start` needs its
    /// `content_block_stop`. Claude Code assembles content by index; a repeated
    /// or skipped one silently overwrites or drops a block.
    #[test]
    fn every_opened_block_is_closed_and_indices_advance() {
        let events = run(&[
            r#"{"type":"thinking","subtype":"delta","text":"mm"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"one"}]}}"#,
            r#"{"type":"thinking","subtype":"delta","text":"more"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ]);
        let starts: Vec<u64> = events
            .iter()
            .filter(|(e, _)| e == "content_block_start")
            .map(|(_, d)| d["index"].as_u64().unwrap())
            .collect();
        let stops: Vec<u64> = events
            .iter()
            .filter(|(e, _)| e == "content_block_stop")
            .map(|(_, d)| d["index"].as_u64().unwrap())
            .collect();
        assert_eq!(starts, vec![0, 1, 2, 3]);
        assert_eq!(stops, starts, "each start is matched by a stop at its index");
    }

    #[test]
    fn the_session_id_is_kept_because_resume_needs_it() {
        let mut t = Translator::new("m");
        t.push_line(r#"{"type":"system","subtype":"init","session_id":"9b5c3b0a-2e0b"}"#);
        assert_eq!(t.session_id.as_deref(), Some("9b5c3b0a-2e0b"));
    }

    /// A dead subprocess must still produce a terminated stream, or the caller
    /// hangs waiting for `message_stop`.
    #[test]
    fn a_turn_that_never_reports_a_result_is_still_terminated() {
        let events = run(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"half a th"}]}}"#,
        ]);
        let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(names.last(), Some(&"message_stop"));
        assert!(names.contains(&"message_delta"));
    }

    #[test]
    fn a_failed_result_is_recorded_as_an_error_outcome() {
        let mut t = Translator::new("m");
        t.push_line(r#"{"type":"result","subtype":"error","is_error":true,"result":"model refused"}"#);
        assert_eq!(t.outcome, Some(Outcome::Error("model refused".into())));
    }

    /// Cursor's own tool calls have already run. Emitting them as `tool_use`
    /// would ask Claude Code to execute something that is finished, and would
    /// leave a `tool_use` with no matching `tool_result` in the transcript.
    #[test]
    fn a_tool_cursor_ran_itself_is_narrated_not_requested() {
        let events = run(&[
            r#"{"type":"tool_call","subtype":"started","tool_call":{"readToolCall":{"args":{"path":"/tmp/x"}}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ]);
        assert!(
            !events.iter().any(|(_, d)| d["content_block"]["type"] == "tool_use"),
            "a report must not become a request"
        );
        let note = events
            .iter()
            .find(|(e, _)| e == "content_block_delta")
            .expect("the call is surfaced");
        assert_eq!(note.1["delta"]["type"], "thinking_delta");
        assert!(note.1["delta"]["thinking"].as_str().unwrap().contains("read"));
    }

    /// An MCP call is named by its arguments, not by the `mcpToolCall` key.
    #[test]
    fn an_mcp_call_is_narrated_with_the_real_tool_name() {
        let events = run(&[
            r#"{"type":"tool_call","subtype":"started","tool_call":{"mcpToolCall":{"args":{"name":"headroom-Read","args":{}}}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ]);
        let note = events.iter().find(|(e, _)| e == "content_block_delta").unwrap();
        assert!(note.1["delta"]["thinking"].as_str().unwrap().contains("headroom-Read"));
    }

    /// Cursor adds event kinds between releases. An unknown one is not a reason
    /// to fail a turn that is otherwise fine.
    #[test]
    fn unknown_and_malformed_lines_are_ignored() {
        let mut t = Translator::new("m");
        assert!(t.push_line("not json at all").is_empty());
        assert!(t.push_line(r#"{"type":"somethingNew","subtype":"whatever"}"#).is_empty());
        assert!(t.push_line("").is_empty());
    }

    /// Replay of a turn recorded off the live agent on 2026-08-26 — the one in
    /// which Grok was steered onto the proxy's own MCP tools and answered from
    /// them. Synthetic lines prove each mapping; this proves the mappings are
    /// the ones a real transcript actually needs.
    #[test]
    fn a_recorded_turn_replays_into_a_coherent_stream() {
        let raw = include_str!("../../tests/fixtures/cursor/mcp-tool-turn.jsonl");
        let mut t = Translator::new("cursor-grok-4.6-high").with_fixed_id("msg_test");
        let mut frames = Vec::new();
        for line in raw.lines() {
            frames.extend(t.push_line(line));
        }
        assert!(
            t.finish_unterminated().is_empty(),
            "the recording ends in a result, so nothing should need salvaging"
        );
        let events = parse(&frames);

        let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(names.first(), Some(&"message_start"));
        assert_eq!(names.last(), Some(&"message_stop"));

        // Balanced blocks, in order, with no index reused.
        let mut depth = 0i32;
        let mut expect = 0u64;
        for (name, data) in &events {
            match name.as_str() {
                "content_block_start" => {
                    depth += 1;
                    assert_eq!(depth, 1, "blocks never nest");
                    assert_eq!(data["index"].as_u64().unwrap(), expect);
                }
                "content_block_stop" => {
                    depth -= 1;
                    assert_eq!(depth, 0, "a stop with nothing open");
                    assert_eq!(data["index"].as_u64().unwrap(), expect);
                    expect += 1;
                }
                "content_block_delta" => {
                    assert_eq!(depth, 1, "a delta outside any block");
                    assert_eq!(data["index"].as_u64().unwrap(), expect);
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "the stream ends with a block still open");

        // The answer survives the round trip. CRIMSON-42 is the marker the
        // MCP tool returned, so its presence also shows the tool result
        // reached the model and came back out.
        let said: String = events
            .iter()
            .filter(|(e, _)| e == "content_block_delta")
            .filter_map(|(_, d)| d["delta"]["text"].as_str())
            .collect();
        assert!(said.contains("CRIMSON-42"), "got: {said}");

        assert_eq!(t.session_id.as_deref(), Some("cf8812c0-d9cc-4a5c-90ab-fdb08209f2b0"));
        assert_eq!(t.outcome, Some(Outcome::EndTurn));
        assert_eq!(t.usage.input_tokens, 17882);
        assert_eq!(t.usage.cache_read_input_tokens, 27136);
    }
}
