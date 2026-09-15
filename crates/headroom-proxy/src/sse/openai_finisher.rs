//! Close out an OpenAI stream that dies after the client is committed.
//!
//! Mirrors `stream_finisher` (Anthropic) for the two OpenAI wire shapes the
//! proxy forwards: Chat Completions (`/v1/chat/completions`) and Responses
//! (`/v1/responses`, which is also what OpenCode Zen speaks). Plus a generic
//! fallback for SSE on paths with no registered parser.
//!
//! Why this exists: the proxy streams bytes through. Once the 200 headers are
//! sent, a mid-response upstream drop used to surface as a reset socket with a
//! truncated body — the client reports the turn as lost and the session stalls.
//! Nothing can recover the tokens the model never sent. What is recoverable is
//! the *shape* of the reply: a truncated message that is still well-formed ends
//! the turn, hands control back, and leaves the session usable.
//!
//! Safety properties (same as the Anthropic finisher):
//!
//! * **Response-body-only.** The request bytes define prefix/cache keys, so the
//!   tail never touches them. No recache risk.
//! * **Above the telemetry tee.** The tee lives inside `resp_stream`; this wraps
//!   it, so accounting still books the turn as the incomplete one it was.
//! * **Never papers over a provider error.** An in-band `error` /
//!   `response.failed` disables the tail.
//! * **A partial tool call never runs.** A truncated `tool_calls` /
//!   `function_call` is downgraded to plain text (`stop` / `completed` with a
//!   marker naming the discarded call), so the model re-issues it next turn.
//! * **Inert when complete.** A stream that already ended properly passes
//!   through untouched; a transport error trailing it is dropped as noise.

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::framing::SseFramer;

/// Depth of the hand-off queue to the client. Matches `stream_finisher`.
#[allow(dead_code)]
const CLIENT_QUEUE_DEPTH: usize = 64;

/// Appended to the reply so a truncated answer never reads as a finished one.
#[allow(dead_code)]
const TRUNCATION_MARKER: &str = "\n\n[truncated: the connection to the API dropped mid-response]";

/// The marker for a drop that took an unfinished tool call with it. Naming the
/// call is what makes it recoverable: the model reads this on its next turn
/// and re-issues the call itself.
#[allow(dead_code)]
fn tool_truncation_marker(tool_name: Option<&str>) -> String {
    match tool_name {
        Some(name) => format!(
            "\n\n[truncated: the connection to the API dropped mid-response. \
             A pending `{name}` tool call was discarded and did NOT run. \
             Re-issue it if it is still wanted.]"
        ),
        None => "\n\n[truncated: the connection to the API dropped mid-response. \
         A pending tool call was discarded and did NOT run. \
         Re-issue it if it is still wanted.]"
            .to_string(),
    }
}

#[allow(dead_code)]
fn client_gone(request_id: &str) {
    tracing::warn!(
        request_id = %request_id,
        event = "openai_finisher_client_gone",
        "client stopped reading; upstream tail left unread"
    );
}

/// One parsed SSE block: optional `event:` name, concatenated `data:` payload.
#[allow(dead_code)]
struct ParsedBlock {
    event: Option<String>,
    data: Vec<u8>,
}

#[allow(dead_code)]
fn parse_block(raw: &[u8]) -> Option<ParsedBlock> {
    let mut event: Option<String> = None;
    let mut datas: Vec<&[u8]> = Vec::new();
    for line in raw.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b':' {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"event:") {
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            if let Ok(s) = std::str::from_utf8(rest) {
                event = Some(s.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"data:") {
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            datas.push(rest);
        }
    }
    if datas.is_empty() {
        return None;
    }
    let mut data = Vec::new();
    for (i, d) in datas.iter().enumerate() {
        if i > 0 {
            data.push(b'\n');
        }
        data.extend_from_slice(d);
    }
    Some(ParsedBlock { event, data })
}

#[allow(dead_code)]
fn is_done(data: &[u8]) -> bool {
    data == b"[DONE]"
}

// ─── Chat Completions ────────────────────────────────────────────────

#[derive(Default)]
#[allow(dead_code)]
struct ChatTool {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Default)]
#[allow(dead_code)]
struct ChatChoice {
    finish_reason: Option<String>,
    tools: HashMap<usize, ChatTool>,
    has_content: bool,
}

