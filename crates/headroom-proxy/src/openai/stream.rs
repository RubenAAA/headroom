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
        if self.outcome.is_some() && !self.observation_completed && self.last_usage.is_some() {
            // A stream that died mid-flight still billed whatever the provider
            // last reported. Booking it here is what keeps a dropped stream
            // from leaving its tokens out of the cache-health totals.
            //
            // Only when usage actually arrived. With none, every counter would
            // be a zero this code invented, and a fabricated turn class is
            // worse than the pending entry the LRU is about to evict.
            let usage = self.last_usage.clone();
            self.complete_usage_observation(usage.as_ref());
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
    /// Whether any `output_text.delta` arrived for the message item currently
    /// streaming. The `output_item.done` event carries the finished item, and
    /// when the upstream sent no deltas that copy is the only one there is.
    saw_text_delta: bool,
    /// Same, for `response.refusal.delta` on the current item.
    saw_refusal_delta: bool,
    /// Whether any `function_call_arguments.delta` arrived for the tool call
    /// currently streaming. `arguments.done` carries the whole arguments and
    /// replays them when no delta did.
    saw_arg_delta: bool,
    /// Set once the turn was closed early (`abort_terminal`): any straggler
    /// frames after a transport error must not reopen it.
    terminated: bool,
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
    /// Whether the CTX-7 usage observation parked by `begin_request` has been
    /// closed. Separate from `outcome_emitted`: the outcome and the observation
    /// are booked by different code on different events, and either can fire
    /// without the other.
    observation_completed: bool,
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
            saw_text_delta: false,
            saw_refusal_delta: false,
            saw_arg_delta: false,
            terminated: false,
            pending_reasoning: PendingReasoning::default(),
            codex_limits: None,
            codex_rate_limits_seen: false,
            codex_rate_limits_finished: false,
            outcome: None,
            outcome_emitted: false,
            observation_completed: false,
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
    fn complete_usage_observation(&mut self, usage: Option<&Value>) {
        if self.observation_completed {
            return;
        }
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
        let cache_details = usage.and_then(|u| {
            u.get("input_tokens_details")
                .or_else(|| u.get("prompt_tokens_details"))
        });
        let cache_read = cache_details
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        // A missing `cached_tokens` field is "the provider reported no cache
        // data", not a miss: counting it as zero drags the fleet hit rate down
        // on providers with no cache telemetry.
        let cache_capable = cache_details.and_then(|d| d.get("cached_tokens")).is_some();
        // Both OpenAI shapes report an `input_tokens` that *includes* the
        // cached prefix; the observer's counter is Anthropic-shaped and
        // excludes it, and it adds `cache_read` back on to form the
        // denominator. Passing the provider's number straight through counts
        // the prefix twice and drags every routed turn's hit rate down.
        let provider_input = get("input_tokens").max(get("prompt_tokens"));
        observer.note_output_tokens(&ctx.request_id, self.total_output_tokens);
        let class = observer.complete_with_cache_capability(
            &ctx.request_id,
            provider_input.saturating_sub(cache_read),
            cache_read,
            0,
            // The Responses API publishes no cache-creation counter at all, so
            // there is no TTL breakdown to split — `None`, not a pair of zeros,
            // which would claim this endpoint wrote nothing at either tier.
            None,
            cache_capable,
        );
        self.observation_completed = true;
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

        if self.terminated {
            return events;
        }

        if data.trim().is_empty() || data.trim() == "[DONE]" {
            if data.trim() == "[DONE]" {
                // Last chance to book the turn: Chat Completions has no
                // terminal event, and a Responses stream can be cut off before
                // one arrives. No-op when a terminal event already booked it.
                let usage = self.last_usage.clone();
                self.emit_outcome(usage.as_ref(), 200);
                // The observation needs the same last-chance close. Only
                // `response.completed` closed it before, and that event exists
                // only in the Responses API — every Chat Completions turn (all
                // of the cost-routed traffic) left its pending entry to be
                // evicted from the LRU unclassified, so routed turns never
                // reached the cache-health counters at all.
                self.complete_usage_observation(usage.as_ref());
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
                    self.saw_text_delta = true;
                }
            }
            // `output_text.done` carries the whole text. Normally the deltas
            // above already delivered it and this is a marker; when the
            // upstream sent no deltas it is the only copy, mirroring the
            // message-item recovery below.
            "response.output_text.done" | "output_text.done" => {
                if !self.saw_text_delta {
                    if let Some(text) = chunk.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            self.open_block(OpenBlock::Text, &mut events);
                            events.push(self.emit_text_delta(text));
                            self.close_block(&mut events);
                            self.saw_text_delta = true;
                        }
                    }
                }
            }
            // A refusal is the turn's only text. Anthropic has no refusal
            // block, so it rides as a text block; without this the client
            // receives an empty `end_turn` it cannot tell from silence.
            "response.refusal.delta" | "refusal.delta" => {
                let delta = chunk.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if !delta.is_empty() {
                    self.open_block(OpenBlock::Text, &mut events);
                    events.push(self.emit_text_delta(delta));
                    self.saw_refusal_delta = true;
                }
            }
            "response.refusal.done" | "refusal.done" => {
                if !self.saw_refusal_delta {
                    if let Some(refusal) = chunk.get("refusal").and_then(|v| v.as_str()) {
                        if !refusal.is_empty() {
                            self.open_block(OpenBlock::Text, &mut events);
                            events.push(self.emit_text_delta(refusal));
                            self.close_block(&mut events);
                            self.saw_refusal_delta = true;
                        }
                    }
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
                    // Per call, not per stream: `arguments.done` below may
                    // only replay the full arguments when no delta arrived
                    // for this call.
                    self.saw_arg_delta = false;
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
                            self.saw_arg_delta = true;
                        }
                    }
                }
                // A delta arriving before `output_item.added` (no open tool
                // block to attribute it to) is skipped rather than guessed
                // at: the `arguments.done` fallback below replays the whole
                // arguments, so nothing is lost.
            }
            // `arguments.done` carries the whole arguments string. Normally
            // the deltas above already delivered it; when they did not — or
            // arrived before the item announced itself — this is the only
            // copy, mirroring the message-item recovery.
            "response.function_call_arguments.done" => {
                if self.open == Some(OpenBlock::Tool) && !self.saw_arg_delta {
                    if let Some(args) = chunk.get("arguments").and_then(|v| v.as_str()) {
                        if !args.is_empty() {
                            events.push(self.emit_input_json_delta(args));
                            self.saw_arg_delta = true;
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
                // A finished message item carries the whole answer. Normally
                // we have already streamed it delta by delta and this is a
                // no-op, but a reasoning delivery that sends the message whole
                // emits no deltas at all -- and then this event holds the only
                // copy. Dropping it hands the client a turn containing a
                // thought and nothing else, which Claude Code renders as a
                // stopped turn and answers with "your previous response had no
                // visible output": the model is fine, the text was lost here.
                if item_type == Some("message") {
                    if !self.saw_text_delta && !self.saw_refusal_delta {
                        let text: String = chunk
                            .get("item")
                            .and_then(|i| i.get("content"))
                            .and_then(|c| c.as_array())
                            .map(|blocks| {
                                blocks
                                    .iter()
                                    .filter_map(|b| {
                                        b.get("text")
                                            .and_then(|t| t.as_str())
                                            .or_else(|| b.get("refusal").and_then(|r| r.as_str()))
                                    })
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        if !text.is_empty() {
                            tracing::debug!(
                                event = "codex_message_without_deltas",
                                chars = text.len(),
                                "recovered a message item the upstream never streamed"
                            );
                            self.open_block(OpenBlock::Text, &mut events);
                            events.push(self.emit_text_delta(&text));
                            self.close_block(&mut events);
                        }
                    }
                    // Per item, not per stream: a second message must be
                    // judged on its own deltas.
                    self.saw_text_delta = false;
                    self.saw_refusal_delta = false;
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
                // The turn still has to end on the wire: without terminal
                // events the client hangs, and the `[DONE]` fallback cannot
                // rescue it — the block above already closed `open`, which is
                // the fallback's trigger. `end_turn`, never `tool_use`: a
                // half-streamed call must not run.
                events.push(self.emit_message_delta("end_turn"));
                events.push(self.emit_message_stop());
            }
            "response.incomplete" => {
                let reason = chunk
                    .get("response")
                    .and_then(|v| v.get("incomplete_details"))
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str());
                self.close_block_final(&mut events);
                let stop_reason = match reason {
                    Some("max_output_tokens") => "max_tokens",
                    _ => "end_turn",
                };
                events.push(self.emit_message_delta(stop_reason));
                events.push(self.emit_message_stop());
                // Outside the reason: a response that stopped short still
                // spent tokens, whether or not it said why.
                let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
                self.emit_outcome(usage.as_ref(), 200);
            }
            other => {
                // Silence here is how the message-item gap stayed hidden: an
                // event we do not translate is content the client never sees.
                // Log the name so the next one is a grep, not an investigation.
                if other.starts_with("response.") {
                    tracing::debug!(
                        event = "codex_unhandled_stream_event",
                        stream_event = other,
                        "no translation for this Responses event; nothing emitted"
                    );
                }
            }
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

    /// Terminal events for a turn whose upstream died mid-stream.
    ///
    /// Deliberately NOT a complete close: no `message_stop`, and never a
    /// `content_block_stop` for a half-streamed tool call. The translated
    /// stream always runs under `sse::stream_finisher::finish_on_drop`
    /// downstream, which owns the close — it withholds the partial tool
    /// block, names it in the truncation marker, and ends `end_turn`.
    /// Closing the tool block here would look complete downstream and the
    /// call could run on truncated input; stopping the message here would
    /// read as a clean finish with no marker. Empty when nothing started —
    /// no `message_start` went out, so there is nothing to close and the
    /// caller propagates the transport error instead. `end_turn`, never
    /// `tool_use`: a half-streamed call must not run.
    fn abort_terminal(&mut self) -> Vec<String> {
        if !self.started {
            return Vec::new();
        }
        self.terminated = true;
        let mut events = Vec::new();
        if self.open != Some(OpenBlock::Tool) {
            self.close_block_final(&mut events);
        }
        events.push(self.emit_message_delta("end_turn"));
        events
    }
}

/// Carried across polls by the [`translate_openai_stream_to_anthropic`]
/// adapter below.
struct TranslateState<S> {
    upstream: S,
    translator: StreamTranslator,
    buffer: String,
    current_event: Option<String>,
    current_data: Vec<String>,
    /// Set once the upstream has failed. A reqwest stream that has yielded an
    /// error yields the same error on every later poll, so without this the
    /// fold below would re-close the turn and re-log forever.
    finished: bool,
}

impl<S> TranslateState<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    /// Fold one upstream chunk into SSE lines, dispatching each blank-line
    /// terminated frame. Appends emitted bytes to `output`.
    fn push_chunk(&mut self, text: &str, output: &mut Vec<u8>) {
        self.buffer.push_str(text);
        while let Some(newline_pos) = self.buffer.find('\n') {
            let line = self.buffer[..newline_pos]
                .trim_end_matches('\r')
                .to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();
            Self::push_line(
                &line,
                &mut self.translator,
                &mut self.current_event,
                &mut self.current_data,
                output,
            );
        }
    }

    fn push_line(
        line: &str,
        translator: &mut StreamTranslator,
        current_event: &mut Option<String>,
        current_data: &mut Vec<String>,
        output: &mut Vec<u8>,
    ) {
        if line.is_empty() {
            let data = current_data.join("\n");
            for event in translator.process_frame(current_event.as_deref(), &data) {
                output.extend_from_slice(event.as_bytes());
            }
            *current_event = None;
            current_data.clear();
            return;
        }
        if let Some(event) = line.strip_prefix("event:") {
            *current_event = Some(event.trim().to_string());
            return;
        }
        if let Some(data) = line.strip_prefix("data:") {
            current_data.push(data.trim_start().to_string());
        }
    }

    /// Dispatch whatever the upstream left behind: a trailing line without
    /// its newline, then a final frame without its terminating blank line.
    /// Without this the last event — potentially `response.completed`
    /// itself, i.e. the usage, the stop reason, and the client's
    /// `message_stop` — is silently dropped whenever a stream ends
    /// mid-frame.
    fn flush_trailing(&mut self, output: &mut Vec<u8>) {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            for line in rest.split('\n') {
                Self::push_line(
                    line.trim_end_matches('\r'),
                    &mut self.translator,
                    &mut self.current_event,
                    &mut self.current_data,
                    output,
                );
            }
        }
        if self.current_event.is_some() || !self.current_data.is_empty() {
            let data = self.current_data.join("\n");
            for event in self
                .translator
                .process_frame(self.current_event.as_deref(), &data)
            {
                output.extend_from_slice(event.as_bytes());
            }
            self.current_event = None;
            self.current_data.clear();
        }
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

    let translator = StreamTranslator::new(model)
        .with_codex_limits(codex_limits)
        .with_initial_rate_limits_seen(quota_seen_in_headers)
        .with_outcome(outcome);

    futures_util::stream::unfold(
        TranslateState {
            upstream: stream,
            translator,
            buffer: String::new(),
            current_event: None,
            current_data: Vec::new(),
            finished: false,
        },
        |mut state| async move {
            if state.finished {
                return None;
            }
            loop {
                match state.upstream.next().await {
                    Some(Ok(bytes)) => {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        let mut output = Vec::new();
                        state.push_chunk(&text, &mut output);
                        if !output.is_empty() {
                            return Some((Ok(bytes::Bytes::from(output)), state));
                        }
                    }
                    Some(Err(e)) => {
                        // The client already holds part of this turn, so there
                        // is no fallback to be had: re-dispatching now would
                        // splice a second upstream's events onto a half-finished
                        // message. The abort below closes text/thinking and
                        // downgrades to `end_turn` but deliberately leaves the
                        // message unstopped and any half-streamed tool call
                        // unclosed — `finish_on_drop` downstream owns that
                        // close (partial tool discarded with a named marker).
                        // Either way this turn is over: the upstream will hand
                        // back the same error for as long as it is polled.
                        tracing::warn!(
                            event = "routed_stream_aborted",
                            error = %e,
                            "routed upstream stream failed after the client had events"
                        );
                        let terminal = state.translator.abort_terminal();
                        // Either way this turn is over: the upstream will hand
                        // back the same error for as long as it is polled.
                        state.finished = true;
                        if terminal.is_empty() {
                            // Nothing started: no `message_start` went out, so
                            // there is no turn to close — propagate the
                            // transport error.
                            return Some((Err(std::io::Error::other(e.to_string())), state));
                        }
                        let mut output = Vec::new();
                        for event in terminal {
                            output.extend_from_slice(event.as_bytes());
                        }
                        return Some((Ok(bytes::Bytes::from(output)), state));
                    }
                    None => {
                        let mut output = Vec::new();
                        state.flush_trailing(&mut output);
                        state.finished = true;
                        if output.is_empty() {
                            return None;
                        }
                        return Some((Ok(bytes::Bytes::from(output)), state));
                    }
                }
            }
        },
    )
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
            reroute: None,
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
            redact_store: None,
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

    /// The bug behind "Muse Spark randomly stops": a reasoning turn arrived as
    /// a reasoning item plus a finished message item, with no
    /// `output_text.delta` for the message. We translated the reasoning into a
    /// thinking block and dropped the message, so the client got a turn made of
    /// a thought and nothing else -- which Claude Code renders as a stopped
    /// turn and answers with "your previous response had no visible output".
    #[test]
    fn a_message_item_that_never_streamed_deltas_still_reaches_the_client() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(
            t.process_frame(
                Some("response.output_item.done"),
                &json!({
                    "item": {
                        "type": "message",
                        "content": [{"type": "output_text", "text": "the answer is 391"}]
                    }
                })
                .to_string(),
            ),
        );
        out.extend(t.process_frame(
            Some("response.completed"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
        ));

        let joined = out.join("");
        assert!(
            joined.contains("the answer is 391"),
            "the only copy of the answer was dropped: {joined}"
        );
        assert!(
            joined.contains("\"type\":\"text\""),
            "it has to arrive as a text block, not a thought: {joined}"
        );
    }

    /// The other half: when deltas did arrive, the `done` event is a summary of
    /// what the client already has and must not be replayed on top of it.
    #[test]
    fn a_message_item_that_streamed_is_not_sent_twice() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "the answer is 391"}).to_string(),
        ));
        out.extend(
            t.process_frame(
                Some("response.output_item.done"),
                &json!({
                    "item": {
                        "type": "message",
                        "content": [{"type": "output_text", "text": "the answer is 391"}]
                    }
                })
                .to_string(),
            ),
        );

        let joined = out.join("");
        assert_eq!(
            joined.matches("the answer is 391").count(),
            1,
            "the streamed text came through twice: {joined}"
        );
    }

    /// Two messages in one response are judged separately: the second having
    /// no deltas must not be silenced by the first having had some.
    #[test]
    fn each_message_item_is_judged_on_its_own_deltas() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "first"}).to_string(),
        ));
        let done = json!({
            "item": {"type": "message", "content": [{"type": "output_text", "text": "first"}]}
        })
        .to_string();
        out.extend(t.process_frame(Some("response.output_item.done"), &done));
        out.extend(t.process_frame(
            Some("response.output_item.done"),
            &json!({
                "item": {"type": "message", "content": [{"type": "output_text", "text": "second"}]}
            })
            .to_string(),
        ));

        let joined = out.join("");
        assert_eq!(joined.matches("first").count(), 1, "{joined}");
        assert!(joined.contains("second"), "{joined}");
    }

    /// A failed turn still has to end on the wire. Before, `response.failed`
    /// booked the outcome but emitted no terminal events, and the `[DONE]`
    /// fallback could not rescue it (the block was already closed, which is
    /// the fallback's trigger) — so the client hung.
    #[test]
    fn a_failed_turn_still_terminates() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "partial"}).to_string(),
        ));
        out.extend(t.process_frame(
            Some("response.failed"),
            &json!({"response": {}}).to_string(),
        ));
        out.extend(t.process_frame(None, "[DONE]"));

        let joined = out.join("");
        assert!(
            joined.contains("message_stop"),
            "failed turn left the client hanging: {joined}"
        );
        assert!(
            joined.contains("end_turn"),
            "a half-streamed call must not run as tool_use: {joined}"
        );
        assert_eq!(
            joined.matches("event: message_stop").count(),
            1,
            "the [DONE] fallback must not double-terminate: {joined}"
        );
    }

    /// `response.incomplete` without a reason still ended the turn upstream;
    /// the client must see terminal events, not silence plus an outcome.
    #[test]
    fn an_incomplete_turn_without_a_reason_still_terminates() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(t.process_frame(
            Some("response.incomplete"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
        ));

        let joined = out.join("");
        assert!(
            joined.contains("message_stop"),
            "reason-less incomplete left the client hanging: {joined}"
        );
    }

    /// A stream ending mid-frame — no trailing blank line, no trailing
    /// newline — must still deliver its last event. Before, the adapter only
    /// dispatched on blank lines, so a cut-off `response.completed` took the
    /// usage, the stop reason, and the client's `message_stop` with it.
    #[tokio::test]
    async fn trailing_frame_without_terminator_is_flushed() {
        use futures_util::{stream, StreamExt};
        redirect_savings_ledger();
        let sse = concat!(
            "event: response.created\n",
            "data: {}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hi\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}",
        );
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(sse))];
        let translated = translate_openai_stream_to_anthropic(
            stream::iter(chunks),
            "muse-spark-1.3".to_string(),
            crate::codex_rate_limits::CodexRateLimitStore::new(),
            false,
            None,
        );
        let out: Vec<String> = translated
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await;
        let joined = out.join("");
        assert!(joined.contains("hi"), "delta lost: {joined}");
        assert!(
            joined.contains("event: message_stop"),
            "unterminated completed dropped the turn end: {joined}"
        );
    }

    /// A reqwest stream that has yielded an error yields that same error on
    /// every later poll. The fold used to close the turn and keep the stream
    /// alive, so it re-closed and re-logged as fast as the executor allowed:
    /// on 2026-09-10 that wrote a 59 GB log at 55 MB/s and grew the proxy by
    /// 3.2 GB a minute until the machine's OOM killer took it. The upstream
    /// must not be polled again once it has failed.
    #[tokio::test]
    async fn a_failed_upstream_is_never_polled_again() {
        use futures_util::StreamExt;
        redirect_savings_ledger();
        // A real `reqwest::Error`, made without touching the network.
        let err = reqwest::Client::new()
            .get("http://")
            .send()
            .await
            .expect_err("no host in the url");
        let started = concat!(
            "event: response.created\n",
            "data: {}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hi\"}\n",
            "\n",
        );
        let mut step = 0;
        let mut err = Some(err);
        let upstream = futures_util::stream::poll_fn(move |_| {
            step += 1;
            std::task::Poll::Ready(match step {
                1 => Some(Ok(bytes::Bytes::from(started))),
                2 => Some(Err(err.take().expect("one error"))),
                _ => panic!("upstream polled after it failed"),
            })
        });
        let translated = translate_openai_stream_to_anthropic(
            Box::pin(upstream),
            "muse-spark-1.3".to_string(),
            crate::codex_rate_limits::CodexRateLimitStore::new(),
            false,
            None,
        );
        let out: Vec<String> = translated
            .map(|r| String::from_utf8(r.unwrap().to_vec()).unwrap())
            .collect()
            .await;
        let joined = out.join("");
        assert!(joined.contains("hi"), "delta lost: {joined}");
        // The translator leaves the turn unstopped on purpose: the stop
        // (and the truncation marker) is `finish_on_drop`'s job downstream.
        // A `message_stop` here would read as a clean finish with no marker.
        assert!(
            !joined.contains("event: message_stop"),
            "abort stopped the turn; the finisher would read it as clean: {joined}"
        );
        assert!(
            joined.contains("end_turn"),
            "abort must downgrade to end_turn: {joined}"
        );
        // And the finisher does close it, exactly once, marked truncated.
        let closed: Vec<String> = crate::sse::stream_finisher::finish_on_drop(
            futures_util::stream::iter(
                out.into_iter()
                    .map(|s| Ok::<_, std::io::Error>(bytes::Bytes::from(s))),
            ),
            "test".to_string(),
        )
        .map(|r| {
            String::from_utf8(
                r.expect("finisher must not error on an aborted turn")
                    .to_vec(),
            )
            .unwrap()
        })
        .collect()
        .await;
        let shut = closed.join("");
        assert!(
            shut.contains("dropped mid-response"),
            "no truncation marker; the turn reads as finished: {shut}"
        );
        assert_eq!(
            shut.matches("event: message_stop").count(),
            1,
            "the turn must be closed exactly once: {shut}"
        );
    }

    /// A refusal is the turn's only text and must reach the client as text,
    /// not vanish into an empty `end_turn`.
    #[test]
    fn a_refused_turn_reaches_the_client_as_text() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(t.process_frame(
            Some("response.refusal.delta"),
            &json!({"delta": "I cannot"}).to_string(),
        ));
        out.extend(t.process_frame(
            Some("response.refusal.done"),
            &json!({"refusal": "I cannot do that"}).to_string(),
        ));
        out.extend(t.process_frame(
            Some("response.completed"),
            &json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 9}}}).to_string(),
        ));

        let joined = out.join("");
        assert!(
            joined.contains("I cannot do that") || joined.contains("I cannot"),
            "refusal never reached the client: {joined}"
        );
        assert_eq!(
            joined.matches("I cannot").count(),
            1,
            "refusal streamed twice: {joined}"
        );
    }

    /// `arguments.done` carries the whole arguments string: when no argument
    /// delta arrived for the call, it is the only copy.
    #[test]
    fn a_tool_call_without_argument_deltas_uses_the_done_fallback() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(
            t.process_frame(
                Some("response.output_item.added"),
                &json!({"item": {"type": "function_call", "call_id": "c1", "name": "bash"}})
                    .to_string(),
            ),
        );
        out.extend(t.process_frame(
            Some("response.function_call_arguments.done"),
            &json!({"arguments": "{\"command\":\"ls\"}"}).to_string(),
        ));
        out.extend(
            t.process_frame(
                Some("response.output_item.done"),
                &json!({"item": {"type": "function_call", "call_id": "c1", "name": "bash"}})
                    .to_string(),
            ),
        );

        let joined = out.join("");
        assert!(
            joined.contains("ls"),
            "arguments lost when deltas never arrived: {joined}"
        );
    }

    /// A transport error after the turn started must leave the close to
    /// `finish_on_drop` downstream: no `message_stop` here (a stopped turn
    /// reads as cleanly finished, with no marker), and straggler frames
    /// after the abort must not reopen it. Before anything started there is
    /// no turn to close, so the abort is empty and the error propagates.
    #[test]
    fn an_aborted_turn_stays_open_for_the_finisher_and_stays_closed() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        assert!(t.abort_terminal().is_empty());
        t.process_frame(Some("response.created"), &json!({}).to_string());
        t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "partial"}).to_string(),
        );
        let term = t.abort_terminal().join("");
        assert!(
            term.contains("end_turn"),
            "half-streamed call must not run: {term}"
        );
        assert!(
            !term.contains("event: message_stop"),
            "abort stopped the turn; the finisher would read it as clean: {term}"
        );
        let after = t
            .process_frame(
                Some("response.output_text.delta"),
                &json!({"delta": "late"}).to_string(),
            )
            .join("");
        assert!(
            !after.contains("late"),
            "straggler reopened the turn: {after}"
        );
    }

    /// A half-streamed tool call must reach the finisher unclosed: a
    /// `content_block_stop` here would look complete downstream and the call
    /// could run on truncated input. The finisher withholds the partial
    /// block and names it in the marker instead.
    #[test]
    fn an_aborted_tool_call_stays_open_for_the_finisher() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        t.process_frame(Some("response.created"), &json!({}).to_string());
        t.process_frame(
            Some("response.output_item.added"),
            &json!({"item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}})
                .to_string(),
        );
        t.process_frame(
            Some("response.function_call_arguments.delta"),
            &json!({"delta": "{\"comma"}).to_string(),
        );
        let term = t.abort_terminal().join("");
        assert!(
            term.contains("end_turn"),
            "half-streamed call must not run: {term}"
        );
        assert!(
            !term.contains("content_block_stop"),
            "abort closed the partial tool call; it would run truncated: {term}"
        );
        assert!(
            !term.contains("event: message_stop"),
            "abort stopped the turn; the finisher would read it as clean: {term}"
        );
    }

    /// End to end across the seam: translator abort frames through
    /// `finish_on_drop` must come out marked truncated, ended `end_turn`,
    /// stopped exactly once — with the partial tool input withheld, never
    /// run. This is the contract the abort shape above exists for; it fails
    /// if either side drifts (translator stopping the turn, finisher
    /// releasing the partial call).
    #[test]
    fn aborted_routed_turn_closes_marked_through_the_finisher() {
        use futures_util::StreamExt;
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut frames: Vec<String> = Vec::new();
        frames.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        frames.extend(t.process_frame(
            Some("response.output_text.delta"),
            &json!({"delta": "working on it"}).to_string(),
        ));
        frames.extend(
            t.process_frame(
                Some("response.output_item.added"),
                &json!({"item": {"type": "function_call", "call_id": "call_9", "name": "Read"}})
                    .to_string(),
            ),
        );
        frames.extend(t.process_frame(
            Some("response.function_call_arguments.delta"),
            &json!({"delta": "{\"path\""}).to_string(),
        ));
        frames.extend(t.abort_terminal());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = frames
            .into_iter()
            .map(|s| Ok(bytes::Bytes::from(s)))
            .collect();
        let out = rt.block_on(async {
            let mut acc = String::new();
            let mut s = Box::pin(crate::sse::stream_finisher::finish_on_drop(
                futures_util::stream::iter(chunks),
                "test".to_string(),
            ));
            while let Some(item) = s.next().await {
                let b = item.expect("finisher must not error on an aborted turn");
                acc.push_str(&String::from_utf8_lossy(&b));
            }
            acc
        });
        assert!(
            out.contains("did NOT run"),
            "no truncation marker; the turn reads as finished: {out}"
        );
        assert!(
            out.contains("`Read`"),
            "marker must name the discarded call so the model can re-issue it: {out}"
        );
        assert!(
            !out.contains("{\"path\""),
            "partial tool input reached the client and could run truncated: {out}"
        );
        assert!(
            out.contains("\"stop_reason\":\"end_turn\""),
            "turn must end end_turn, never tool_use: {out}"
        );
        assert_eq!(
            out.matches("event: message_stop").count(),
            1,
            "turn must stop exactly once: {out}"
        );
    }

    /// `output_text.done` carries the whole text: when no delta arrived for
    /// the item, it is the only copy.
    #[test]
    fn a_text_part_without_deltas_uses_the_done_fallback() {
        let mut t = StreamTranslator::new("muse-spark-1.3".to_string());
        let mut out = Vec::new();
        out.extend(t.process_frame(Some("response.created"), &json!({}).to_string()));
        out.extend(t.process_frame(
            Some("response.output_text.done"),
            &json!({"text": "the answer is 391"}).to_string(),
        ));
        out.extend(
            t.process_frame(
                Some("response.output_item.done"),
                &json!({
                    "item": {
                        "type": "message",
                        "content": [{"type": "output_text", "text": "the answer is 391"}]
                    }
                })
                .to_string(),
            ),
        );

        let joined = out.join("");
        assert_eq!(
            joined.matches("the answer is 391").count(),
            1,
            "text lost or doubled without deltas: {joined}"
        );
    }
}
