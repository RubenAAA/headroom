//! Restore redaction placeholders in an SSE stream one event at a time.
//!
//! A byte scan survives a token cut by a chunk edge but not one cut by SSE
//! framing. A routed model streams a placeholder it echoes a few characters
//! per delta, so on the wire the token's halves sit in two delta events with
//! a frame's worth of JSON between them, and no byte scan can see it whole.
//! The Codex translator emits one `input_json_delta` per upstream argument
//! fragment, which is how a Skill call reached Claude Code still holding the
//! token as its skill name.
//!
//! So the stream is framed first, and each streamed text delta is read with
//! the stream it belongs to: an Anthropic content block, an OpenAI Responses
//! item, a Chat Completions choice or tool call, a Gemini candidate. A delta
//! whose text ends inside a token (or on a partial `__HR_`) is held whole.
//! When the next delta of the same stream arrives, the unfinished token moves
//! out of the held event into it, so the token travels in one event. Events
//! are never split or duplicated: each keeps its own metadata (a tool call's
//! `id` and `name`, a `finishReason`), and only text moves between them.
//! Keepalives pass a held delta; any other event flushes it first, so nothing
//! is reordered and nothing outlives its block.
//!
//! Each event is then restored inside its decoded JSON, not in its bytes: a
//! value holding `"` or `\` spliced raw into the wire would end its string
//! early. A text value goes back verbatim and the serializer escapes it; a
//! `partial_json` or `arguments` value is itself JSON text, so it is escaped
//! once more for that inner level. Events without a token pass through
//! byte-identical.

use serde_json::Value;

use crate::redact::{Escape, MAX_TOKEN_LEN, PREFIX, RestoreTable, hold_from, parse_token};
use crate::sse::framing::SseFramer;

/// One step from an event's JSON root towards its streamed text.
#[derive(Clone, Copy)]
enum Seg {
    Key(&'static str),
    Index(usize),
}

/// A streamed text delta, as received and as it will go out.
struct Delta {
    /// Which stream the text belongs to; only deltas of one stream join.
    stream: String,
    block: String,
    data: std::ops::Range<usize>,
    value: Value,
    path: Vec<Seg>,
    original: String,
    text: String,
}

impl Delta {
    fn parse(block: &[u8]) -> Option<Self> {
        let block = std::str::from_utf8(block).ok()?;
        let (data, payload) = single_data_line(block)?;
        let value: Value = serde_json::from_str(payload).ok()?;
        let (stream, path) = stream_leaf(&value)?;
        let text = leaf(&value, &path)?.as_str()?.to_string();
        Some(Self {
            stream,
            block: block.to_string(),
            data,
            value,
            path,
            original: text.clone(),
            text,
        })
    }

    /// The event as it goes out: byte-identical unless its text changed.
    fn render(mut self) -> Vec<u8> {
        if self.text == self.original {
            return self.block.into_bytes();
        }
        if let Some(slot) = leaf_mut(&mut self.value, &self.path) {
            *slot = Value::String(self.text);
        }
        let mut out = String::with_capacity(self.block.len());
        out.push_str(&self.block[..self.data.start]);
        out.push_str("data: ");
        out.push_str(&self.value.to_string());
        out.push_str(&self.block[self.data.end..]);
        out.into_bytes()
    }
}

/// What the body turned out to be, read from its first bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Unknown,
    Sse,
    Json,
    Other,
}

pub(crate) struct PlaceholderJoin {
    shape: Shape,
    /// Opening bytes too short to tell the shape by (`ev` of `event:`).
    undecided: Vec<u8>,
    framer: SseFramer,
    held: Option<Delta>,
}

impl Default for PlaceholderJoin {
    fn default() -> Self {
        Self {
            shape: Shape::Unknown,
            undecided: Vec::new(),
            framer: SseFramer::new(),
            held: None,
        }
    }
}