#[derive(Default)]
#[allow(dead_code)]
struct ChatWire {
    id: Option<String>,
    model: Option<String>,
    saw_first: bool,
    saw_done: bool,
    saw_error: bool,
    choices: HashMap<usize, ChatChoice>,
}

impl ChatWire {
    #[allow(dead_code)]
    fn observe(&mut self, v: &serde_json::Value) {
        if v.get("error").is_some() {
            self.saw_error = true;
            return;
        }
        if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
            if self.id.is_none() {
                self.id = Some(id.to_string());
            }
            self.saw_first = true;
        }
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            if self.model.is_none() {
                self.model = Some(m.to_string());
            }
            self.saw_first = true;
        }
        let Some(choices) = v.get("choices").and_then(|x| x.as_array()) else {
            return;
        };
        // An empty choices array is the usage trailer, not a turn chunk; it
        // must not mark the stream as started.
        if choices.is_empty() {
            return;
        }
        self.saw_first = true;
        for choice in choices {
            let index = choice.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let entry = self.choices.entry(index).or_default();
            if let Some(delta) = choice.get("delta") {
                if delta
                    .get("content")
                    .and_then(|x| x.as_str())
                    .is_some_and(|s| !s.is_empty())
                {
                    entry.has_content = true;
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                    for tc in tcs {
                        let tc_index =
                            tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        let t = entry.tools.entry(tc_index).or_default();
                        if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                            if t.id.is_none() {
                                t.id = Some(id.to_string());
                            }
                        }
                        if let Some(func) = tc.get("function") {
                            if let Some(n) = func.get("name").and_then(|x| x.as_str()) {
                                if t.name.is_none() {
                                    t.name = Some(n.to_string());
                                }
                            }
                        }
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                entry.finish_reason = Some(fr.to_string());
            }
        }
    }

    /// Bytes still owed the client. Empty when there is nothing to close, when
    /// the stream already ended, or when the provider reported an error.
    fn tail(&self) -> Vec<Bytes> {
        if !self.saw_first || self.saw_done || self.saw_error {
            return Vec::new();
        }
        let id = self.id.as_deref().unwrap_or("chatcmpl-truncated");
        let model = self.model.as_deref().unwrap_or("unknown");
        let created: i64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Every open choice still owes a finish chunk; a stream that already
        // has one for every choice only owes the sentinel.
        let mut open: Vec<usize> = self
            .choices
            .iter()
            .filter(|(_, c)| c.finish_reason.is_none())
            .map(|(i, _)| *i)
            .collect();
        open.sort_unstable();
        if open.is_empty() {
            if self.choices.is_empty() {
                return Vec::new();
            }
            return vec![Bytes::from_static(b"data: [DONE]\n\n")];
        }

        let mut out = Vec::new();
        for index in open {
            let choice = &self.choices[&index];
            let tool_name = choice.tools.values().find_map(|t| t.name.as_deref());
            let marker = if choice.tools.is_empty() {
                TRUNCATION_MARKER.to_string()
            } else {
                tool_truncation_marker(tool_name)
            };
            // The marker rides as assistant text so the turn is never empty
            // and the next turn can see what was lost.
            out.push(Bytes::from(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": index,
                        "delta": {"role": "assistant", "content": marker},
                        "finish_reason": serde_json::Value::Null,
                    }],
                })
            )));
            // Downgrade to `stop`: claiming `tool_calls` for a call whose
            // arguments never finished would make the client run it partial.
            out.push(Bytes::from(format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": index,
                        "delta": {},
                        "finish_reason": "stop",
                    }],
                })
            )));
        }
        out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }
}

