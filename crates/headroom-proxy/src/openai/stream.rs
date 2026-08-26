//! Streaming OpenAI → Anthropic translation.
//!
//! `StreamTranslator` holds the per-turn state that an Anthropic SSE stream
//! needs but the OpenAI wire formats do not carry: which content block is
//! open, what index it has, and how much of the reasoning envelope has been
//! emitted. Dropping it books the turn, so a client disconnect mid-stream
//! still reaches the cost tracker.

use crate::handlers::reasoning_signature::{encode_reasoning_signature, PendingReasoning};
use crate::routed::outcome::{book_routed_outcome, RoutedOutcomeContext};
use serde_json::{json, Value};

/// Safety net for turns that never reach a terminal event — a client
/// disconnect, or an upstream that drops the connection mid-stream. Those
/// tokens were still spent and still cost money, and the Claude path books
/// them too (its state machine emits when the channel closes, however it
/// closed). `emit_outcome` is idempotent, so this is a no-op for the ordinary
/// case where `response.completed` or `[DONE]` already booked the turn.
impl Drop for StreamTranslator {
    fn drop(&mut self) {
        self.finish_rate_limit_observation();
        if self.outcome.is_some() && !self.outcome_emitted {
            let usage = self.last_usage.clone();
            self.emit_outcome(usage.as_ref(), 200);
        }
    }
}

/// Which content block is open, if any.
///
/// Anthropic's stream allows one open block at a time and numbers them
/// consecutively, so "what is open" and "what index it has" are one fact. They
/// used to be four fields — three booleans and a counter — updated by hand at
/// eighteen call sites, and every site had to remember to advance the index on
/// the way out. Missing that once puts two blocks on the same index, which the
/// client renders as a single garbled one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text,
    Thinking,
    Tool,
}

pub(crate) struct StreamTranslator {
    model: String,
    content_block_index: usize,
    started: bool,
    open: Option<OpenBlock>,
    current_tool_id: String,
    current_tool_name: String,
    total_output_tokens: u64,
    saw_tool_use: bool,
    /// Identity of the reasoning item currently streaming, assembled from the
    /// `output_item.added`/`.done` pair that describes it.
    pending_reasoning: PendingReasoning,
    /// Where to file a `rate_limits` object if one appears in the stream.
    /// `None` in unit tests, which do not exercise quota reporting.
    codex_limits: Option<crate::codex_rate_limits::CodexRateLimitStore>,
    /// True when either the response headers or any SSE frame carried quota.
    /// The negative signal is emitted once from `Drop`, which is the actual end
    /// of the upstream stream rather than one ordinary frame that lacked it.
    codex_rate_limits_seen: bool,
    codex_rate_limits_finished: bool,
    /// Where to book the turn once usage arrives. `None` in unit tests, which
    /// assert on translated events rather than metrics.
    outcome: Option<RoutedOutcomeContext>,
    /// Guards against booking one turn twice. A stream can carry a terminal
    /// event *and* a trailing `[DONE]`, and the buffered fallback can fire on
    /// top of that.
    outcome_emitted: bool,
    /// Latched on the first upstream frame — the only point where TTFB is
    /// observable.
    ttfb_ms: f64,
    /// Most recent provider `usage` block seen. Chat Completions delivers it on
    /// a chunk of its own rather than a terminal event, so it has to be held
    /// until the stream ends.
    last_usage: Option<Value>,
}

impl StreamTranslator {
    /// Close the open block and move to the next index.
    ///
    /// Pairing the stop event with the index bump is the whole point: they are
    /// never correct apart.
    fn close_block(&mut self, events: &mut Vec<String>) {
        if self.open.take().is_some() {
            events.push(self.emit_content_block_stop());
            self.content_block_index += 1;
        }
    }

    /// Close the open block only if it is `kind`, leaving any other alone.
    fn close_block_if(&mut self, kind: OpenBlock, events: &mut Vec<String>) {
        if self.open == Some(kind) {
            self.close_block(events);
        }
    }

    /// Close the open block at the end of the stream.
    ///
    /// The index does not advance here, because no further block will use it.
    fn close_block_final(&mut self, events: &mut Vec<String>) {
        if self.open.take().is_some() {
            events.push(self.emit_content_block_stop());
        }
    }

    /// Make `kind` the open block, closing whatever else was open first.
    ///
    /// A no-op when `kind` is already open, so callers can say what they need
    /// rather than test what they have.
    fn open_block(&mut self, kind: OpenBlock, events: &mut Vec<String>) {
        if self.open == Some(kind) {
            return;
        }
        self.close_block(events);
        events.push(match kind {
            OpenBlock::Text => self.emit_content_block_start_text(),
            OpenBlock::Thinking => self.emit_content_block_start_thinking(),
            OpenBlock::Tool => self.emit_content_block_start_tool(
                &self.current_tool_id.clone(),
                &self.current_tool_name.clone(),
            ),
        });
        self.open = Some(kind);
    }