impl PlaceholderJoin {
    /// How the caller should restore what `push` hands back: `None` when this
    /// stage restored it already (SSE), otherwise the escape for a byte pass.
    pub(crate) fn byte_escape(&self) -> Option<Escape> {
        match self.shape {
            Shape::Sse => None,
            Shape::Json => Some(Escape::Json),
            Shape::Unknown | Shape::Other => Some(Escape::Raw),
        }
    }

    /// Feed a chunk. For SSE, returns whole restored events; for anything
    /// else, the chunk untouched. Opening bytes that could still be either
    /// come back with the chunk that decides.
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        table: &RestoreTable,
        misses: &mut usize,
    ) -> Vec<u8> {
        let decided;
        let chunk = if self.shape == Shape::Unknown {
            self.undecided.extend_from_slice(chunk);
            let start = self.undecided.trim_ascii_start();
            if start.is_empty() {
                return std::mem::take(&mut self.undecided);
            }
            let markers = [b"event:".as_slice(), b"data:", b":"];
            // A first read can stop inside the marker; deciding on `ev`
            // would send the whole stream past the join.
            if markers
                .iter()
                .any(|p| start.len() < p.len() && p.starts_with(start))
            {
                return Vec::new();
            }
            self.shape = if markers.iter().any(|p| start.starts_with(p)) {
                Shape::Sse
            } else if start.starts_with(b"{") || start.starts_with(b"[") {
                Shape::Json
            } else {
                Shape::Other
            };
            decided = std::mem::take(&mut self.undecided);
            &decided[..]
        } else {
            chunk
        };
        if self.shape != Shape::Sse {
            return chunk.to_vec();
        }
        self.framer.push(chunk);
        let mut out = Vec::new();
        while let Some(block) = self.framer.next_raw_block() {
            for event in self.join(&block) {
                out.extend(restore_event(table, &event, misses));
            }
        }
        out
    }

    /// End of stream. SSE: the held tail, then any unframed bytes, restored.
    /// Otherwise: opening bytes a shape was never decided for, unrestored.
    pub(crate) fn finish(&mut self, table: &RestoreTable, misses: &mut usize) -> Vec<u8> {
        if self.shape != Shape::Sse {
            return std::mem::take(&mut self.undecided);
        }
        let mut out = Vec::new();
        if let Some(held) = self.held.take() {
            out.extend(restore_event(table, &held.render(), misses));
        }
        let rest = self.framer.take_remaining();
        let (restored, m) = table.restore_bytes_as(&rest, Escape::Raw);
        *misses += m;
        out.extend(restored);
        out
    }

    /// The events `block` becomes once split tokens are rejoined.
    fn join(&mut self, block: &[u8]) -> Vec<Vec<u8>> {
        let Some(mut next) = Delta::parse(block) else {
            if is_keepalive(block) {
                return vec![block.to_vec()];
            }
            let mut out: Vec<Vec<u8>> = self.held.take().map(Delta::render).into_iter().collect();
            out.push(block.to_vec());
            return out;
        };
        let mut out = Vec::new();
        match self.held.take() {
            Some(mut held) if held.stream == next.stream => {
                let cut = safe_cut(&held.text, &next.text);
                next.text.insert_str(0, &held.text[cut..]);
                held.text.truncate(cut);
                out.push(held.render());
            }
            Some(held) => out.push(held.render()),
            None => {}
        }
        if hold_from(next.text.as_bytes(), MAX_TOKEN_LEN).is_some() {
            self.held = Some(next);
        } else {
            out.push(next.render());
        }
        out
    }
}

/// Where the held text `head` may end once `tail` follows it: before any
/// token the two would otherwise split, and before an unfinished one.
/// Every cut lands on an ASCII `_` or on the join, so it is a char boundary.
fn safe_cut(head: &str, tail: &str) -> usize {
    let joined = format!("{head}{tail}");
    let bytes = joined.as_bytes();
    let mut cut = head.len();
    let mut i = 0;
    while i < cut {
        match parse_token(bytes, i) {
            Some((_, len)) if i + len > head.len() => cut = i,
            Some((_, len)) => i += len,
            None => i += 1,
        }
    }
    match hold_from(bytes, MAX_TOKEN_LEN) {
        Some(open) => cut.min(open),
        None => cut,
    }
}