/// Wrap an OpenAI Chat Completions SSE body so a mid-stream drop still ends as
/// a well-formed turn (`stop` + `[DONE]`, marked truncated).
#[allow(dead_code)]
pub(crate) fn finish_openai_chat_on_drop<S, E>(
    inner: S,
    request_id: String,
) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + std::fmt::Debug + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, E>>(CLIENT_QUEUE_DEPTH);
    tokio::spawn(async move {
        let mut inner = Box::pin(inner);
        let mut framer = SseFramer::new();
        let mut wire = ChatWire::default();
        let mut drop_err: Option<E> = None;

        loop {
            let chunk = match inner.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    drop_err = Some(e);
                    break;
                }
                None => break,
            };
            framer.push(&chunk);
            while let Some(raw) = framer.next_raw_block() {
                if let Some(block) = parse_block(&raw) {
                    if is_done(&block.data) {
                        wire.saw_done = true;
                    } else if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&block.data) {
                        wire.observe(&v);
                    }
                }
                if tx.send(Ok(raw)).await.is_err() {
                    return client_gone(&request_id);
                }
            }
        }

        let tail = wire.tail();
        if tail.is_empty() {
            let Some(e) = drop_err else { return };
            if wire.saw_first {
                tracing::debug!(
                    request_id = %request_id,
                    error = %e,
                    cause = ?e,
                    "transport error after the chat stream ended; not forwarded"
                );
                return;
            }
            tracing::debug!(
                request_id = %request_id,
                error = %e,
                cause = ?e,
                "chat stream dropped before any chunk; passing the error down"
            );
            let _ = tx.send(Err(e)).await;
            return;
        }
        tracing::warn!(
            request_id = %request_id,
            event = "stream_tail_synthesised",
            provider = "openai_chat",
            error = drop_err.as_ref().map_or("body ended early".to_string(), |e| e.to_string()),
            cause = ?drop_err,
            "upstream chat stream died mid-response; closing the turn cleanly so the session survives"
        );
        for b in tail {
            if tx.send(Ok(b)).await.is_err() {
                return client_gone(&request_id);
            }
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

// ─── Responses (incl. Zen) ───────────────────────────────────────────

#[derive(Default)]
#[allow(dead_code)]
struct RespWire {
    response_id: Option<String>,
    model: Option<String>,
    saw_created: bool,
    saw_done: bool,
    terminal: Option<String>,
    saw_error: bool,
    message_items: HashMap<String, bool>, // item_id -> complete
    function_calls: HashMap<String, (Option<String>, bool)>, // item_id -> (name, complete)
    last_message: Option<String>,
}

impl RespWire {
    #[allow(dead_code)]
    fn observe(&mut self, event: Option<&str>, v: &serde_json::Value) {
        let Some(name) = event else {
            // Responses mandates an event: line; a bare data: chunk is drift.
            // A top-level error object without one is still a provider error.
            if v.get("error").is_some() {
                self.saw_error = true;
            }
            return;
        };
        match name {
            "response.created" => {
                if let Some(resp) = v.get("response") {
                    if let Some(id) = resp.get("id").and_then(|x| x.as_str()) {
                        self.response_id = Some(id.to_string());
                    }
                    if let Some(m) = resp.get("model").and_then(|x| x.as_str()) {
                        self.model = Some(m.to_string());
                    }
                }
                self.saw_created = true;
            }
            "output_item.added" => {
                if let Some(item) = v.get("item") {
                    let id = item.get("id").and_then(|x| x.as_str()).map(str::to_string);
                    let ty = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    if let Some(id) = id {
                        if ty == "function_call" {
                            let nm = item
                                .get("name")
                                .and_then(|x| x.as_str())
                                .map(str::to_string);
                            self.function_calls.entry(id).or_insert((nm, false));
                        } else if ty == "message" {
                            self.message_items.entry(id.clone()).or_insert(false);
                            self.last_message = Some(id);
                        }
                    }
                }
            }
            "output_item.done" => {
                if let Some(item) = v.get("item") {
                    if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                        if let Some(c) = self.message_items.get_mut(id) {
                            *c = true;
                        }
                        if let Some((_, c)) = self.function_calls.get_mut(id) {
                            *c = true;
                        }
                    }
                }
            }
            "response.output_text.delta" | "output_text.delta" => {
                if let Some(id) = v.get("item_id").and_then(|x| x.as_str()) {
                    self.message_items.entry(id.to_string()).or_insert(false);
                    self.last_message = Some(id.to_string());
                }
            }
            "response.function_call_arguments.delta" | "function_call_arguments.delta" => {
                if let Some(id) = v.get("item_id").and_then(|x| x.as_str()) {
                    self.function_calls
                        .entry(id.to_string())
                        .or_insert((None, false));
                }
            }
            "response.function_call_arguments.done" | "function_call_arguments.done" => {
                // Arguments finished, but the item still owes output_item.done;
                // do not mark complete here.
            }
            "response.completed" => {
                self.terminal = Some("completed".to_string());
                if let Some(resp) = v.get("response") {
                    if let Some(id) = resp.get("id").and_then(|x| x.as_str()) {
                        self.response_id = Some(id.to_string());
                    }
                }
            }
            "response.incomplete" => {
                self.terminal = Some("incomplete".to_string());
            }
            "response.failed" => {
                self.terminal = Some("failed".to_string());
            }
            _ => {}
        }
    }

    fn open_function_name(&self) -> Option<String> {
        let mut names: Vec<&String> = self
            .function_calls
            .iter()
            .filter(|(_, (_, done))| !(*done))
            .filter_map(|(_, (n, _))| n.as_ref())
            .collect();
        names.sort();
        names.first().map(|s| (*s).clone())
    }

    fn tail(&self) -> Vec<Bytes> {
        if !self.saw_created || self.saw_done || self.saw_error {
            return Vec::new();
        }
        if let Some(t) = self.terminal.as_deref() {
            // Failed is a provider error: never paper over it. Completed /
            // incomplete only owe the sentinel.
            if t == "failed" {
                return Vec::new();
            }
            return vec![Bytes::from_static(b"data: [DONE]\n\n")];
        }
        let id = self.response_id.as_deref().unwrap_or("resp_truncated");
        let mut out = Vec::new();

        let discarded = self.function_calls.iter().any(|(_, (_, done))| !(*done));
        let marker = if discarded {
            tool_truncation_marker(self.open_function_name().as_deref())
        } else {
            TRUNCATION_MARKER.to_string()
        };

        // Reuse the open message item so the marker lands in context;
        // otherwise open a fresh one to guarantee non-empty content.
        let target: String = self
            .message_items
            .iter()
            .find(|(_, done)| !(**done))
            .map(|(k, _)| k.clone())
            .or_else(|| self.last_message.clone())
            .unwrap_or_else(|| "msg_truncated".to_string());
        let is_new = !self.message_items.contains_key(&target);

        if is_new {
            out.push(Bytes::from(format!(
                "event: output_item.added\ndata: {}\n\n",
                serde_json::json!({
                    "type": "output_item.added",
                    "output_index": 0,
                    "item": {"id": target, "type": "message", "role": "assistant", "content": []},
                })
            )));
        }
        out.push(Bytes::from(format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            serde_json::json!({
                "type": "response.output_text.delta",
                "item_id": target,
                "output_index": 0,
                "content_index": 0,
                "delta": marker,
            })
        )));
        out.push(Bytes::from(format!(
            "event: response.output_text.done\ndata: {}\n\n",
            serde_json::json!({
                "type": "response.output_text.done",
                "item_id": target,
                "output_index": 0,
                "content_index": 0,
                "text": marker,
            })
        )));
        out.push(Bytes::from(format!(
            "event: output_item.done\ndata: {}\n\n",
            serde_json::json!({
                "type": "output_item.done",
                "output_index": 0,
                "item": {"id": target, "type": "message", "role": "assistant", "status": "completed"},
            })
        )));

        let mut resp = serde_json::json!({
            "type": "response.completed",
            "response": {"id": id, "status": "completed"},
        });
        if let Some(m) = self.model.as_deref() {
            resp["response"]["model"] = serde_json::Value::String(m.to_string());
        }
        out.push(Bytes::from(format!(
            "event: response.completed\ndata: {resp}\n\n"
        )));
        out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }
}