    /// Open a tool block for the call now in `current_tool_id`/`_name`.
    ///
    /// Unlike `open_block`, this always closes first: two consecutive tool
    /// calls are two blocks, not one.
    fn open_tool_block(&mut self, events: &mut Vec<String>) {
        self.close_block(events);
        events.push(self.emit_content_block_start_tool(
            &self.current_tool_id.clone(),
            &self.current_tool_name.clone(),
        ));
        self.open = Some(OpenBlock::Tool);
    }

    fn new(model: String) -> Self {
        Self {
            model,
            content_block_index: 0,
            started: false,
            open: None,
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            total_output_tokens: 0,
            saw_tool_use: false,
            pending_reasoning: PendingReasoning::default(),
            codex_limits: None,
            codex_rate_limits_seen: false,
            codex_rate_limits_finished: false,
            outcome: None,
            outcome_emitted: false,
            ttfb_ms: 0.0,
            last_usage: None,
        }
    }

    fn with_codex_limits(mut self, store: crate::codex_rate_limits::CodexRateLimitStore) -> Self {
        self.codex_limits = Some(store);
        self
    }

    fn with_initial_rate_limits_seen(mut self, seen: bool) -> Self {
        self.codex_rate_limits_seen = seen;
        self
    }

    fn finish_rate_limit_observation(&mut self) {
        if self.codex_limits.is_none()
            || self.codex_rate_limits_seen
            || self.codex_rate_limits_finished
        {
            return;
        }
        self.codex_rate_limits_finished = true;
        let request_id = self
            .outcome
            .as_ref()
            .map(|ctx| ctx.request_id.as_str())
            .unwrap_or("unknown");
        tracing::warn!(
            event = "codex_rate_limits_missing",
            request_id = %request_id,
            model = %self.model,
            "routed Codex stream ended without quota in response headers or SSE frames"
        );
    }

    fn with_outcome(mut self, ctx: Option<RoutedOutcomeContext>) -> Self {
        self.outcome = ctx;
        self
    }

    /// Close out the CTX-7 usage observation parked at request time.
    ///
    /// `begin_request` leaves a pending entry keyed by request id; without a
    /// matching `complete` the turn is never classified and the re-cache
    /// watchdog (and the cache-health statusline segment) stays blank.
    ///
    /// The observer takes Anthropic-named counters. The Responses API reports
    /// cache reads but has no cache-creation counter, so zero goes in for
    /// writes — the same mapping used elsewhere on this path.
    fn complete_usage_observation(&self, usage: Option<&Value>) {
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        let Some(observer) = ctx.usage_observer.as_ref() else {
            return;
        };
        let get = |key: &str| -> u64 {
            usage
                .and_then(|u| u.get(key))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };
        let cache_read = usage
            .and_then(|u| {
                u.get("input_tokens_details")
                    .or_else(|| u.get("prompt_tokens_details"))
            })
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let class = observer.complete(
            &ctx.request_id,
            get("input_tokens").max(get("prompt_tokens")),
            cache_read,
            0,
            // The Responses API publishes no cache-creation counter at all, so
            // there is no TTL breakdown to split — `None`, not a pair of zeros,
            // which would claim this endpoint wrote nothing at either tier.
            None,
        );
        // Persist it, same as the Claude path: the observer's counters are
        // in-memory and reset on restart.
        if let Some(class) = class {
            use headroom_core::request_outcome::OutcomeSink as _;
            let (reason, wasted) = class.as_record();
            ctx.sink.record_cache_outcome("routed", reason, wasted);
        }
    }

    /// Hand the turn's cache-token counts to the prefix-replay store, which
    /// needs them to judge how much of the prefix the provider actually held.
    ///
    /// Only on a clean completion, matching the Claude path's `MessageStop`
    /// gate: a turn that died mid-stream tells us nothing reliable about the
    /// cache, and recording it would corrupt next turn's replay decision.
    /// The Responses API reports cache *reads* only — there is no write
    /// counter to pass, unlike Anthropic's `cache_creation_input_tokens`.
    fn complete_replay(&self, usage: Option<&Value>) {
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        let Some(store) = ctx.replay_store.as_ref() else {
            return;
        };
        let cache_read = usage
            .and_then(|u| {
                u.get("input_tokens_details")
                    .or_else(|| u.get("prompt_tokens_details"))
            })
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        store.complete(&ctx.request_id, cache_read, 0);
    }

    /// Latch time-to-first-byte. Written once and never overwritten, mirroring
    /// `latch_ttfb` on the Claude path.
    fn latch_ttfb(&mut self) {
        if self.ttfb_ms == 0.0 {
            if let Some(ctx) = self.outcome.as_ref() {
                self.ttfb_ms = ctx.started_at.elapsed().as_secs_f64() * 1000.0;
            }
        }
    }