/// A comment or `ping`: no text, and safe to send ahead of a held delta.
fn is_keepalive(block: &[u8]) -> bool {
    std::str::from_utf8(block).is_ok_and(|b| {
        !b.lines().any(|l| l.starts_with("data:")) || b.lines().any(|l| l == "event: ping")
    })
}

/// The stream a delta event belongs to, and the path to its text.
fn stream_leaf(v: &Value) -> Option<(String, Vec<Seg>)> {
    anthropic_leaf(v)
        .or_else(|| responses_leaf(v))
        .or_else(|| chat_leaf(v))
        .or_else(|| gemini_leaf(v))
}

/// A field's JSON text, or empty when absent: part of a stream's name.
fn field(v: &Value, key: &str) -> String {
    v.get(key).map(Value::to_string).unwrap_or_default()
}

/// Anthropic: a `content_block_delta`, one stream per block `index`.
fn anthropic_leaf(v: &Value) -> Option<(String, Vec<Seg>)> {
    if v.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let delta_type = v.get("delta")?.get("type")?.as_str()?;
    let text = match delta_type {
        "text_delta" => "text",
        "input_json_delta" => "partial_json",
        "thinking_delta" => "thinking",
        _ => return None,
    };
    let stream = format!("anthropic:{}:{delta_type}", field(v, "index"));
    Some((stream, vec![Seg::Key("delta"), Seg::Key(text)]))
}

/// OpenAI Responses: every `response.*.delta` carries its text in `delta`.
fn responses_leaf(v: &Value) -> Option<(String, Vec<Seg>)> {
    let kind = v.get("type")?.as_str()?;
    if !kind.starts_with("response.") || !kind.ends_with(".delta") || !v.get("delta")?.is_string() {
        return None;
    }
    let stream = format!(
        "responses:{kind}:{}:{}:{}:{}",
        field(v, "item_id"),
        field(v, "output_index"),
        field(v, "content_index"),
        field(v, "summary_index")
    );
    Some((stream, vec![Seg::Key("delta")]))
}

/// Chat Completions: one choice whose delta has exactly one text field.
fn chat_leaf(v: &Value) -> Option<(String, Vec<Seg>)> {
    let [choice] = v.get("choices")?.as_array()?.as_slice() else {
        return None;
    };
    let delta = choice.get("delta")?;
    let mut found = Vec::new();
    for text in ["content", "reasoning_content", "reasoning"] {
        if delta.get(text).is_some_and(Value::is_string) {
            found.push((text.to_string(), vec![Seg::Key(text)]));
        }
    }
    let calls = delta.get("tool_calls").and_then(Value::as_array);
    for (j, call) in calls.into_iter().flatten().enumerate() {
        let arguments = call.get("function").and_then(|f| f.get("arguments"));
        if arguments.is_some_and(Value::is_string) {
            found.push((
                format!("tool:{}", field(call, "index")),
                vec![
                    Seg::Key("tool_calls"),
                    Seg::Index(j),
                    Seg::Key("function"),
                    Seg::Key("arguments"),
                ],
            ));
        }
    }
    let [(name, tail)] = <[_; 1]>::try_from(found).ok()?;
    let mut path = vec![Seg::Key("choices"), Seg::Index(0), Seg::Key("delta")];
    path.extend(tail);
    Some((format!("chat:{}:{name}", field(choice, "index")), path))
}

/// Gemini: one candidate with exactly one text part; thoughts stream apart.
fn gemini_leaf(v: &Value) -> Option<(String, Vec<Seg>)> {
    let [candidate] = v.get("candidates")?.as_array()?.as_slice() else {
        return None;
    };
    let parts = candidate.get("content")?.get("parts")?.as_array()?;
    let mut texts = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| p.get("text").is_some_and(Value::is_string));
    let (k, part) = texts.next()?;
    if texts.next().is_some() {
        return None;
    }
    let thought = part
        .get("thought")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stream = format!("gemini:{}:{thought}", field(candidate, "index"));
    let path = vec![
        Seg::Key("candidates"),
        Seg::Index(0),
        Seg::Key("content"),
        Seg::Key("parts"),
        Seg::Index(k),
        Seg::Key("text"),
    ];
    Some((stream, path))
}