/// Wrap an OpenAI Responses SSE body (incl. Zen) so a mid-stream drop still
/// ends as a well-formed turn (`response.completed` + `[DONE]`, marked).
#[allow(dead_code)]
pub(crate) fn finish_openai_responses_on_drop<S, E>(
    inner: S,
    request_id: String,
) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + std::fmt::Debug + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, E>>(CLIENT_QUEUE_DEPTH);
    tokio::spawn(async move {
        let mut inner = Box::pin(inner);
        let mut framer = SseFramer::new();
        let mut wire = RespWire::default();
        let mut drop_err: Option<E> = None;

        loop {
            let chunk = match inner.next().await {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    drop_err = Some(e);
                    break;
                }
                None => break,
            };
            framer.push(&chunk);
            while let Some(raw) = framer.next_raw_block() {
                if let Some(block) = parse_block(&raw) {
                    if is_done(&block.data) {
                        wire.saw_done = true;
                    } else if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&block.data) {
                        wire.observe(block.event.as_deref(), &v);
                    }
                }
                if tx.send(Ok(raw)).await.is_err() {
                    return client_gone(&request_id);
                }
            }
        }

        let tail = wire.tail();
        if tail.is_empty() {
            let Some(e) = drop_err else { return };
            if wire.saw_created {
                tracing::debug!(
                    request_id = %request_id,
                    error = %e,
                    cause = ?e,
                    "transport error after the responses stream ended; not forwarded"
                );
                return;
            }
            tracing::debug!(
                request_id = %request_id,
                error = %e,
                cause = ?e,
                "responses stream dropped before created; passing the error down"
            );
            let _ = tx.send(Err(e)).await;
            return;
        }
        tracing::warn!(
            request_id = %request_id,
            event = "stream_tail_synthesised",
            provider = "openai_responses",
            error = drop_err.as_ref().map_or("body ended early".to_string(), |e| e.to_string()),
            cause = ?drop_err,
            "upstream responses stream died mid-response; closing the turn cleanly so the session survives"
        );
        for b in tail {
            if tx.send(Ok(b)).await.is_err() {
                return client_gone(&request_id);
            }
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

// ─── Generic SSE fallback ────────────────────────────────────────────
//
// For SSE on paths with no registered parser (unknown provider shapes): no
// terminator can be synthesised safely, so the patch is only to end the body
// cleanly instead of resetting it once bytes have gone out. Zero tokens
// added, request untouched, telemetry still books the short stream.

/// End a provider-unknown SSE body cleanly after a mid-stream drop.
#[allow(dead_code)]
pub(crate) fn finish_generic_on_drop<S, E>(
    inner: S,
    request_id: String,
) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + std::fmt::Debug + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, E>>(CLIENT_QUEUE_DEPTH);
    tokio::spawn(async move {
        let mut inner = Box::pin(inner);
        let mut saw_bytes = false;
        loop {
            match inner.next().await {
                Some(Ok(b)) => {
                    if !b.is_empty() {
                        saw_bytes = true;
                    }
                    if tx.send(Ok(b)).await.is_err() {
                        return client_gone(&request_id);
                    }
                }
                Some(Err(e)) => {
                    if saw_bytes {
                        tracing::warn!(
                            request_id = %request_id,
                            event = "stream_tail_synthesised",
                            provider = "generic_sse",
                            error = %e,
                            cause = ?e,
                            "upstream SSE stream died mid-response; ending the body cleanly so the session survives"
                        );
                        return;
                    }
                    let _ = tx.send(Err(e)).await;
                    return;
                }
                None => return,
            }
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive_chat(chunks: &[&str]) -> String {
        let owned: Vec<Result<Bytes, std::io::Error>> = chunks
            .iter()
            .map(|c| Ok(Bytes::from(c.to_string())))
            .collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let out = finish_openai_chat_on_drop(futures_util::stream::iter(owned), "test".into());
            futures_util::pin_mut!(out);
            let mut s = String::new();
            while let Some(item) = out.next().await {
                s.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
            }
            s
        })
    }

    fn drive_responses(chunks: &[&str]) -> String {
        let owned: Vec<Result<Bytes, std::io::Error>> = chunks
            .iter()
            .map(|c| Ok(Bytes::from(c.to_string())))
            .collect();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let out =
                finish_openai_responses_on_drop(futures_util::stream::iter(owned), "test".into());
            futures_util::pin_mut!(out);
            let mut s = String::new();
            while let Some(item) = out.next().await {
                s.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
            }
            s
        })
    }

    const CHAT_FIRST: &str = "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
    const CHAT_DONE: &str = "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    #[test]
    fn chat_complete_passes_through_untouched() {
        let chunks = [CHAT_FIRST, CHAT_DONE];
        assert_eq!(drive_chat(&chunks), chunks.concat());
    }

    #[test]
    fn chat_drop_closes_with_stop_and_done() {
        let out = drive_chat(&[CHAT_FIRST]);
        assert!(out.contains("[truncated"), "marker missing: {out}");
        assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
        assert!(out.trim_end().ends_with("data: [DONE]"), "{out}");
        assert!(out.contains("hi"), "paid-for text lost: {out}");
    }

    #[test]
    fn chat_drop_after_finish_only_owes_done() {
        let finish_only = "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let out = drive_chat(&[CHAT_FIRST, finish_only]);
        assert!(
            !out.contains("[truncated"),
            "complete message marked: {out}"
        );
        assert!(out.trim_end().ends_with("data: [DONE]"), "{out}");
    }

    #[test]
    fn chat_drop_before_any_chunk_synthesises_nothing() {
        assert_eq!(drive_chat(&[]), "");
    }

    #[test]
    fn chat_in_band_error_is_never_papered_over() {
        let out = drive_chat(&[CHAT_FIRST, "data: {\"error\":{\"message\":\"boom\"}}\n\n"]);
        assert!(out.contains("boom"), "{out}");
        assert!(!out.contains("[truncated"), "{out}");
    }

    #[test]
    fn chat_partial_tool_is_downgraded_and_named() {
        let tool = "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Agent\",\"arguments\":\"{\\\"p\"}}]},\"finish_reason\":null}]}\n\n";
        let out = drive_chat(&[CHAT_FIRST, tool]);
        assert!(out.contains("Agent"), "{out}");
        assert!(out.contains("did NOT run"), "{out}");
        assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
        assert!(!out.contains("\"finish_reason\":\"tool_calls\""), "{out}");
    }

    const RESP_CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\"}}\n\n";
    const RESP_DELTA: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"hi\"}\n\n";
    const RESP_COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\"}}\n\n";
    const RESP_DONE: &str = "data: [DONE]\n\n";

    #[test]
    fn responses_complete_passes_through_untouched() {
        let chunks = [RESP_CREATED, RESP_DELTA, RESP_COMPLETED, RESP_DONE];
        assert_eq!(drive_responses(&chunks), chunks.concat());
    }

    #[test]
    fn responses_drop_closes_with_completed_and_done() {
        let out = drive_responses(&[RESP_CREATED, RESP_DELTA]);
        assert!(out.contains("[truncated"), "{out}");
        assert!(out.contains("response.completed"), "{out}");
        assert!(out.trim_end().ends_with("data: [DONE]"), "{out}");
        assert!(out.contains("hi"), "{out}");
    }

    #[test]
    fn responses_terminal_without_done_only_owes_done() {
        let out = drive_responses(&[RESP_CREATED, RESP_DELTA, RESP_COMPLETED]);
        assert!(!out.contains("[truncated"), "{out}");
        assert!(out.trim_end().ends_with("data: [DONE]"), "{out}");
    }

    #[test]
    fn responses_drop_before_created_synthesises_nothing() {
        assert_eq!(drive_responses(&[]), "");
    }

    #[test]
    fn responses_failed_is_never_papered_over() {
        let failed = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\"}}\n\n";
        let out = drive_responses(&[RESP_CREATED, RESP_DELTA, failed]);
        assert!(!out.contains("[truncated"), "{out}");
        assert!(!out.contains("response.completed"), "{out}");
    }

    #[test]
    fn responses_dropped_function_call_is_named() {
        let added = "event: output_item.added\ndata: {\"type\":\"output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"name\":\"SendMessage\"}}\n\n";
        let delta = "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"t\"}}\n\n";
        let out = drive_responses(&[RESP_CREATED, RESP_DELTA, added, delta]);
        assert!(out.contains("SendMessage"), "{out}");
        assert!(out.contains("did NOT run"), "{out}");
        assert!(out.contains("response.completed"), "{out}");
    }
}
