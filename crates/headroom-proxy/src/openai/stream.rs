//! Streaming OpenAI → Anthropic translation.
//!
//! `StreamTranslator` holds the per-turn state that an Anthropic SSE stream
//! needs but the OpenAI wire formats do not carry: which content block is
//! open, what index it has, and how much of the reasoning envelope has been
//! emitted. Dropping it books the turn, so a client disconnect mid-stream
//! still reaches the cost tracker.

use crate::handlers::reasoning_signature::{PendingReasoning, encode_reasoning_signature};
use crate::routed::outcome::{RoutedOutcomeContext, book_routed_outcome};
use serde_json::{Value, json};

/// Sightings of one untranslated event name between log lines.
const UNHANDLED_LOG_EVERY: u64 = 500;
/// Distinct names counted. A provider inventing names cannot grow the map.
const UNHANDLED_NAMES: usize = 64;

/// Running count of `name` since the proxy started; `u64::MAX` once the name
/// table is full, which never logs.
fn count_unhandled_event(name: &str) -> u64 {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    let Ok(mut seen) = SEEN.get_or_init(Default::default).lock() else {
        return u64::MAX;
    };
    if !seen.contains_key(name) && seen.len() >= UNHANDLED_NAMES {
        return u64::MAX;
    }
    let n = seen.entry(name.to_string()).or_insert(0);
    *n += 1;
    *n
}

/// Characters of a string value kept in a logged event shape.
const SHAPE_STRING: usize = 40;
/// Longest shape line logged. Sized so the largest interesting shape
/// (`response.completed` with its `usage` block) fits whole: a mid-object
/// cut hides `output_tokens_details` and breaks JSON parsing.
const SHAPE_LINE: usize = 2000;

/// `value` with long strings cut and every array reduced to its first element:
/// what fields an event carries, without its text.
fn shape_of(value: &Value) -> Value {
    match value {
        Value::String(s) if s.chars().count() > SHAPE_STRING => {
            Value::String(s.chars().take(SHAPE_STRING).collect::<String>() + "...")
        }
        Value::Array(items) => Value::Array(items.first().map(shape_of).into_iter().collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape_of(v))).collect())
        }
        other => other.clone(),
    }
}

/// True the first time `key` is seen since the proxy started; false once
/// [`UNHANDLED_NAMES`] keys are held, so a provider cannot grow the set.
fn first_sighting(key: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let Ok(mut seen) = SEEN.get_or_init(Default::default).lock() else {
        return false;
    };
    seen.len() < UNHANDLED_NAMES && seen.insert(key.to_string())
}

/// Log the shape of the first event of each kind (its name, plus the item type
/// for `output_item.*`), handled or not, so a field the translator ignores in
/// an event it does handle is visible too.
fn sample_event_shape(event_name: &str, chunk: &Value) {
    let item_type = chunk
        .get("item")
        .and_then(|i| i.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let key = format!("{event_name}/{item_type}");
    if !first_sighting(&key) {
        return;
    }
    let mut shape = shape_of(chunk).to_string();
    shape.truncate(shape.floor_char_boundary(SHAPE_LINE));
    tracing::info!(
        event = "responses_stream_event_shape",
        stream_event = event_name,
        item_type,
        shape,
        "first event of this kind since the proxy started"
    );
}

/// The `output_index` a Responses stream event names, if any.
fn output_index(chunk: &Value) -> Option<u64> {
    chunk.get("output_index").and_then(Value::as_u64)
}

/// A client alias for a translated route (Spark, Codex) rather than a native
/// Anthropic turn. Both bill from a different cache universe than the
/// Anthropic footprint the usage observer scores — translated prefix, no
/// write/TTL telemetry — so scoring them reads as a bust on nearly every
/// turn. Case-insensitive, matching the guards this replaces.
pub(crate) fn translated_route_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("spark") || lower.contains("codex")
}

#[cfg(test)]
mod translated_route_model_tests {
    use super::translated_route_model;

    /// The Codex alias must trip the same exclusion Spark has, or its
    /// zero-creation turns keep filing as recache waste.
    #[test]
    fn codex_and_spark_aliases_are_translated_routes() {
        for model in [
            "claude-codex-6-luna",
            "claude-codex-5.6",
            "claude-muse-spark-1.3",
            "gpt-5.6-codex",
        ] {
            assert!(translated_route_model(model), "{model} is translated");
        }
        for model in ["claude-opus-5-5", "claude-sonnet-5", "qwen-local"] {
            assert!(!translated_route_model(model), "{model} is native");
        }
    }
}

/// First-round provider usage captured for a turn whose booking waits
/// downstream. Written when usage arrives and at every outcome emission, so
/// the completion guard books exactly what an immediate booking would have.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeferredFirstRound {
    pub usage: Option<Value>,
    pub output_tokens: i64,
    pub ttfb_ms: f64,
    /// Provisional 200 until a terminal event records the real code; a usage
    /// chunk never arrives after a terminal event (the `terminated` flag
    /// guards it), so the provisional value cannot clobber a real one.
    pub status_code: i64,
}