fn leaf<'a>(v: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, seg| match *seg {
        Seg::Key(k) => v.get(k),
        Seg::Index(i) => v.get(i),
    })
}

fn leaf_mut<'a>(v: &'a mut Value, path: &[Seg]) -> Option<&'a mut Value> {
    path.iter().try_fold(v, |v, seg| match *seg {
        Seg::Key(k) => v.get_mut(k),
        Seg::Index(i) => v.get_mut(i),
    })
}

/// The one `data:` line's byte range and payload. `None` for zero or several.
fn single_data_line(block: &str) -> Option<(std::ops::Range<usize>, &str)> {
    let mut found = None;
    let mut at = 0;
    for line in block.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if let Some(payload) = body.strip_prefix("data:") {
            if found.is_some() {
                return None;
            }
            found = Some((
                at..at + body.len(),
                payload.strip_prefix(' ').unwrap_or(payload),
            ));
        }
        at += line.len();
    }
    found
}

/// Restore one whole event.
fn restore_event(table: &RestoreTable, event: &[u8], misses: &mut usize) -> Vec<u8> {
    if !event.windows(PREFIX.len()).any(|w| w == PREFIX.as_bytes()) {
        return event.to_vec();
    }
    let parsed = std::str::from_utf8(event).ok().and_then(|text| {
        let (range, payload) = single_data_line(text)?;
        let v: Value = serde_json::from_str(payload).ok()?;
        Some((text, range, v))
    });
    let Some((text, range, mut v)) = parsed else {
        // Not one JSON payload: nothing to decode into, so restore the bytes.
        let (out, m) = table.restore_bytes_as(event, Escape::Raw);
        *misses += m;
        return out;
    };
    // Responses streams tool arguments as `delta` on a `*arguments*` event.
    let arguments_event = v
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.contains("arguments"));
    restore_strings(&mut v, None, arguments_event, table, misses);
    let mut out = Vec::with_capacity(event.len());
    out.extend_from_slice(&text.as_bytes()[..range.start]);
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(v.to_string().as_bytes());
    out.extend_from_slice(&text.as_bytes()[range.end..]);
    out
}

