//! Restore redaction placeholders in an SSE stream one event at a time.
//!
//! A byte scan survives a token cut by a chunk edge but not one cut by SSE
//! framing. A routed model streams a placeholder it echoes a few characters
//! per delta, so on the wire the token's halves sit in two
//! `content_block_delta` events with a frame's worth of JSON between them,
//! and no byte scan can see it whole. The Codex translator emits one
//! `input_json_delta` per upstream argument fragment, which is how a Skill
//! call reached Claude Code still holding the token as its skill name.
//!
//! So the stream is framed first. When a delta's text ends inside a token (or
//! on a partial `__HR_`), the tail is held and prepended to the next delta of
//! the same block, so the token travels in one event. Any other event flushes
//! the held tail first, so nothing is reordered and nothing outlives its
//! block.
//!
//! Each event is then restored inside its decoded JSON, not in its bytes: a
//! value holding `"` or `\` spliced raw into the wire would end its string
//! early. A text value goes back verbatim and the serializer escapes it; a
//! `partial_json` or `arguments` value is itself JSON text, so it is escaped
//! once more for that inner level. Events without a token pass through
//! byte-identical.

use serde_json::Value;

use crate::redact::{Escape, MAX_TOKEN_LEN, PREFIX, RestoreTable, hold_from};
use crate::sse::framing::SseFramer;
use crate::sse::outbound::content_block_delta;

/// A delta tail waiting for the rest of its token.
struct Held {
    index: usize,
    delta_type: String,
    field: &'static str,
    text: String,
}

impl Held {
    fn frame(&self) -> String {
        content_block_delta(self.index, &self.delta_type, self.field, &self.text)
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
    framer: SseFramer,
    held: Option<Held>,
}

impl Default for PlaceholderJoin {
    fn default() -> Self {
        Self {
            shape: Shape::Unknown,
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
    /// else, the chunk untouched.
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        table: &RestoreTable,
        misses: &mut usize,
    ) -> Vec<u8> {
        if self.shape == Shape::Unknown {
            let start = chunk.trim_ascii_start();
            if start.is_empty() {
                return chunk.to_vec();
            }
            self.shape = if [b"event:".as_slice(), b"data:", b":"]
                .iter()
                .any(|p| start.starts_with(p))
            {
                Shape::Sse
            } else if start.starts_with(b"{") || start.starts_with(b"[") {
                Shape::Json
            } else {
                Shape::Other
            };
        }
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

    /// End of stream, SSE only: the held tail, then any unframed bytes.
    pub(crate) fn finish(&mut self, table: &RestoreTable, misses: &mut usize) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(held) = self.held.take() {
            out.extend(restore_event(table, held.frame().as_bytes(), misses));
        }
        let rest = self.framer.take_remaining();
        let (restored, m) = table.restore_bytes_as(&rest, Escape::Raw);
        *misses += m;
        out.extend(restored);
        out
    }

    /// The events `block` becomes once split tokens are rejoined.
    fn join(&mut self, block: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some((index, delta_type, field, text)) = text_delta(block) else {
            if let Some(held) = self.held.take() {
                out.push(held.frame().into_bytes());
            }
            out.push(block.to_vec());
            return out;
        };
        let (mut text, joined) = match self.held.take() {
            Some(held) if held.index == index && held.delta_type == delta_type => {
                (held.text + &text, true)
            }
            Some(held) => {
                out.push(held.frame().into_bytes());
                (text, false)
            }
            None => (text, false),
        };
        match hold_from(text.as_bytes(), MAX_TOKEN_LEN) {
            // The cut lands on `_`, which is ASCII, so it is a char boundary.
            Some(cut) => {
                let tail = text.split_off(cut);
                if !text.is_empty() {
                    out.push(content_block_delta(index, &delta_type, field, &text).into_bytes());
                }
                self.held = Some(Held {
                    index,
                    delta_type,
                    field,
                    text: tail,
                });
            }
            None if joined => {
                out.push(content_block_delta(index, &delta_type, field, &text).into_bytes());
            }
            None => out.push(block.to_vec()),
        }
        out
    }
}

/// `(index, delta type, field, text)` for a streamed text-bearing delta.
fn text_delta(block: &[u8]) -> Option<(usize, String, &'static str, String)> {
    let block = std::str::from_utf8(block).ok()?;
    if !block.contains("content_block_delta") {
        return None;
    }
    let v: Value = serde_json::from_str(single_data_line(block)?.1).ok()?;
    if v.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let index = usize::try_from(v.get("index")?.as_u64()?).ok()?;
    let delta = v.get("delta")?;
    let delta_type = delta.get("type")?.as_str()?;
    let field = match delta_type {
        "text_delta" => "text",
        "input_json_delta" => "partial_json",
        "thinking_delta" => "thinking",
        _ => return None,
    };
    let text = delta.get(field)?.as_str()?;
    Some((index, delta_type.to_string(), field, text.to_string()))
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
            format!(
                "{}{}{}",
                text_delta(0, "see "),
                text_delta(0, "__HR_SEC"),
                content_block_stop(0)
            )
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
}