    /// Book the finished turn through the shared outcome funnel.
    ///
    /// `usage` is the provider's own block, in whichever shape the endpoint
    /// uses. Cache accounting follows the OpenAI convention the Claude path
    /// already encodes for these providers: `input_tokens` *includes* the
    /// cached prefix, so uncached is the difference. (Anthropic's own
    /// `input_tokens` already excludes it — getting this backwards would
    /// double-count the prefix.)
    fn emit_outcome(&mut self, usage: Option<&Value>, status_code: i64) {
        if self.outcome_emitted {
            return;
        }
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        self.outcome_emitted = true;
        book_routed_outcome(
            ctx,
            usage,
            self.total_output_tokens as i64,
            self.ttfb_ms,
            status_code,
        );
    }

    #[cfg(test)]
    fn process_line(&mut self, line: &str) -> Vec<String> {
        self.process_frame(None, line)
    }

    fn process_frame(&mut self, event_name: Option<&str>, data: &str) -> Vec<String> {
        let mut events = Vec::new();
        self.latch_ttfb();

        if data.trim().is_empty() || data.trim() == "[DONE]" {
            if data.trim() == "[DONE]" {
                // Last chance to book the turn: Chat Completions has no
                // terminal event, and a Responses stream can be cut off before
                // one arrives. No-op when a terminal event already booked it.
                let usage = self.last_usage.clone();
                self.emit_outcome(usage.as_ref(), 200);
            }
            if data.trim() == "[DONE]" && self.open.is_some() {
                self.close_block_final(&mut events);
                events.push(self.emit_message_delta("end_turn"));
                events.push(self.emit_message_stop());
            }
            return events;
        }

        if let Some(name) = event_name {
            if name.starts_with("response.") || name.starts_with("output_") {
                return self.process_responses_frame(name, data);
            }
        }

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return events,
        };