fn restore_strings(
    v: &mut Value,
    key: Option<&str>,
    arguments_event: bool,
    table: &RestoreTable,
    misses: &mut usize,
) {
    match v {
        Value::String(s) if s.contains(PREFIX) => {
            let escape = match key {
                Some("partial_json" | "arguments") => Escape::Json,
                Some("delta") if arguments_event => Escape::Json,
                _ => Escape::Raw,
            };
            let (out, m) = table.restore_bytes_as(s.as_bytes(), escape);
            *misses += m;
            *s = String::from_utf8_lossy(&out).into_owned();
        }
        Value::Array(items) => {
            for item in items {
                restore_strings(item, key, arguments_event, table, misses);
            }
        }
        Value::Object(map) => {
            for (k, item) in map.iter_mut() {
                restore_strings(item, Some(k), arguments_event, table, misses);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::{RedactStore, redact_body, restore_table};
    use crate::sse::outbound::{content_block_stop, input_json_delta, text_delta};
    use serde_json::json;

    /// A store holding one secret, and the token it minted.
    fn minted(secret: &str) -> (RestoreTable, String) {
        let store = RedactStore::with_key([7u8; 32]);
        let mut body =
            json!({"messages": [{"role": "user", "content": format!("token={secret}")}]});
        redact_body(&store, "sess", &mut body);
        let text = body["messages"][0]["content"].as_str().unwrap();
        let token = text[text.find(PREFIX).unwrap()..text.rfind("__").unwrap() + 2].to_string();
        (restore_table(&store, "sess").unwrap(), token)
    }

    fn run(table: &RestoreTable, chunks: &[String]) -> String {
        let mut join = PlaceholderJoin::default();
        let mut misses = 0;
        let mut out = Vec::new();
        for c in chunks {
            out.extend(join.push(c.as_bytes(), table, &mut misses));
        }
        out.extend(join.finish(table, &mut misses));
        assert_eq!(misses, 0);
        String::from_utf8(out).unwrap()
    }

    /// Every `partial_json` of the stream's deltas, joined, parsed.
    fn tool_input(sse: &str) -> Value {
        let mut json = String::new();
        let mut framer = SseFramer::new();
        framer.push(sse.as_bytes());
        while let Some(Ok(event)) = framer.next_event() {
            let v: Value = serde_json::from_slice(&event.data).unwrap();
            if let Some(p) = v["delta"]["partial_json"].as_str() {
                json.push_str(p);
            }
        }
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn a_token_split_across_deltas_is_restored() {
        let (table, token) = minted("abcdefghij1234567890");
        let (a, b) = token.split_at(9);
        let out = run(
            &table,
            &[
                input_json_delta(1, &format!("{{\"skill\":\"ta{a}")),
                input_json_delta(1, &format!("{b}\"}}")),
                content_block_stop(1),
            ],
        );
        assert_eq!(tool_input(&out), json!({"skill": "taabcdefghij1234567890"}));
        assert!(out.ends_with(&content_block_stop(1)));
    }

    /// A quote or backslash in the value must survive both JSON levels of a
    /// tool call and the one level of a text delta.
    #[test]
    fn values_needing_escapes_keep_both_documents_valid() {
        let secret = r#"pa"ss\wo"rd99"#;
        let (table, token) = minted(&format!("'{secret}'"));
        let out = run(
            &table,
            &[
                input_json_delta(1, &format!("{{\"command\":\"echo {token}\"}}")),
                content_block_stop(1),
            ],
        );
        assert_eq!(
            tool_input(&out),
            json!({"command": format!("echo {secret}")})
        );
        let out = run(&table, &[text_delta(0, &format!("it is {token}."))]);
        assert!(
            out.contains(&text_delta(0, &format!("it is {secret}."))),
            "got: {out}"
        );
    }

    #[test]
    fn a_held_tail_flushes_before_the_block_stops() {
        let (table, _) = minted("abcdefghij1234567890");
        let out = run(
            &table,
            &[text_delta(0, "see __HR_SEC"), content_block_stop(0)],
        );
        assert_eq!(
            out,
            format!("{}{}", text_delta(0, "see __HR_SEC"), content_block_stop(0))
        );
    }

    #[test]
    fn plain_deltas_pass_through_byte_identical() {
        let (table, _) = minted("abcdefghij1234567890");
        let frames = [
            text_delta(0, "hello "),
            text_delta(0, "world"),
            content_block_stop(0),
        ];
        assert_eq!(run(&table, &frames), frames.concat());
    }

    #[test]
    fn a_json_body_is_left_to_the_byte_pass() {
        let (table, _) = minted("abcdefghij1234567890");
        let body = r#"{"content":[{"type":"text","text":"x __HR_SEC"}]}"#.to_string();
        let mut join = PlaceholderJoin::default();
        let mut misses = 0;
        assert_eq!(
            join.push(body.as_bytes(), &table, &mut misses),
            body.as_bytes()
        );
        assert!(matches!(join.byte_escape(), Some(Escape::Json)));
    }

    /// A first read that stops inside `event:` must still be read as SSE,
    /// or a token split across deltas reaches the client unjoined.
    #[test]
    fn a_stream_read_a_byte_at_a_time_is_still_joined() {
        let (table, token) = minted("abcdefghij1234567890");
        let (a, b) = token.split_at(9);
        let sse = [
            input_json_delta(1, &format!("{{\"skill\":\"ta{a}")),
            input_json_delta(1, &format!("{b}\"}}")),
            content_block_stop(1),
        ]
        .concat();
        let bytes: Vec<String> = sse.chars().map(String::from).collect();
        assert_eq!(
            tool_input(&run(&table, &bytes)),
            json!({"skill": "taabcdefghij1234567890"})
        );
    }

    fn frame(v: Value) -> String {
        format!("data: {v}\n\n")
    }

    /// The string at `pointer` in every event of `sse`, concatenated.
    fn joined(sse: &str, pointer: &str) -> String {
        let mut framer = SseFramer::new();
        framer.push(sse.as_bytes());
        let mut out = String::new();
        while let Some(Ok(event)) = framer.next_event() {
            let v: Value = serde_json::from_slice(&event.data).unwrap();
            out.push_str(v.pointer(pointer).and_then(Value::as_str).unwrap_or(""));
        }
        out
    }

    #[test]
    fn a_ping_between_the_halves_does_not_break_the_join() {
        let (table, token) = minted("abcdefghij1234567890");
        let (a, b) = token.split_at(9);
        let ping = "event: ping\ndata: {\"type\": \"ping\"}\n\n".to_string();
        let out = run(
            &table,
            &[
                text_delta(0, &format!("key {a}")),
                ping.clone(),
                text_delta(0, b),
            ],
        );
        assert_eq!(joined(&out, "/delta/text"), "key abcdefghij1234567890");
        assert!(out.starts_with(&ping), "the ping goes ahead: {out}");
    }

    #[test]
    fn responses_argument_deltas_are_joined_without_repeating_an_event() {
        let (table, token) = minted("abcdefghij1234567890");
        let (a, b) = token.split_at(12);
        let delta = |seq: u64, d: String| {
            frame(json!({"type": "response.function_call_arguments.delta",
                "sequence_number": seq, "item_id": "fc_1", "output_index": 0, "delta": d}))
        };
        let out = run(
            &table,
            &[
                delta(1, format!("{{\"skill\":\"ta{a}")),
                delta(2, format!("{b}\"}}")),
            ],
        );
        let args: Value = serde_json::from_str(&joined(&out, "/delta")).unwrap();
        assert_eq!(args, json!({"skill": "taabcdefghij1234567890"}));
        assert_eq!(out.matches("sequence_number").count(), 2);
    }

    #[test]
    fn a_chat_tool_call_keeps_its_id_and_name_in_the_first_chunk() {
        let (table, token) = minted("abcdefghij1234567890");
        let (a, b) = token.split_at(7);
        let first = frame(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_1", "type": "function",
             "function": {"name": "Skill", "arguments": format!("{{\"skill\":\"{a}")}}]}}]}));
        let rest = frame(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": format!("{b}\"}}")}}]}}]}));
        let out = run(&table, &[first, rest]);
        let args = joined(&out, "/choices/0/delta/tool_calls/0/function/arguments");
        assert_eq!(
            serde_json::from_str::<Value>(&args).unwrap(),
            json!({"skill": "abcdefghij1234567890"})
        );
        assert_eq!(out.matches("call_1").count(), 1);
        assert_eq!(out.matches("\"name\"").count(), 1);
    }

    #[test]
    fn gemini_text_parts_are_joined() {
        let (table, token) = minted("abcdefghij1234567890");
        let (a, b) = token.split_at(3);
        let part = |t: String, done: bool| {
            let mut c = json!({"index": 0, "content": {"role": "model", "parts": [{"text": t}]}});
            if done {
                c["finishReason"] = json!("STOP");
            }
            frame(json!({"candidates": [c]}))
        };
        let out = run(
            &table,
            &[
                part(format!("it is {a}"), false),
                part(format!("{b}."), true),
            ],
        );
        assert_eq!(
            joined(&out, "/candidates/0/content/parts/0/text"),
            "it is abcdefghij1234567890."
        );
        assert_eq!(out.matches("STOP").count(), 1);
    }
}