/// Defers a streamed turn's booking to the completion guard in
/// `routed::response_arms`, which folds continuation rounds in. The
/// translator keeps `outcome` for everything else — usage observation,
/// replay completion, TTFB, quota warnings — only the book call moves.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeferredCcrBooking {
    /// Continuation rounds, populated by the rewriter task after the
    /// translator's `[DONE]`. Shared, never moved.
    pub rounds: std::sync::Arc<std::sync::Mutex<crate::proxy::CcrRoundUsage>>,
    /// First-round usage snapshot, written by the translator as it streams.
    pub first: std::sync::Arc<std::sync::Mutex<DeferredFirstRound>>,
    /// Set by the translator when the turn completed text-only and its last
    /// message carried `phase: "commentary"`: the model labelled its own
    /// text as a preamble to work, then stopped. The rewriter's orphan
    /// continuation reads this instead of guessing from the words.
    pub ended_on_commentary: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DeferredCcrBooking {
    pub(crate) fn new() -> Self {
        Self {
            rounds: std::sync::Arc::new(std::sync::Mutex::new(
                crate::proxy::CcrRoundUsage::default(),
            )),
            first: std::sync::Arc::new(std::sync::Mutex::new(DeferredFirstRound {
                status_code: 200,
                ..Default::default()
            })),
            ended_on_commentary: Default::default(),
        }
    }
}

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
        // A translated turn is released even without usage: its entry exists
        // only for the concurrency cap, and a dead one would count against
        // the conversation until the horizon sweep.
        if self.outcome.is_some()
            && !self.observation_completed
            && (self.last_usage.is_some() || self.translated_turn())
        {
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
    /// Client-facing alias at construction (`claude-muse-spark-*`,
    /// `claude-codex-*`). `model` gets overwritten by the upstream name
    /// from `response.created`; this snapshot survives it, so route-level
    /// decisions (e.g. Zen never sends quota) stay correct.
    client_model: String,
    content_block_index: usize,
    started: bool,
    open: Option<OpenBlock>,
    current_tool_id: String,
    current_tool_name: String,
    /// Tool-call ids already handed to the client this turn. Same-id
    /// `tool_use` blocks get results the upstream then rejects, so
    /// `claim_tool_id` renames any repeat. Every rename logged on 2026-10-06
    /// (54 of 54) came from this translator re-emitting a late `.done` as a
    /// new call, not from Spark (see `announced_tool_indices`).
    claimed_tool_ids: std::collections::HashSet<String>,
    /// `output_index` of every function_call announced by
    /// `output_item.added` this turn. Zen streams each call's `.added` and
    /// arguments in order but holds every call's `.done` until all items
    /// have streamed (captured raw 2026-10-06), so a `.done` routinely lands
    /// after its block was closed. Membership here is what tells that late
    /// `.done` apart from a call whose `.added` truly never came; treating
    /// it as the latter re-emitted the call, and the client ran it twice.
    announced_tool_indices: std::collections::HashSet<u64>,
    /// `output_index` of the call whose tool block is open, when known.
    /// A late `.done` or `arguments.done` for another call must not close
    /// this block or write into it.
    open_tool_index: Option<u64>,
    /// `phase` of the last message item this turn (`commentary` or
    /// `final_answer`), for the completion log. A text-only turn ending on
    /// `commentary` is the model saying, in its own label, that it meant to
    /// keep working.
    last_message_phase: Option<String>,
    total_output_tokens: u64,
    saw_tool_use: bool,
    /// Whether any `output_text.delta` arrived for the message item currently
    /// streaming. The `output_item.done` event carries the finished item, and
    /// when the upstream sent no deltas that copy is the only one there is.
    saw_text_delta: bool,
    /// Whether any text block reached the client this turn, by any path.
    /// The per-item flags above reset in `output_item.done` so a second item
    /// is judged on its own deltas; the completion log needs the sticky
    /// answer instead (the Chat `delta.content` path and the message-item
    /// recovery below emit text without touching the per-item flags).
    saw_text_ever: bool,
    /// First ~240 chars of text emitted this turn, for the completion log.
    /// A turn that announces an action and then emits no tool call is only
    /// diagnosable if the log shows what it announced.
    text_head: String,
    /// Characters of text emitted this turn, for the completion log's
    /// token-gap check.
    visible_chars: usize,
    /// Same, for `response.refusal.delta` on the current item.
    saw_refusal_delta: bool,
    /// Whether any `function_call_arguments.delta` arrived for the tool call
    /// currently streaming. `arguments.done` carries the whole arguments and
    /// replays them when no delta did.
    saw_arg_delta: bool,
    /// Whether visible thinking text reached the client for the reasoning item
    /// currently streaming. A reasoning item that streamed no summary text
    /// carries nothing a human can read — only the replay envelope — so on
    /// upstreams that discard reasoning anyway (Zen/spark, see
    /// `strip_unreplayable_reasoning`) emitting its block is pure noise: an
    /// empty thinking box in the transcript plus prefix churn the provider
    /// never sees. The `output_item.done` arm reads this to decide.
    saw_thinking_text: bool,
    /// Set once the turn was closed early (`abort_terminal`): any straggler
    /// frames after a transport error must not reopen it.
    terminated: bool,
    /// Set once a Responses terminal event (`completed`/`incomplete`/`failed`)
    /// reached the wire. Upstreams occasionally send two terminals for one
    /// turn; the second must be swallowed, not emitted — a `failed` after a
    /// `completed` must not error a good turn, and a `completed` after a
    /// `failed` must not re-close an errored one. Separate from `terminated`,
    /// which also stops frame processing: ordinary frames (notably the
    /// trailing `[DONE]`, which still books the outcome) must keep flowing.
    terminal_emitted: bool,
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
    /// Fallback label for the abort and quota-miss log lines when `outcome`
    /// is `None` (sidecars, stateless turns). Booking context is overkill
    /// there; the request id alone keeps the lines attributable.
    request_id: Option<String>,
    /// When set, `emit_outcome` records into `first` instead of booking: the
    /// rewriter downstream owns the single booking, with rounds folded in.
    /// Everything else `outcome` drives stays live.
    deferred_ccr: Option<DeferredCcrBooking>,
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
    /// Reverse tool-name map (upstream → client) for the Zen gate's
    /// lowercase rename. `None` on every path that never renamed outbound,
    /// so only Zen-bound routed turns pay attention to it.
    tool_aliases: Option<crate::routed::tool_alias::ToolAlias>,
    /// Tool calls this turn whose upstream name was a gate shadow name the
    /// rename layer could not restore (`ToolAlias::is_shadow_call`). For the
    /// drops-audit rate on the completion log.
    shadow_tool_uses: u32,
    /// `--report-true-usage`: put the provider's own counts in the terminal
    /// `message_delta` instead of the legacy output-only shape.
    report_true_usage: bool,
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

    /// Claim a tool-call id for this turn, renaming upstream duplicates.
    /// First claim keeps the id; repeats take `id_2`, `id_3`, … The rename
    /// stays consistent end to end: the client answers the renamed ids and
    /// the request translator replays them as the new `call_id`s, so calls
    /// and outputs still pair up. The suffix stays inside Anthropic's
    /// `^[a-zA-Z0-9_-]+$` id pattern: the session's history can go to a
    /// Claude model after `/model`, which rejects any other character.
    /// Logs once per rename.
    fn claim_tool_id(&mut self, raw: &str) -> String {
        if self.claimed_tool_ids.insert(raw.to_string()) {
            return raw.to_string();
        }
        let mut n = 2;
        loop {
            let candidate = format!("{raw}_{n}");
            if self.claimed_tool_ids.insert(candidate.clone()) {
                tracing::info!(
                    event = "routed_duplicate_call_id_renamed",
                    raw_call_id = %raw,
                    renamed_to = %candidate,
                    "upstream reused a call id in one turn; renamed the repeat"
                );
                return candidate;
            }
            n += 1;
        }
    }

    /// Whether the open block is the tool call `chunk` belongs to. Events
    /// without an `output_index` (and blocks opened without one) match any
    /// open tool block, as before indices were tracked.
    fn tool_block_open_for(&self, chunk: &Value) -> bool {
        self.open == Some(OpenBlock::Tool)
            && match (self.open_tool_index, output_index(chunk)) {
                (Some(open), Some(event)) => open == event,
                _ => true,
            }
    }

    /// Remember the `phase` a message item carries; see `last_message_phase`.
    fn note_message_phase(&mut self, item: Option<&Value>) {
        if let Some(phase) = item.and_then(|i| i.get("phase")).and_then(Value::as_str) {
            self.last_message_phase = Some(phase.to_string());
        }
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
            model: model.clone(),
            client_model: model,
            content_block_index: 0,
            started: false,
            open: None,
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            claimed_tool_ids: std::collections::HashSet::new(),
            announced_tool_indices: std::collections::HashSet::new(),
            open_tool_index: None,
            last_message_phase: None,
            total_output_tokens: 0,
            saw_tool_use: false,
            saw_text_delta: false,
            saw_text_ever: false,
            text_head: String::new(),
            visible_chars: 0,
            saw_refusal_delta: false,
            saw_arg_delta: false,
            saw_thinking_text: false,
            terminated: false,
            terminal_emitted: false,
            pending_reasoning: PendingReasoning::default(),
            codex_limits: None,
            codex_rate_limits_seen: false,
            codex_rate_limits_finished: false,
            outcome: None,
            request_id: None,
            deferred_ccr: None,
            outcome_emitted: false,
            observation_completed: false,
            ttfb_ms: 0.0,
            last_usage: None,
            tool_aliases: None,
            shadow_tool_uses: 0,
            report_true_usage: false,
        }
    }

    /// Restore client tool names on the way back from a renamed outbound
    /// turn (see `routed::tool_alias`). The stored name is what every
    /// later emission for this call uses, so one mapping at capture covers
    /// the block start, the done frame, and the abort marker alike.
    pub(crate) fn with_tool_aliases(
        mut self,
        aliases: Option<crate::routed::tool_alias::ToolAlias>,
    ) -> Self {
        self.tool_aliases = aliases;
        self
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
            .or(self.request_id.as_deref())
            .unwrap_or("unknown");
        // Zen never sends quota (no headers, no rate_limits frames): warn
        // would fire on every routed turn. Debug keeps the signal for
        // troubleshooting without log noise. Matched on the client alias
        // snapshot: self.model is the upstream name by now. Only the Spark
        // aliases route to Zen; a `gpt-5` prefix here also silenced direct
        // Codex models, where a missing quota is a real signal.
        if self.client_model.starts_with("claude-muse-spark") {
            tracing::debug!(
                event = "codex_rate_limits_missing",
                request_id = %request_id,
                model = %self.model,
                "routed Codex stream ended without quota (expected on Zen)"
            );
            return;
        }
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

    fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// Hand booking downstream: the completion guard books once, with
    /// continuation rounds folded in. Observation, replay, TTFB, and quota
    /// paths keep working off `outcome` unchanged.
    fn with_deferred_ccr(mut self, deferred: DeferredCcrBooking) -> Self {
        self.deferred_ccr = Some(deferred);
        self
    }

    fn with_report_true_usage(mut self, on: bool) -> Self {
        self.report_true_usage = on;
        self
    }

    /// Record the tool name for the call now streaming, mapping it back
    /// to the client's name when the outbound turn was renamed for the
    /// Zen gate (see `with_tool_aliases`). One site covers both wire
    /// shapes: Responses `output_item.added` and Chat `delta.tool_calls`.
    fn capture_tool_name(&mut self, upstream_name: &str) {
        if self
            .tool_aliases
            .as_ref()
            .is_some_and(|a| a.is_shadow_call(upstream_name))
        {
            self.shadow_tool_uses += 1;
        }
        self.current_tool_name = self
            .tool_aliases
            .as_ref()
            .map(|aliases| aliases.reverse_name(upstream_name).to_string())
            .unwrap_or_else(|| upstream_name.to_string());
    }

    /// Snapshot first-round usage for the deferred booking. Written when
    /// usage arrives (status provisional) and overwritten at every outcome
    /// emission with the terminal status code. The output fallback follows
    /// the provider-wins rule below, so the guard books what the buffered
    /// arm would book for the same usage.
    fn record_deferred_first(&self, usage: Option<&Value>, status_code: i64) {
        let Some(deferred) = self.deferred_ccr.as_ref() else {
            return;
        };
        if let Ok(mut first) = deferred.first.lock() {
            *first = DeferredFirstRound {
                usage: usage.cloned(),
                output_tokens: Self::output_fallback(usage, self.total_output_tokens),
                ttfb_ms: self.ttfb_ms,
                status_code,
            };
        }
    }

    /// Output fallback for booking: the provider's own count wins whenever
    /// it reported one (matching the buffered arms, which pass a
    /// shape-derived fallback the reported numbers already cover). The
    /// translator's running count survives only for provider-silent turns —
    /// dropped streams whose usage block never arrived — where it is the
    /// only number there is.
    fn output_fallback(usage: Option<&Value>, counted: u64) -> i64 {
        let reported = usage.and_then(|u| {
            u.get("output_tokens")
                .or_else(|| u.get("completion_tokens"))
                .and_then(|v| v.as_i64())
        });
        if reported.is_some() {
            0
        } else {
            counted as i64
        }
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
    /// Spark or Codex, judged on every model name the turn carries.
    fn translated_turn(&self) -> bool {
        translated_route_model(&self.model)
            || translated_route_model(&self.client_model)
            || self
                .outcome
                .as_ref()
                .is_some_and(|ctx| translated_route_model(&ctx.model))
    }

    fn complete_usage_observation(&mut self, usage: Option<&Value>) {
        if self.observation_completed {
            return;
        }
        let Some(ctx) = self.outcome.as_ref() else {
            return;
        };
        // Spark bills from a different cache universe than the Anthropic
        // footprint this observer scores (translated prefix, no write/TTL
        // telemetry, separate per-model lineage). Booking it here reads as a
        // bust on nearly every turn and drags the fleet hit rate down for no
        // reason, so Spark stays out of cache health entirely — the request
        // side already skips parking it, and this guard covers any turn that
        // parked through another path. Spend is still booked through the
        // outcome funnel; only the cache-health signal skips it.
        //
        // Codex rides the same translated Responses route with the same gap
        // (never a cache-creation counter, so every shortfall reads Recache):
        // same exclusion. Matched on the client-alias snapshot, since `model`
        // is the upstream name by now.
        //
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
        // Both OpenAI shapes report an `input_tokens` that *includes* the
        // cached prefix; the observer's counter is Anthropic-shaped and
        // excludes it, and it adds `cache_read` back on to form the
        // denominator. Passing the provider's number straight through counts
        // the prefix twice and drags every routed turn's hit rate down.
        let provider_input = get("input_tokens").max(get("prompt_tokens"));
        // The request side still parks these turns so the concurrency cap
        // can count them; hand the entry back unscored, with the first-turn
        // marker so routed sessions delimit the way native ones do.
        if self.translated_turn() {
            // No usage (a stream dropped before any arrived): release the
            // entry without the first-turn marker. Two invented zeros would
            // mark the conversation seen, and its real first turn would never
            // be counted.
            if let Some(observer) = ctx.usage_observer.as_ref() {
                if usage.is_none() {
                    observer.end_unscored(&ctx.request_id);
                } else {
                    observer.complete_translated_first_turn(
                        &ctx.request_id,
                        cache_read,
                        provider_input.saturating_sub(cache_read),
                    );
                }
            }
            self.observation_completed = true;
            return;
        }
        let Some(observer) = ctx.usage_observer.as_ref() else {
            return;
        };
        // A missing `cached_tokens` field is "the provider reported no cache
        // data", not a miss: counting it as zero drags the fleet hit rate down
        // on providers with no cache telemetry.
        let cache_capable = cache_details.and_then(|d| d.get("cached_tokens")).is_some();
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
        if self.ttfb_ms == 0.0
            && let Some(ctx) = self.outcome.as_ref()
        {
            self.ttfb_ms = ctx.started_at.elapsed().as_secs_f64() * 1000.0;
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
        // Deferred turns book downstream with rounds folded in; record the
        // terminal numbers here and disarm, so Drop cannot book a second time.
        if self.deferred_ccr.is_some() {
            self.record_deferred_first(usage, status_code);
            self.outcome_emitted = true;
            return;
        }
        self.outcome_emitted = true;
        book_routed_outcome(
            ctx,
            usage,
            Self::output_fallback(usage, self.total_output_tokens),
            self.ttfb_ms,
            status_code,
        );
    }

    /// `[DONE]` with a block still open and no terminal event seen: the
    /// upstream closed the stream without `completed`/`failed`/`incomplete`.
    /// A tool block closed here rides out with `end_turn`, so the client
    /// never runs the call — the "announced it and stopped" shape. Log loud
    /// so the next one joins to a lane instead of reading as a clean stop.
    fn log_done_with_open_block(&self) {
        if self.terminal_emitted {
            return;
        }
        let kind = match self.open {
            Some(OpenBlock::Tool) => "tool",
            Some(OpenBlock::Text) => "text",
            Some(OpenBlock::Thinking) => "thinking",
            None => return,
        };
        let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
            self.terminal_attribution();
        tracing::warn!(
            event = "routed_done_with_open_block",
            request_id = %request_id,
            session_id = session_id.as_deref().unwrap_or("none"),
            egress_slot,
            upstream_model = %model,
            routed_provider = %provider,
            stream_age_s,
            open_block = kind,
            saw_tool_use = self.saw_tool_use,
            saw_text = self.saw_text_ever,
            output_tokens = self.total_output_tokens,
            "upstream closed the stream with a block still open and no terminal event"
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
                self.log_done_with_open_block();
                if self.open == Some(OpenBlock::Tool) && !self.terminal_emitted {
                    // A tool call still streaming when the upstream quit is
                    // as truncated as one cut by a dead connection: close it
                    // the same way, so `finish_on_drop` withholds the partial
                    // block and marks the turn instead of releasing the call
                    // as complete.
                    events.extend(self.abort_terminal());
                } else {
                    self.close_block_final(&mut events);
                    let usage = self.last_usage.clone();
                    events.push(self.emit_message_delta_with_usage("end_turn", usage.as_ref()));
                    events.push(self.emit_message_stop());
                }
            }
            return events;
        }

        if let Some(name) = event_name
            && (name.starts_with("response.") || name.starts_with("output_"))
        {
            return self.process_responses_frame(name, data);
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
                // Snapshot for a deferred booking while the numbers are hot;
                // the terminal emission below overwrites with its status code.
                self.record_deferred_first(Some(usage), 200);
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
                self.record_text_emitted(text);
            }

            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        self.current_tool_id = self.claim_tool_id(id);
                        self.capture_tool_name(
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or(""),
                        );

                        self.open_tool_block(&mut events);
                    }

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        && !args.is_empty()
                    {
                        self.open_block(OpenBlock::Tool, &mut events);
                        events.push(self.emit_input_json_delta(args));
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
            // Truth is whatever a `usage` chunk stored before this one; when
            // the upstream sends usage after `finish_reason` there is none
            // yet and the delta keeps the legacy shape.
            let usage = self.last_usage.clone();
            events.push(self.emit_message_delta_with_usage(stop_reason, usage.as_ref()));
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
        sample_event_shape(event_name, &chunk);

        // Quota can ride in the stream as well as the headers, and which one
        // carries it has changed before. Take it from wherever it shows up.
        if let Some(store) = self.codex_limits.as_ref()
            && let Some(limits) = crate::codex_rate_limits::extract_rate_limits(&chunk)
        {
            store.record_rate_limits(&self.model, limits);
            self.codex_rate_limits_seen = true;
        }

        if !self.started
            && event_name == "response.created"
            && let Some(model) = chunk
                .get("response")
                .and_then(|resp| resp.get("model"))
                .and_then(|v| v.as_str())
        {
            self.model = model.to_string();
        }

        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        match event_name {
            "response.output_text.delta" | "output_text.delta" => {
                self.on_output_text_delta(&chunk, &mut events);
            }
            // `output_text.done` carries the whole text. Normally the deltas
            // above already delivered it and this is a marker; when the
            // upstream sent no deltas it is the only copy, mirroring the
            // message-item recovery below.
            "response.output_text.done" | "output_text.done" => {
                self.on_output_text_done(&chunk, &mut events);
            }
            // A refusal is the turn's only text. Anthropic has no refusal
            // block, so it rides as a text block; without this the client
            // receives an empty `end_turn` it cannot tell from silence.
            "response.refusal.delta" | "refusal.delta" => {
                self.on_refusal_delta(&chunk, &mut events);
            }
            "response.refusal.done" | "refusal.done" => {
                self.on_refusal_done(&chunk, &mut events);
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.on_reasoning_delta(&chunk, &mut events);
            }
            "response.reasoning_summary_part.added" => {
                // Part boundary: close the current thinking block so the next
                // summary part starts a fresh one.
                self.close_block_if(OpenBlock::Thinking, &mut events);
            }
            "response.output_item.added" => {
                self.on_output_item_added(&chunk, &mut events);
            }
            "response.function_call_arguments.delta" => {
                self.on_function_args_delta(&chunk, &mut events);
            }
            // `arguments.done` carries the whole arguments string. Normally
            // the deltas above already delivered it; when they did not — or
            // arrived before the item announced itself — this is the only
            // copy, mirroring the message-item recovery.
            "response.function_call_arguments.done" => {
                self.on_function_args_done(&chunk, &mut events);
            }
            "response.output_item.done" => {
                self.on_output_item_done(&chunk, &mut events);
            }
            "response.completed" => {
                self.on_completed(&chunk, &mut events);
            }
            "response.failed" => {
                self.on_failed(&chunk, &mut events);
            }
            "response.incomplete" => {
                self.on_incomplete(&chunk, &mut events);
            }
            other => {
                Self::on_unhandled_event(other);
            }
        }

        events
    }

    /// `response.output_text.delta`: stream a text delta into the open text block.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_output_text_delta(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let delta = chunk.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if !delta.is_empty() {
            self.open_block(OpenBlock::Text, events);
            events.push(self.emit_text_delta(delta));
            self.saw_text_delta = true;
            self.record_text_emitted(delta);
        }
    }

    /// `output_text.done` carries the whole text. Normally the deltas above
    /// already delivered it and this is a marker; when the upstream sent no
    /// deltas it is the only copy, mirroring the message-item recovery below.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_output_text_done(&mut self, chunk: &Value, events: &mut Vec<String>) {
        if !self.saw_text_delta
            && let Some(text) = chunk.get("text").and_then(|v| v.as_str())
            && !text.is_empty()
        {
            self.open_block(OpenBlock::Text, events);
            events.push(self.emit_text_delta(text));
            self.close_block(events);
            self.saw_text_delta = true;
            self.record_text_emitted(text);
        }
    }

    /// A refusal is the turn's only text. Anthropic has no refusal block, so
    /// it rides as a text block; without this the client receives an empty
    /// `end_turn` it cannot tell from silence.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_refusal_delta(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let delta = chunk.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if !delta.is_empty() {
            self.open_block(OpenBlock::Text, events);
            events.push(self.emit_text_delta(delta));
            self.saw_refusal_delta = true;
            self.record_text_emitted(delta);
        }
    }

    /// `refusal.done`: replay the whole refusal when no delta arrived.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_refusal_done(&mut self, chunk: &Value, events: &mut Vec<String>) {
        if !self.saw_refusal_delta
            && let Some(refusal) = chunk.get("refusal").and_then(|v| v.as_str())
            && !refusal.is_empty()
        {
            self.open_block(OpenBlock::Text, events);
            events.push(self.emit_text_delta(refusal));
            self.close_block(events);
            self.saw_refusal_delta = true;
            self.record_text_emitted(refusal);
        }
    }

    /// Reasoning delta: stream into the open thinking block.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_reasoning_delta(&mut self, chunk: &Value, events: &mut Vec<String>) {
        if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str())
            && !delta.is_empty()
        {
            self.open_block(OpenBlock::Thinking, events);
            events.push(self.emit_thinking_delta(delta));
            self.saw_thinking_text = true;
        }
    }

    /// `response.output_item.added`: open tool blocks and start assembling
    /// reasoning items as soon as they are announced.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_output_item_added(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let item = chunk.get("item");
        let item_type = item.and_then(|i| i.get("type")).and_then(|t| t.as_str());
        if item_type == Some("function_call") {
            // `call_id` is what must round-trip back as
            // function_call_output; fall back to `id` if absent.
            // Claimed, not copied: the upstream reuses one id across
            // calls in a turn.
            let raw_id = item
                .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            self.current_tool_id = self.claim_tool_id(raw_id);
            let upstream_name = item
                .and_then(|i| i.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            self.capture_tool_name(upstream_name);
            self.open_tool_block(events);
            self.open_tool_index = output_index(chunk);
            if let Some(index) = self.open_tool_index {
                self.announced_tool_indices.insert(index);
            }
            self.saw_tool_use = true;
            // Per call, not per stream: `arguments.done` below may
            // only replay the full arguments when no delta arrived
            // for this call.
            self.saw_arg_delta = false;
        }
        if item_type == Some("message") {
            self.note_message_phase(item);
        }
        // A reasoning item may announce its id here and carry the blob
        // on `.done`, so start assembling as soon as it appears.
        if item_type == Some("reasoning") {
            if let Some(item) = item {
                self.pending_reasoning.capture(item);
            }
            // Per item, not per stream: the `.done` arm judges each
            // item on its own visible thinking (see `saw_thinking_text`).
            self.saw_thinking_text = false;
        }
    }

    /// `function_call_arguments.delta`: stream argument deltas into the open
    /// tool block.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_function_args_delta(&mut self, chunk: &Value, events: &mut Vec<String>) {
        if self.tool_block_open_for(chunk)
            && let Some(delta) = chunk.get("delta").and_then(|v| v.as_str())
            && !delta.is_empty()
        {
            events.push(self.emit_input_json_delta(delta));
            self.saw_arg_delta = true;
        }
        // A delta arriving before `output_item.added` (no open tool
        // block to attribute it to) is skipped rather than guessed
        // at: when `.added` still arrives before `.done`, the
        // `arguments.done` fallback below replays the whole arguments,
        // so nothing is lost. When `.added` never arrives at all, the
        // `.done` handler rebuilds the call from the item instead.
    }

    /// `arguments.done` carries the whole arguments string. Normally the
    /// deltas above already delivered it; when they did not — or arrived
    /// before the item announced itself — this is the only copy, mirroring
    /// the message-item recovery.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_function_args_done(&mut self, chunk: &Value, events: &mut Vec<String>) {
        if self.tool_block_open_for(chunk)
            && !self.saw_arg_delta
            && let Some(args) = chunk.get("arguments").and_then(|v| v.as_str())
            && !args.is_empty()
        {
            events.push(self.emit_input_json_delta(args));
            self.saw_arg_delta = true;
        }
    }

    /// Recovery: `output_item.added` for this call never arrived, so no tool
    /// block is open and neither deltas nor `arguments.done` had anywhere to
    /// land. This `.done` item carries the whole call, so rebuild the block
    /// from it, mirroring the message-item recovery in
    /// `on_output_item_done`. Without this the call is silently lost and the
    /// client sees an announcement followed by `end_turn` with no tool call.
    fn recover_unannounced_call(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
            self.terminal_attribution();
        tracing::warn!(
            event = "routed_function_call_without_added",
            request_id = %request_id,
            session_id = session_id.as_deref().unwrap_or("none"),
            egress_slot,
            upstream_model = %model,
            routed_provider = %provider,
            stream_age_s,
            "recovered a function_call item whose output_item.added never arrived"
        );
        let item = chunk.get("item");
        let raw_id = item
            .and_then(|i| i.get("call_id").or_else(|| i.get("id")))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.current_tool_id = self.claim_tool_id(raw_id);
        self.capture_tool_name(
            item.and_then(|i| i.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        self.open_tool_block(events);
        self.open_tool_index = output_index(chunk);
        self.saw_tool_use = true;
        self.saw_arg_delta = false;
        if let Some(args) = item
            .and_then(|i| i.get("arguments"))
            .and_then(|v| v.as_str())
            && !args.is_empty()
        {
            events.push(self.emit_input_json_delta(args));
            self.saw_arg_delta = true;
        }
    }

    /// `response.output_item.done`: close tool blocks, recover whole messages
    /// the upstream never streamed, and seal reasoning signatures.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_output_item_done(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let item_type = chunk
            .get("item")
            .and_then(|i| i.get("type"))
            .and_then(|t| t.as_str());
        let announced =
            output_index(chunk).is_some_and(|i| self.announced_tool_indices.contains(&i));
        if item_type == Some("function_call") && announced {
            // The call already reached the client from its `.added` and
            // deltas. Zen sends this `.done` only after every item has
            // streamed, so its block is usually long closed; close it only
            // if it is still the open one.
            if self.tool_block_open_for(chunk) {
                self.close_block(events);
            }
        } else if item_type == Some("function_call") {
            if !self.tool_block_open_for(chunk) {
                self.recover_unannounced_call(chunk, events);
            }
            self.close_block_if(OpenBlock::Tool, events);
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
            self.note_message_phase(chunk.get("item"));
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
                    self.open_block(OpenBlock::Text, events);
                    events.push(self.emit_text_delta(&text));
                    self.record_text_emitted(&text);
                    self.close_block(events);
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
            // Judged on this item's own visible thinking, then consumed:
            // a later silent item must not inherit an earlier one's.
            let visible_thinking = self.saw_thinking_text;
            self.saw_thinking_text = false;
            if let Some(signature) = signature {
                // Zen discards reasoning server-side and we strip it
                // client-side next turn (`strip_unreplayable_reasoning`),
                // so on spark a reasoning item with no visible summary
                // text would emit a thinking block holding nothing but
                // our opaque envelope: an empty thinking box in the
                // transcript plus prefix churn the provider never sees.
                // Skip the block; visible thinking still streams, and
                // replay-preserving upstreams (codex) are untouched.
                let silent_on_unreplayable =
                    !visible_thinking && self.model.to_lowercase().contains("spark");
                if !silent_on_unreplayable {
                    // Reasoning summaries can be off entirely, in which case
                    // no block was ever opened. Open an empty one rather
                    // than drop the only copy of the item.
                    self.open_block(OpenBlock::Thinking, events);
                    events.push(self.emit_signature_delta(&signature));
                    self.close_block(events);
                }
            }
        }
    }

    /// `response.completed`: record usage, book the outcome, and end the turn.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_completed(&mut self, chunk: &Value, events: &mut Vec<String>) {
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
        if !self.terminal_emitted {
            self.recover_unstreamed_calls(chunk, events);
        }
        self.close_block_final(events);
        // A second terminal for one turn (usually `completed` after `failed`):
        // swallow it rather than re-closing an errored turn as clean.
        if !self.claim_terminal() {
            return;
        }
        // A completed response can still carry `incomplete_details`,
        // and truncation outranks a tool call: a `tool_use` stop on a
        // cut-off turn would have the client run a half-streamed call.
        // Logged loud with attribution: a completed-but-cut turn still
        // looks like a voluntary stop downstream.
        let incomplete_reason = chunk
            .get("response")
            .and_then(|v| v.get("incomplete_details"))
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str());
        let truncated = incomplete_reason == Some("max_output_tokens");
        let stop_reason = if truncated {
            "max_tokens"
        } else if self.saw_tool_use {
            "tool_use"
        } else {
            "end_turn"
        };
        if !truncated && incomplete_reason.is_some() {
            let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
                self.terminal_attribution();
            tracing::warn!(
                event = "routed_response_completed_incomplete",
                request_id = %request_id,
                session_id = session_id.as_deref().unwrap_or("none"),
                egress_slot,
                upstream_model = %model,
                routed_provider = %provider,
                stream_age_s,
                output_tokens = self.total_output_tokens,
                incomplete_reason = incomplete_reason.unwrap_or("none"),
                stop_reason,
                "routed turn completed but carried incomplete_details"
            );
        }
        // Every clean completion logs one line: the Responses path is
        // otherwise silent per turn, so a text-only stop (model announced
        // work but emitted no tool call) was indistinguishable from a
        // translator drop. `saw_tool_use`/`saw_text_delta` join the next
        // "turn just stopped" report to what upstream actually sent.
        self.log_completion(stop_reason, chunk);
        if stop_reason == "end_turn"
            && self.last_message_phase.as_deref() == Some("commentary")
            && let Some(deferred) = self.deferred_ccr.as_ref()
        {
            deferred
                .ended_on_commentary
                .store(true, std::sync::atomic::Ordering::Release);
        }
        events.push(self.emit_message_delta_with_usage(stop_reason, usage.as_ref()));
        events.push(self.emit_message_stop());
    }

    /// Deliver any function_call that `response.completed` lists but the
    /// stream never announced. The final payload repeats every output item,
    /// so this is a check against the upstream's own record, not a guess: a
    /// call found here was produced by the model and lost on the way. Each
    /// one logs `routed_completed_call_not_streamed`.
    fn recover_unstreamed_calls(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let Some(items) = chunk.pointer("/response/output").and_then(Value::as_array) else {
            return;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                continue;
            }
            let raw_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if raw_id.is_empty() || self.claimed_tool_ids.contains(raw_id) {
                continue;
            }
            let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
                self.terminal_attribution();
            tracing::warn!(
                event = "routed_completed_call_not_streamed",
                request_id = %request_id,
                session_id = session_id.as_deref().unwrap_or("none"),
                egress_slot,
                upstream_model = %model,
                routed_provider = %provider,
                stream_age_s,
                "response.completed lists a function_call the stream never sent; delivering it"
            );
            self.current_tool_id = self.claim_tool_id(raw_id);
            self.capture_tool_name(item.get("name").and_then(Value::as_str).unwrap_or(""));
            self.open_tool_block(events);
            self.open_tool_index = None;
            self.saw_tool_use = true;
            if let Some(args) = item.get("arguments").and_then(Value::as_str)
                && !args.is_empty()
            {
                events.push(self.emit_input_json_delta(args));
            }
            self.close_block(events);
        }
    }

    /// Record emitted text: sticky flag for the completion log plus the
    /// first ~240 chars of the turn (logged at DEBUG), so an
    /// announced-but-unrun action is diagnosable without a body capture.
    fn record_text_emitted(&mut self, text: &str) {
        self.saw_text_ever = true;
        self.visible_chars += text.chars().count();
        if self.text_head.len() < 240 {
            let room = 240 - self.text_head.len();
            self.text_head.extend(text.chars().take(room));
        }
    }

    /// One INFO line per clean completion. Extracted so `on_completed`
    /// stays under the complexity ratchet; see the call-site comment.
    ///
    /// `output_items` and `reasoning_tokens` come from the final
    /// `response.completed` payload, not the stream: on a text-only stop they
    /// show whether the model produced a call at all, without reading its
    /// words. `output_tokens` well above reasoning plus visible text is a
    /// call generated and never delivered.
    fn log_completion(&self, stop_reason: &str, completed: &Value) {
        let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
            self.terminal_attribution();
        let response = completed.get("response");
        let output_items = response
            .and_then(|r| r.get("output"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|i| i.get("type").and_then(Value::as_str).unwrap_or("?"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let reasoning_tokens = response
            .and_then(|r| r.pointer("/usage/output_tokens_details/reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // Input includes its cached part; the pair gives the turn's cache hit
        // rate, which no other routed log line carries.
        let usage = |pointer: &str| {
            response
                .and_then(|r| r.pointer(pointer))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        tracing::info!(
            output_items = %output_items,
            reasoning_tokens,
            input_tokens = usage("/usage/input_tokens"),
            cached_tokens = usage("/usage/input_tokens_details/cached_tokens"),
            visible_chars = self.visible_chars,
            event = "routed_response_completed",
            request_id = %request_id,
            session_id = session_id.as_deref().unwrap_or("none"),
            egress_slot,
            upstream_model = %model,
            routed_provider = %provider,
            stream_age_s,
            output_tokens = self.total_output_tokens,
            saw_text = self.saw_text_ever,
            saw_tool_use = self.saw_tool_use,
            shadow_tool_uses = self.shadow_tool_uses,
            last_phase = self.last_message_phase.as_deref().unwrap_or("none"),
            stop_reason,
            "routed turn completed"
        );
        // The head of the model's text, kept off INFO: a reply can quote a
        // secret the model read, and INFO is the default level.
        tracing::debug!(
            event = "routed_response_text_head",
            request_id = %request_id,
            text_head = self.text_head.as_str(),
        );
    }

    /// Claim the single terminal emission for this turn. Returns false when a
    /// terminal event already reached the wire and this one must be swallowed.
    fn claim_terminal(&mut self) -> bool {
        if self.terminal_emitted {
            return false;
        }
        self.terminal_emitted = true;
        true
    }

    /// Attribution shared by the non-clean terminal log lines, so the next
    /// "the turn just stopped" report joins to a lane, a stream age, and a
    /// token count instead of a bare request id.
    fn terminal_attribution(&self) -> (String, Option<usize>, String, String, f64, Option<String>) {
        match self.outcome.as_ref() {
            Some(ctx) => (
                ctx.request_id.clone(),
                ctx.egress_slot,
                ctx.model.clone(),
                ctx.provider.clone(),
                ctx.started_at.elapsed().as_secs_f64(),
                ctx.session_id.clone(),
            ),
            None => (
                self.request_id.clone().unwrap_or_else(|| "unknown".into()),
                None,
                "unknown".into(),
                "unknown".into(),
                -1.0,
                None,
            ),
        }
    }

    /// `response.failed`: book a 500 outcome and end the turn LOUD on the
    /// wire. Before, this emitted `end_turn` + `message_stop`, so a provider
    /// failure arrived at the client indistinguishable from a voluntary stop —
    /// and, worse, the client accepted it instead of retrying. An in-band
    /// `api_error` is terminal too (no hang: the finisher treats an error
    /// event as the end of the message), but the client surfaces it and
    /// retries the turn.
    fn on_failed(&mut self, chunk: &Value, events: &mut Vec<String>) {
        // A straggler after a transport abort: the finisher owns that close.
        if self.terminated {
            return;
        }
        self.close_block_final(events);
        // Booked as a 500 so the outcome funnel routes it to
        // `record_failed` — a failed turn must not feed the save-rate.
        let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
        self.emit_outcome(usage.as_ref(), 500);
        // Terminally dead without ever reaching `complete`: pop the parked
        // entry so it never flags a later turn concurrent. The usage
        // observer's pending map is the only state touched — spend already
        // booked above through the outcome funnel.
        if self.translated_turn() {
            self.complete_usage_observation(None);
        } else if let Some(ctx) = self.outcome.as_ref()
            && let Some(observer) = ctx.usage_observer.as_ref()
        {
            observer.note_stream_ended(&ctx.request_id);
        }
        // A second terminal for one turn (usually `failed` after `completed`):
        // swallow it rather than erroring a turn that already closed clean.
        if !self.claim_terminal() {
            return;
        }
        let provider_message = chunk
            .get("response")
            .and_then(|v| v.get("error"))
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("upstream response failed");
        let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
            self.terminal_attribution();
        tracing::warn!(
            event = "routed_response_failed",
            request_id = %request_id,
            session_id = session_id.as_deref().unwrap_or("none"),
            egress_slot,
            upstream_model = %model,
            routed_provider = %provider,
            stream_age_s,
            output_tokens = self.total_output_tokens,
            provider_message = %provider_message,
            "routed turn failed upstream; ending loud so the client retries"
        );
        events.push(crate::sse::outbound::api_error(provider_message));
    }

    /// `response.incomplete`: end the turn with the truncation stop reason.
    /// The stop mapping is unchanged on purpose: `max_output_tokens` is the
    /// client's cue to continue, `content_filter` retried would filter again,
    /// and a missing reason is routine on healthy providers — erroring those
    /// would break good turns. But an incomplete turn must never pass
    /// silently, so it logs loud with attribution.
    fn on_incomplete(&mut self, chunk: &Value, events: &mut Vec<String>) {
        let reason = chunk
            .get("response")
            .and_then(|v| v.get("incomplete_details"))
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str());
        self.close_block_final(events);
        // A second terminal for one turn: swallow it.
        if !self.claim_terminal() {
            return;
        }
        let stop_reason = match reason {
            Some("max_output_tokens") => "max_tokens",
            _ => "end_turn",
        };
        let (request_id, egress_slot, model, provider, stream_age_s, session_id) =
            self.terminal_attribution();
        tracing::warn!(
            event = "routed_response_incomplete",
            request_id = %request_id,
            session_id = session_id.as_deref().unwrap_or("none"),
            egress_slot,
            upstream_model = %model,
            routed_provider = %provider,
            stream_age_s,
            output_tokens = self.total_output_tokens,
            incomplete_reason = reason.unwrap_or("none"),
            stop_reason,
            "routed turn stopped short upstream"
        );
        // Outside the reason: a response that stopped short still
        // spent tokens, whether or not it said why. Extracted before the
        // terminal events so the delta can carry the true counts.
        let usage = chunk.get("response").and_then(|v| v.get("usage")).cloned();
        events.push(self.emit_message_delta_with_usage(stop_reason, usage.as_ref()));
        events.push(self.emit_message_stop());
        self.emit_outcome(usage.as_ref(), 200);
    }

    /// Untranslated event: silence here is content the client never sees, so
    /// name it. The first sighting of each name logs at info, then every
    /// [`UNHANDLED_LOG_EVERY`]th, with the running count, so the next one is a
    /// grep and a chatty one does not flood the log.
    /// Extracted from `process_responses_frame` without behavior change.
    fn on_unhandled_event(other: &str) {
        if !other.starts_with("response.") {
            return;
        }
        let count = count_unhandled_event(other);
        if count == 1 || count.is_multiple_of(UNHANDLED_LOG_EVERY) {
            tracing::info!(
                event = "codex_unhandled_stream_event",
                stream_event = other,
                count,
                "no translation for this Responses event; nothing emitted"
            );
        }
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

    /// Usage block for a terminal `message_delta`.
    ///
    /// The legacy shape is output-only: translated routes always told the
    /// client `input_tokens: 0` at `message_start` and only
    /// `output_tokens` here. With `--report-true-usage` the provider's own
    /// counts go out instead, in Anthropic's key vocabulary, so the client
    /// compacts against real context sizes. `None`, or a block without
    /// both counts, falls back to the legacy shape: a missing number must
    /// not become a zero the client trusts.
    fn delta_usage(&self, truth: Option<&Value>) -> Value {
        let legacy = || json!({"output_tokens": self.total_output_tokens});
        if !self.report_true_usage {
            return legacy();
        }
        let Some(u) = truth else {
            return legacy();
        };
        let input = u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .cloned();
        let output = u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .cloned();
        let (Some(input), Some(output)) = (input, output) else {
            return legacy();
        };
        // OpenAI's input count includes its cached tokens; Anthropic's
        // `input_tokens` excludes them and the client adds the two, so the
        // OpenAI shape must subtract or cached tokens count twice. A block
        // already in Anthropic keys is exclusive and passes as is.
        let (input, cached) = if let Some(cached) = u.get("cache_read_input_tokens") {
            (input, cached.clone())
        } else {
            let cached = u
                .get("input_tokens_details")
                .or_else(|| u.get("prompt_tokens_details"))
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            match input.as_u64() {
                Some(n) => (json!(n.saturating_sub(cached)), json!(cached)),
                None => return legacy(),
            }
        };
        json!({
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": cached,
        })
    }

    /// `emit_message_delta` with a terminal usage block: provider truth
    /// when `--report-true-usage` can supply it, else the legacy shape.
    /// `abort_terminal` stays on `emit_message_delta`: a turn that died
    /// mid-stream reports nothing either way.
    fn emit_message_delta_with_usage(&self, stop_reason: &str, truth: Option<&Value>) -> String {
        crate::sse::outbound::message_delta(stop_reason, self.delta_usage(truth))
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
    /// The upstream error, handed down on the poll after the abort events.
    /// Ending cleanly instead left `stream_finisher` logging "body ended
    /// early" with no cause, which hid a day of VPN-rotation resets
    /// (2026-09-25) behind what read as the provider closing streams.
    pending_err: Option<std::io::Error>,
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn translate_openai_stream_to_anthropic(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    model: String,
    codex_limits: crate::codex_rate_limits::CodexRateLimitStore,
    quota_seen_in_headers: bool,
    outcome: Option<RoutedOutcomeContext>,
    deferred_ccr: Option<DeferredCcrBooking>,
    tool_aliases: Option<crate::routed::tool_alias::ToolAlias>,
    request_id: Option<String>,
    report_true_usage: bool,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    let mut translator = StreamTranslator::new(model)
        .with_codex_limits(codex_limits)
        .with_initial_rate_limits_seen(quota_seen_in_headers)
        .with_outcome(outcome)
        .with_tool_aliases(tool_aliases)
        .with_report_true_usage(report_true_usage)
        .with_request_id(request_id.filter(|id| !id.is_empty()));
    if let Some(deferred) = deferred_ccr {
        translator = translator.with_deferred_ccr(deferred);
    }

    translate_with_translator(stream, translator)
}

fn translate_with_translator(
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    translator: StreamTranslator,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use futures_util::StreamExt;

    futures_util::stream::unfold(
        TranslateState {
            upstream: stream,
            translator,
            buffer: String::new(),
            current_event: None,
            current_data: Vec::new(),
            finished: false,
            pending_err: None,
        },
        |mut state| async move {
            if let Some(e) = state.pending_err.take() {
                return Some((Err(e), state));
            }
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
                            request_id = %state
                                .translator
                                .outcome
                                .as_ref()
                                .map(|ctx| ctx.request_id.as_str())
                                .or(state.translator.request_id.as_deref())
                                .unwrap_or("unknown"),
                            // -1 when the lane is unknown (not Zen).
                            egress_slot = state
                                .translator
                                .outcome
                                .as_ref()
                                .and_then(|ctx| ctx.egress_slot)
                                .map_or(-1, |slot| slot as i64),
                            // Which upstream died and how old the stream was:
                            // without these a stall (no bytes for minutes) is
                            // indistinguishable from the 600s total timeout
                            // killing a slow-but-healthy turn. 2026-10-05:
                            // 538 mid-stream Body-TimedOut aborts on Oct 2 with
                            // every one of these fields unknown.
                            upstream_model = state
                                .translator
                                .outcome
                                .as_ref()
                                .map(|ctx| ctx.model.as_str())
                                .unwrap_or("unknown"),
                            routed_provider = state
                                .translator
                                .outcome
                                .as_ref()
                                .map(|ctx| ctx.provider.as_str())
                                .unwrap_or("unknown"),
                            stream_age_s = state
                                .translator
                                .outcome
                                .as_ref()
                                .map(|ctx| ctx.started_at.elapsed().as_secs_f64())
                                .unwrap_or(-1.0),
                            output_tokens = state.translator.total_output_tokens,
                            error = %e,
                            cause = ?e,
                            "routed upstream stream failed after the client had events"
                        );
                        let terminal = state.translator.abort_terminal();
                        // Either way this turn is over: the upstream will hand
                        // back the same error for as long as it is polled.
                        state.finished = true;
                        // Wrapped whole, not stringified: the source chain is
                        // what names the reset, and the finisher logs it.
                        let err = std::io::Error::other(e);
                        if terminal.is_empty() {
                            // Nothing started: no `message_start` went out, so
                            // there is no turn to close — propagate the
                            // transport error.
                            return Some((Err(err), state));
                        }
                        // The finisher owns the close either way; the error
                        // after the abort events only tells it why.
                        state.pending_err = Some(err);
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
mod tests;