        self.process_chat_chunk(chunk)
    }

    fn process_chat_chunk(&mut self, chunk: Value) -> Vec<String> {
        let mut events = Vec::new();

        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        let delta = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"));

        let finish_reason = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(|r| r.as_str());

        if let Some(usage) = chunk.get("usage") {
            if let Some(tokens) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.total_output_tokens = tokens;
            }
            if !usage.is_null() {
                self.last_usage = Some(usage.clone());
            }
        }

        if let Some(delta) = delta {
            // Handle reasoning_content (thinking tokens from models like Qwen).
            if let Some(thinking) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                self.open_block(OpenBlock::Thinking, &mut events);
                events.push(self.emit_thinking_delta(thinking));
            }

            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                self.open_block(OpenBlock::Text, &mut events);
                events.push(self.emit_text_delta(text));
            }

            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        self.current_tool_id = id.to_string();
                        self.current_tool_name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();

                        self.open_tool_block(&mut events);
                    }

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            self.open_block(OpenBlock::Tool, &mut events);
                            events.push(self.emit_input_json_delta(args));
                        }
                    }
                }
            }
        }

        if let Some(reason) = finish_reason {
            self.close_block_final(&mut events);

            let stop_reason = match reason {
                "stop" => "end_turn",
                "tool_calls" => "tool_use",
                "length" => "max_tokens",
                _ => "end_turn",
            };
            events.push(self.emit_message_delta(stop_reason));
            events.push(self.emit_message_stop());
        }

        events
    }

    fn process_responses_frame(&mut self, event_name: &str, data: &str) -> Vec<String> {
        let mut events = Vec::new();

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return events,
        };

        // Quota can ride in the stream as well as the headers, and which one
        // carries it has changed before. Take it from wherever it shows up.
        if let Some(store) = self.codex_limits.as_ref() {
            if let Some(limits) = crate::codex_rate_limits::extract_rate_limits(&chunk) {
                store.record_rate_limits(&self.model, limits);
                self.codex_rate_limits_seen = true;
            }
        }

        if !self.started && event_name == "response.created" {
            if let Some(model) = chunk
                .get("response")
                .and_then(|resp| resp.get("model"))
                .and_then(|v| v.as_str())
            {
                self.model = model.to_string();
            }
        }

        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        match event_name {
            "response.output_text.delta" | "output_text.delta" => {
                let delta = chunk.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    self.open_block(OpenBlock::Text, &mut events);
                    events.push(self.emit_text_delta(delta));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                    if !delta.is_empty() {
                        self.open_block(OpenBlock::Thinking, &mut events);
                        events.push(self.emit_thinking_delta(delta));
                    }
                }
            }
            "response.reasoning_summary_part.added" => {
                // Part boundary: close the current thinking block so the next
                // summary part starts a fresh one.
                self.close_block_if(OpenBlock::Thinking, &mut events);
            }
            "response.output_item.added" => {
                let item = chunk.get("item");
                let item_type = item.and_then(|i| i.get("type")).and_then(|t| t.as_str());
                if item_type == Some("function_call") {
                    // `call_id` is what must round-trip back as
                    // function_call_output; fall back to `id` if absent.
                    self.current_tool_id = item
                        .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.current_tool_name = item
                        .and_then(|i| i.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.open_tool_block(&mut events);
                    self.saw_tool_use = true;
                }
                // A reasoning item may announce its id here and carry the blob
                // on `.done`, so start assembling as soon as it appears.
                if item_type == Some("reasoning") {
                    if let Some(item) = item {
                        self.pending_reasoning.capture(item);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if self.open == Some(OpenBlock::Tool) {
                    if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str()) {
                        if !delta.is_empty() {
                            events.push(self.emit_input_json_delta(delta));
                        }
                    }
                }
            }
            "response.output_item.done" => {
                let item_type = chunk
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str());
                if item_type == Some("function_call") {
                    self.close_block_if(OpenBlock::Tool, &mut events);
                }
                // The reasoning item is complete: seal its identity into the
                // thinking block's signature so the client hands it back next
                // turn. Without a usable pair there is nothing to replay and
                // the block stays a plain summary.
                if item_type == Some("reasoning") {
                    if let Some(item) = chunk.get("item") {
                        self.pending_reasoning.capture(item);
                    }
                    let signature = self
                        .pending_reasoning
                        .replay()
                        .as_ref()
                        .and_then(encode_reasoning_signature);
                    self.pending_reasoning.reset();
                    if let Some(signature) = signature {
                        // Reasoning summaries can be off entirely, in which case
                        // no block was ever opened. Open an empty one rather
                        // than drop the only copy of the item.
                        self.open_block(OpenBlock::Thinking, &mut events);
                        events.push(self.emit_signature_delta(&signature));
                        self.close_block(&mut events);
                    }
                }
            }
            "response.completed" => {
                if let Some(usage) = chunk.get("response").and_then(|v| v.get("usage")) {
                    if let Some(tokens) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        self.total_output_tokens = tokens;
                    }
                    // Ground-truth cache effectiveness: how many input tokens
                    // the codex backend served from its prompt cache this turn.
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cached = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let hit_pct = if input_tokens > 0 {
                        (cached as f64 / input_tokens as f64) * 100.0
                    } else {
                        0.0
                    };
                    tracing::debug!(
                        event = "codex_cache_usage",
                        input_tokens,
                        cached_tokens = cached,
                        fresh_tokens = input_tokens.saturating_sub(cached),
                        cache_hit_pct = format!("{hit_pct:.1}"),
                        "codex prompt-cache effectiveness for this turn"
                    );
                }
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.complete_replay(usage.as_ref());
                self.complete_usage_observation(usage.as_ref());
                self.emit_outcome(usage.as_ref(), 200);
                self.close_block_final(&mut events);
                // A completed response can still carry `incomplete_details`,
                // and truncation outranks a tool call: a `tool_use` stop on a
                // cut-off turn would have the client run a half-streamed call.
                let truncated = chunk
                    .get("response")
                    .and_then(|v| v.get("incomplete_details"))
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str())
                    == Some("max_output_tokens");
                let stop_reason = if truncated {
                    "max_tokens"
                } else if self.saw_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                };
                events.push(self.emit_message_delta(stop_reason));
                events.push(self.emit_message_stop());
            }
            "response.failed" => {
                self.close_block_final(&mut events);
                // Booked as a 500 so the outcome funnel routes it to
                // `record_failed` — a failed turn must not feed the save-rate.
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.emit_outcome(usage.as_ref(), 500);
            }
            "response.incomplete" => {
                if let Some(reason) = chunk
                    .get("response")
                    .and_then(|v| v.get("incomplete_details"))
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str())
                {
                    self.total_output_tokens = self.total_output_tokens.max(0);
                    self.close_block_final(&mut events);
                    let stop_reason = match reason {
                        "max_output_tokens" => "max_tokens",
                        _ => "end_turn",
                    };
                    events.push(self.emit_message_delta(stop_reason));
                    events.push(self.emit_message_stop());
                }
                // Outside the `if let`: a response that stopped short still
                // spent tokens, whether or not it said why.
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.emit_outcome(usage.as_ref(), 200);
            }
            _ => {}
        }

        events
    }

    fn emit_message_start(&self) -> String {
        let raw = uuid::Uuid::new_v4().to_string().replace('-', "");
        let msg_id = format!("msg_{}", &raw[..raw.len().min(24)]);
        crate::sse::outbound::message_start(
            &msg_id,
            &self.model,
            json!({"input_tokens": 0, "output_tokens": 0}),
        )
    }

    fn emit_content_block_start_text(&mut self) -> String {
        crate::sse::outbound::content_block_start(
            self.content_block_index,
            json!({"type": "text", "text": ""}),
        )
    }

    fn emit_content_block_start_thinking(&mut self) -> String {
        crate::sse::outbound::content_block_start(
            self.content_block_index,
            json!({"type": "thinking", "thinking": ""}),
        )
    }

    fn emit_content_block_start_tool(&mut self, id: &str, name: &str) -> String {
        crate::sse::outbound::content_block_start(
            self.content_block_index,
            json!({"type": "tool_use", "id": id, "name": name}),
        )
    }

    fn emit_text_delta(&self, text: &str) -> String {
        crate::sse::outbound::text_delta(self.content_block_index, text)
    }

    fn emit_thinking_delta(&self, thinking: &str) -> String {
        crate::sse::outbound::thinking_delta(self.content_block_index, thinking)
    }

    /// Closes a thinking block by handing the client the reasoning envelope it
    /// will echo back to us next turn.
    fn emit_signature_delta(&self, signature: &str) -> String {
        crate::sse::outbound::signature_delta(self.content_block_index, signature)
    }

    fn emit_input_json_delta(&self, json_str: &str) -> String {
        crate::sse::outbound::input_json_delta(self.content_block_index, json_str)
    }

    fn emit_content_block_stop(&self) -> String {
        crate::sse::outbound::content_block_stop(self.content_block_index)
    }

    fn emit_message_delta(&self, stop_reason: &str) -> String {
        crate::sse::outbound::message_delta(
            stop_reason,
            json!({"output_tokens": self.total_output_tokens}),
        )
    }

    fn emit_message_stop(&self) -> String {
        crate::sse::outbound::message_stop()
    }
}

pub(crate) fn translate_openai_stream_to_anthropic(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    codex_limits: crate::codex_rate_limits::CodexRateLimitStore,
    quota_seen_in_headers: bool,
    outcome: Option<RoutedOutcomeContext>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    let mut translator = StreamTranslator::new(model)
        .with_codex_limits(codex_limits)
        .with_initial_rate_limits_seen(quota_seen_in_headers)
        .with_outcome(outcome);
    let mut buffer = String::new();
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();

    stream.filter_map(move |chunk| {
        let translated = match chunk {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                buffer.push_str(&text);

                let mut output = Vec::new();
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() {
                        let data = current_data.join("\n");
                        let events = translator.process_frame(current_event.as_deref(), &data);
                        for event in events {
                            output.extend_from_slice(event.as_bytes());
                        }
                        current_event = None;
                        current_data.clear();
                        continue;
                    }

                    if let Some(event) = line.strip_prefix("event:") {
                        current_event = Some(event.trim().to_string());
                        continue;
                    }

                    if let Some(data) = line.strip_prefix("data:") {
                        current_data.push(data.trim_start().to_string());
                        continue;
                    }
                }

                if output.is_empty() {
                    None
                } else {
                    Some(Ok(bytes::Bytes::from(output)))
                }
            }
            Err(e) => Some(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))),
        };
        async { translated }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::request::anthropic_to_openai_responses_request;
    use crate::test_support::EventCapture;
    use serde_json::json;
    fn translator_with_outcome(
        model: &str,
        tokens_saved: i64,
    ) -> (
        StreamTranslator,
        std::sync::Arc<crate::request_logger::RequestLogger>,
        std::sync::Arc<headroom_core::cost_tracker::CostTracker>,
    ) {
        redirect_savings_ledger();
        let cost_tracker = std::sync::Arc::new(headroom_core::cost_tracker::CostTracker::new(
            None, "monthly",
        ));
        let request_logger = std::sync::Arc::new(crate::request_logger::RequestLogger::new(None));
        let ctx = RoutedOutcomeContext {
            sink: std::sync::Arc::new(crate::proxy::ProxyOutcomeSink {
                cost_tracker: cost_tracker.clone(),
                savings_tracker: std::sync::Arc::new(
                    headroom_core::savings_tracker::SavingsTracker::new(None, false),
                ),
                request_logger: request_logger.clone(),
            }),
            request_id: "req-test".to_string(),
            replay_store: None,
            usage_observer: None,
            session_key: "sess-test".to_string(),
            model: model.to_string(),
            provider: "openai_responses".to_string(),
            client: None,
            project: None,
            tokens_saved,
            transforms_applied: vec!["ctx_offload".to_string()],
            num_messages: 3,
            started_at: std::time::Instant::now(),
            overhead_ms: 1.5,
            forwarded_tokens_estimate: 777,
            upstream_attempts: 1,
        };
        let t = StreamTranslator::new(model.to_string()).with_outcome(Some(ctx));
        (t, request_logger, cost_tracker)
    }

    #[test]
    fn translator_without_outcome_context_books_nothing() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        t.process_frame(
            Some("response.completed"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}).to_string(),
        );
    }

    fn drive(t: &mut StreamTranslator, frames: &[(&str, &str)]) -> String {
        let mut all = String::new();
        for (event, data) in frames {
            for e in t.process_frame(Some(event), data) {
                all.push_str(&e);
            }
        }
        all
    }

    /// The whole point of the envelope: a reasoning item leaves in the thinking
    /// block's signature and comes back from the client's own history, with no
    /// proxy-side state in between.
    #[test]
    fn truncated_completion_reports_max_tokens() {
        for (frames, expected) in [
            (
                vec![(
                    "response.completed",
                    r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":9}}}"#,
                )],
                "max_tokens",
            ),
            (
                vec![
                    (
                        "response.output_item.added",
                        r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
                    ),
                    (
                        "response.completed",
                        r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"output_tokens":9}}}"#,
                    ),
                ],
                "max_tokens",
            ),
            (
                vec![
                    (
                        "response.output_item.added",
                        r#"{"item":{"type":"function_call","call_id":"c1","name":"Bash"}}"#,
                    ),
                    (
                        "response.completed",
                        r#"{"response":{"usage":{"output_tokens":9}}}"#,
                    ),
                ],
                "tool_use",
            ),
            (
                vec![(
                    "response.completed",
                    r#"{"response":{"usage":{"output_tokens":9}}}"#,
                )],
                "end_turn",
            ),
        ] {
            let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
            let sse = drive(&mut t, &frames);
            assert!(
                sse.contains(&format!(r#""stop_reason":"{expected}""#)),
                "expected {expected}, got: {sse}"
            );
        }
    }

    /// A signature-only block must not collide with a block already open, or
    /// two content blocks share an index and the client sees a torn stream.
    #[test]
    fn signature_only_block_closes_open_text_first() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[
                ("response.output_text.delta", r#"{"delta":"partial"}"#),
                (
                    "response.output_item.done",
                    r#"{"item":{"type":"reasoning","id":"rs_4","summary":[],"encrypted_content":"ENC_4"}}"#,
                ),
            ],
        );
        assert!(signature_from_stream(&sse).is_some());
        let indices: Vec<i64> = sse
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .filter(|e| e["type"] == "content_block_start")
            .filter_map(|e| e["index"].as_i64())
            .collect();
        let mut unique = indices.clone();
        unique.dedup();
        assert_eq!(indices, unique, "two content blocks opened on one index");
    }

    /// An id with no blob (or the reverse) is not replayable, so no signature
    /// is minted and the summary stays a plain thinking block.
    #[test]
    fn incomplete_reasoning_item_emits_no_signature() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_3","summary":[]}}"#,
            )],
        );
        assert!(signature_from_stream(&sse).is_none());
    }

    /// Thinking blocks we did not mint must never become reasoning items: a
    /// real Anthropic signature, or none at all, is dropped on the way out.
    #[test]
    fn stream_translator_translates_reasoning_summary_to_thinking() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let mut all = String::new();
        for (event, data) in [
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":"Consider the"}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":" edge cases"}"#,
            ),
            ("response.reasoning_summary_part.added", r#"{}"#),
            ("response.output_text.delta", r#"{"delta":"Answer"}"#),
            (
                "response.completed",
                r#"{"response":{"usage":{"output_tokens":5}}}"#,
            ),
        ] {
            for e in t.process_frame(Some(event), data) {
                all.push_str(&e);
            }
        }
        assert!(all.contains(r#""type":"thinking","thinking":"""#));
        assert!(all.contains(r#""thinking":"Consider the""#));
        assert!(all.contains(r#""text":"Answer""#));
        assert!(all.contains(r#""stop_reason":"end_turn""#));
    }

    #[test]
    fn stream_translator_translates_responses_function_call_frames() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let mut all = String::new();
        for (event, data) in [
            (
                "response.created",
                r#"{"response":{"model":"gpt-5.6-terra"}}"#,
            ),
            (
                "response.output_item.added",
                r#"{"item":{"type":"function_call","call_id":"call_1","name":"Bash","arguments":""}}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"delta":"{\"command\":"}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"delta":"\"ls\"}"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"function_call","call_id":"call_1","name":"Bash"}}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        ] {
            for e in t.process_frame(Some(event), data) {
                all.push_str(&e);
            }
        }
        assert!(all.contains(r#""type":"tool_use","id":"call_1","name":"Bash""#));
        assert!(all.contains(r#""type":"input_json_delta""#));
        assert!(all.contains(r#"\"command\":"#));
        assert!(all.contains(r#""stop_reason":"tool_use""#));
        assert!(all.contains("message_stop"));
    }

    #[test]
    fn stream_translator_text_only() {
        let mut translator = StreamTranslator::new("test-model".to_string());

        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let chunk2 = r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#;
        let chunk4 = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

        let mut all_events = Vec::new();
        all_events.extend(translator.process_line(chunk1));
        all_events.extend(translator.process_line(chunk2));
        all_events.extend(translator.process_line(chunk3));
        all_events.extend(translator.process_line(chunk4));

        let output = all_events.join("");
        assert!(output.contains("event: message_start"));
        assert!(output.contains("event: content_block_start"));
        assert!(output.contains("\"text_delta\""));
        assert!(output.contains("Hello"));
        assert!(output.contains(" world"));
        assert!(output.contains("event: content_block_stop"));
        assert!(output.contains("event: message_delta"));
        assert!(output.contains("event: message_stop"));
    }

    #[test]
    fn stream_translator_tool_calls() {
        let mut translator = StreamTranslator::new("test-model".to_string());

        let chunk1 = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        let chunk2 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#;
        let chunk3 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"co"}}]},"finish_reason":null}]}"#;
        let chunk4 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mmand\"}"}}]},"finish_reason":null}]}"#;
        let chunk5 = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

        let mut all_events = Vec::new();
        all_events.extend(translator.process_line(chunk1));
        all_events.extend(translator.process_line(chunk2));
        all_events.extend(translator.process_line(chunk3));
        all_events.extend(translator.process_line(chunk4));
        all_events.extend(translator.process_line(chunk5));

        let output = all_events.join("");
        assert!(output.contains("event: message_start"));
        assert!(output.contains("\"tool_use\""));
        assert!(output.contains("bash"));
        assert!(output.contains("\"input_json_delta\""));
        assert!(output.contains("\"stop_reason\":\"tool_use\""));
        assert!(output.contains("event: message_stop"));
    }
    #[test]
    fn reasoning_envelope_round_trips_through_the_client() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[
                (
                    "response.reasoning_summary_text.delta",
                    r#"{"delta":"weighing it"}"#,
                ),
                (
                    "response.output_item.done",
                    r#"{"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC_BLOB"}}"#,
                ),
            ],
        );
        assert!(sse.contains(r#""thinking":"weighing it""#));
        let signature = signature_from_stream(&sse).expect("signature delta emitted");

        // Next turn: the client echoes that thinking block back verbatim.
        let request = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "weighing it", "signature": signature},
                    {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}
                ]}
            ]
        });
        let out = anthropic_to_openai_responses_request(&request, true).unwrap();
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "ENC_BLOB");
        assert_eq!(input[0]["summary"], json!([]));
        // Reasoning has to stay ahead of the call it preceded.
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    /// With reasoning summaries disabled no thinking block is ever opened by a
    /// summary delta, so the item's only carrier is a signature-only block.
    #[test]
    fn reasoning_envelope_survives_when_summaries_are_disabled() {
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"ENC_2"}}"#,
            )],
        );
        assert!(sse.contains(r#""type":"thinking","thinking":"""#));
        let signature = signature_from_stream(&sse).expect("signature emitted without summary");

        let request = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": signature}
            ]}]
        });
        let out = anthropic_to_openai_responses_request(&request, true).unwrap();
        assert_eq!(out["input"][0]["type"], "reasoning");
        assert_eq!(out["input"][0]["encrypted_content"], "ENC_2");
    }

    /// A turn cut off at the token ceiling must say so, even when it completed
    /// and even when it had started a tool call.
    #[test]
    fn reasoning_does_not_leak_across_models() {
        let mut first = StreamTranslator::new("model-a".to_string());
        let sse = drive(
            &mut first,
            &[(
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","id":"rs_a","summary":[],"encrypted_content":"ENC_A"}}"#,
            )],
        );
        let signature = signature_from_stream(&sse).unwrap();

        // A turn on another model that does not echo the block gets nothing.
        let clean = json!({
            "model": "model-b",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "next"}]
        });
        let out = anthropic_to_openai_responses_request(&clean, true).unwrap();
        assert!(!out["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "reasoning"));

        // And what is echoed is carried by the request itself, not a cache.
        let echoed = json!({
            "model": "model-a",
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": signature}
            ]}]
        });
        let out = anthropic_to_openai_responses_request(&echoed, true).unwrap();
        assert_eq!(out["input"][0]["encrypted_content"], "ENC_A");
    }

    #[test]
    fn reasoning_item_id_and_blob_may_arrive_on_separate_events() {
        // `added` announces the id, `done` carries the blob; neither event is
        // complete on its own but the pair is.
        let mut t = StreamTranslator::new("claude-codex-5.6".to_string());
        let sse = drive(
            &mut t,
            &[
                (
                    "response.output_item.added",
                    r#"{"item":{"type":"reasoning","id":"rs_split","summary":[]}}"#,
                ),
                (
                    "response.output_item.done",
                    r#"{"item":{"type":"reasoning","summary":[],"encrypted_content":"ENC_SPLIT"}}"#,
                ),
            ],
        );
        let signature = signature_from_stream(&sse).expect("signature from the merged pair");
        let request = json!({
            "model": "claude-codex-5.6",
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": signature}
            ]}]
        });
        let out = anthropic_to_openai_responses_request(&request, true).unwrap();
        assert_eq!(out["input"][0]["id"], "rs_split");
        assert_eq!(out["input"][0]["encrypted_content"], "ENC_SPLIT");
    }
    #[test]
    fn stream_end_emits_one_joinable_missing_quota_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let (translator, _logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        let capture = EventCapture::default();
        let lines = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        tracing::subscriber::with_default(subscriber, || {
            let mut translator =
                translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
            translator.finish_rate_limit_observation();
            translator.finish_rate_limit_observation();
        });

        let joined = lines.lock().unwrap().join("\n");
        let missing: Vec<_> = joined
            .lines()
            .filter(|line| line.contains("codex_rate_limits_missing"))
            .collect();
        assert_eq!(missing.len(), 1, "{joined}");
        assert!(missing[0].contains("request_id=req-test"), "{joined}");
    }

    #[test]
    fn observed_stream_quota_suppresses_the_missing_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let (translator, _logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        let capture = EventCapture::default();
        let lines = capture.0.clone();
        let subscriber = tracing_subscriber::registry().with(capture);
        tracing::subscriber::with_default(subscriber, || {
            let mut translator =
                translator.with_codex_limits(crate::codex_rate_limits::CodexRateLimitStore::new());
            translator.process_frame(
                Some("response.created"),
                &json!({"rate_limits": {"primary": {"used_percent": 4}}}).to_string(),
            );
            translator.finish_rate_limit_observation();
        });

        let joined = lines.lock().unwrap().join("\n");
        assert!(!joined.contains("codex_rate_limits_missing"), "{joined}");
    }

    /// The gap this closes: routed traffic used to reach no tracker at all, so
    /// codex spend was invisible in /stats and the dashboard.
    // Async because a saving > 0 reaches `record_savings_ledger`, which pushes
    // the flocked disk append onto a blocking thread.
    #[tokio::test]
    async fn completed_responses_stream_books_a_request_outcome() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 400);
        t.process_frame(
            Some("response.completed"),
            &json!({
                "response": {
                    "usage": {
                        "input_tokens": 10_000,
                        "output_tokens": 250,
                        "input_tokens_details": {"cached_tokens": 9_000}
                    }
                }
            })
            .to_string(),
        );

        let entries = logger.get_recent(10);
        assert_eq!(entries.len(), 1, "the turn should be booked exactly once");
        let e = &entries[0];
        assert_eq!(e.model, "gpt-5.6-luna");
        assert_eq!(e.provider, "openai_responses");
        assert_eq!(e.output_tokens, 250);
        // Forwarded size is what upstream counted; the original is that plus
        // what the transforms removed.
        assert_eq!(e.input_tokens_optimized, 10_000);
        assert_eq!(e.input_tokens_original, 10_400);
        assert_eq!(e.tokens_saved, 400);
        assert!(e.cache_hit, "9k of 10k input tokens were served from cache");
        assert_eq!(e.transforms_applied, vec!["ctx_offload".to_string()]);
    }

    /// A stream carrying both a terminal event and a trailing `[DONE]`, plus
    /// the drop at the end, must still book exactly one turn.
    #[test]
    fn a_turn_is_booked_only_once() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        t.process_frame(
            Some("response.completed"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}).to_string(),
        );
        t.process_frame(None, "[DONE]");
        drop(t);
        assert_eq!(logger.get_recent(10).len(), 1);
    }

    /// A turn cut off before any terminal event still spent tokens, and the
    /// Claude path books those too.
    #[test]
    fn dropped_stream_still_books_the_turn() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 0);
        t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "partial"}).to_string(),
        );
        assert_eq!(logger.get_recent(10).len(), 0, "not booked mid-stream");
        drop(t);
        assert_eq!(
            logger.get_recent(10).len(),
            1,
            "dropping the translator books the interrupted turn"
        );
    }

    /// `response.failed` routes to `record_failed`, which deliberately skips
    /// the success funnel — a failed turn must not inflate the save rate.
    #[test]
    fn failed_response_is_not_logged_as_a_served_request() {
        let (mut t, logger, _cost) = translator_with_outcome("gpt-5.6-luna", 100);
        t.process_frame(
            Some("response.failed"),
            &json!({"response": {"error": {"message": "boom"}}}).to_string(),
        );
        assert_eq!(logger.get_recent(10).len(), 0);
    }

    /// Chat Completions delivers usage on its own chunk rather than a terminal
    /// event, so the numbers have to survive until the stream ends.
    #[test]
    fn chat_completions_usage_is_booked_at_stream_end() {
        let (mut t, logger, _cost) = translator_with_outcome("qwen-local", 0);
        t.process_frame(
            None,
            &json!({
                "choices": [{"delta": {"content": "hi"}}],
                "usage": {"prompt_tokens": 700, "completion_tokens": 20,
                          "prompt_tokens_details": {"cached_tokens": 500}}
            })
            .to_string(),
        );
        t.process_frame(None, "[DONE]");

        let entries = logger.get_recent(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input_tokens_optimized, 700);
        assert_eq!(entries[0].output_tokens, 20);
    }

    /// Unit tests elsewhere in this file build translators with no outcome
    /// context; that must stay a no-op rather than panicking on drop.
    fn redirect_savings_ledger() {
        static LEDGER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let path = LEDGER.get_or_init(|| {
            let dir = std::mem::ManuallyDrop::new(tempfile::tempdir().expect("tempdir"));
            dir.path().join("savings_events.jsonl")
        });
        std::env::set_var("HEADROOM_SAVINGS_EVENTS_PATH", path);
    }

    fn signature_from_stream(sse: &str) -> Option<String> {
        for line in sse.lines() {
            // Skip the `event:` and blank lines that frame each SSE record.
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if event["delta"]["type"] == "signature_delta" {
                return event["delta"]["signature"].as_str().map(String::from);
            }
        }
        None
    }
}
