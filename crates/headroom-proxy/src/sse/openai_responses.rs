//! OpenAI Responses streaming state machine.
//!
//! Per OpenAI's Responses streaming spec (and §5.3 of the Headroom
//! realignment guide), the Responses stream uses **named events**
//! (an `event:` SSE line per event), unlike Chat Completions. Each
//! event corresponds to a structured update on the response object.
//!
//! Top-level events we handle:
//!
//!   response.created                 — initial envelope; carries `response.id`.
//!   response.in_progress             — periodic status; usually no state change.
//!   output_item.added                — a new output item appears at `item.id`.
//!   output_item.done                 — that item is complete (final shape in
//!                                      `item`). Items can complete in any order
//!                                      (parallel reasoning + function_call).
//!   content_part.added               — a content_part entry under an item.
//!   content_part.done                — that content_part is complete.
//!   output_text.delta                — text fragment to append to a message item.
//!   output_text.done                 — text is finished.
//!   function_call_arguments.delta    — fragment of a function_call's `arguments`
//!                                      string. STAYS A STRING end-to-end.
//!   function_call_arguments.done     — done; `arguments` final value.
//!   reasoning_summary.delta          — fragment of a reasoning summary.
//!   reasoning_summary.done           — summary complete.
//!   response.completed               — final usage + status.
//!   response.failed                  — error; status = Errored.
//!   response.incomplete              — incomplete (usually max_output_tokens).
//!
//! P1-17 telemetry: items keyed by position broke when OpenAI's spec
//! permitted out-of-order completion (item 1 completed before item 0).
//! This module keys EVERYTHING by `item.id` (a string like
//! `msg_abc123`), never by position. Test
//! `out_of_order_item_completion_by_id` enforces this.

use serde_json::Value;
use std::collections::HashMap;

use super::framing::SseEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamStatus {
    #[default]
    Open,
    Completed,
    Failed,
    Incomplete,
}

#[derive(Debug, Default, Clone)]
pub struct ResponseState {
    pub response_id: Option<String>,
    pub model: Option<String>,
    /// Items keyed by their wire `id` field (NOT position). Out-of-
    /// order completion is allowed by spec.
    pub items: HashMap<String, ItemState>,
    /// First-seen order of item ids. The HashMap above loses arrival
    /// order, but the CCR continuation fold must materialize `output[]`
    /// in the order the upstream sent it (text leads the call it
    /// introduces). Pushed on first insert of each id.
    pub order: Vec<String>,
    /// Authoritative `output[]` from the `response.completed` envelope,
    /// when the upstream sent one. Wins over the incrementally gathered
    /// items, mirroring the old `responses_stream_to_turn` fold.
    pub completed_output: Option<Vec<Value>>,
    pub usage: Option<Value>,
    pub status: StreamStatus,
    /// Phase G PR-G3: `service_tier` extracted from the
    /// `response.completed` envelope. Drives
    /// `proxy_service_tier_count_total`.
    pub service_tier: Option<String>,
    /// Phase G PR-G3: `incomplete_details.reason` when
    /// `status == "incomplete"`. Paired with the
    /// `proxy_response_status_count_total{status="incomplete"}` log
    /// line so operators see WHY a stream landed in `incomplete`.
    pub incomplete_reason: Option<String>,
}

impl ResponseState {
    /// Phase G PR-G3: stable terminal-status string for the metrics
    /// label vocabulary. Returns `None` when the stream is still
    /// `Open` — caller skips the counter increment in that case.
    pub fn terminal_status(&self) -> Option<&'static str> {
        use crate::observability::metric_names::response_status::{COMPLETED, FAILED, INCOMPLETE};
        match self.status {
            StreamStatus::Completed => Some(COMPLETED),
            StreamStatus::Failed => Some(FAILED),
            StreamStatus::Incomplete => Some(INCOMPLETE),
            StreamStatus::Open => None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ItemState {
    pub item_type: String,
    /// Concatenated `output_text` for message-type items.
    pub output_text: String,
    /// Concatenated `reasoning_summary` deltas.
    pub reasoning_summary: String,
    /// Concatenated `function_call_arguments`. Stays as a STRING —
    /// arguments may be JSON but the proxy never re-parses it.
    pub function_call_arguments: String,
    /// Initial metadata captured at output_item.added.
    pub metadata: Value,
    /// True after output_item.done for this id.
    pub complete: bool,
    /// Per-content_part state, keyed by part's `index`. We do not
    /// presently surface fields from the part beyond what the Anthropic
    /// state-machine surfaces — this keeps the structure small.
    pub content_parts: HashMap<usize, ContentPartState>,
}

#[derive(Debug, Default, Clone)]
pub struct ContentPartState {
    pub part_type: String,
    pub text: String,
    pub complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("event payload is not valid UTF-8: {0}")]
    PayloadNotUtf8(#[from] std::str::Utf8Error),
    #[error("event payload is not valid JSON: {0}")]
    PayloadNotJson(#[from] serde_json::Error),
    #[error("event missing required field {field}")]
    MissingField { field: &'static str },
}

impl ResponseState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one framed event. The `event:` line is required (the
    /// Responses spec mandates one per event); a None name is logged
    /// and dropped.
    pub fn apply(&mut self, event: SseEvent) -> Result<(), StateError> {
        let Some(name) = event.event_name.as_deref() else {
            tracing::warn!(
                event = "sse_unknown_event",
                provider = "openai_responses",
                event_name = "<missing>",
                payload_preview = %payload_preview(&event.data),
                "openai responses event missing event: line; dropping"
            );
            return Ok(());
        };

        // OpenAI-compatible gateways may send a `ping` heartbeat between
        // Responses events. It carries no response state, but is still
        // forwarded by the stream path. Keep it out of the unknown-event
        // warnings while leaving a distinct breadcrumb available at debug.
        if name == "ping" {
            tracing::debug!(
                event = "sse_heartbeat",
                provider = "openai_responses",
                event_name = name,
                "received OpenAI Responses heartbeat"
            );
            return Ok(());
        }

        let v: Value = parse_json(&event.data)?;
        match name {
            "response.created" => self.on_response_created(&v),
            "response.in_progress" => Ok(()),
            // Both spellings exist in the wild: OpenAI documents the
            // `response.`-prefixed form, but Codex and several
            // OpenAI-compatible gateways (plus this repo's own
            // synthesized truncation streams) emit the bare form.
            "response.output_item.added" | "output_item.added" => self.on_output_item_added(&v),
            "response.output_item.done" | "output_item.done" => self.on_output_item_done(&v),
            "response.content_part.added" | "content_part.added" => self.on_content_part_added(&v),
            "response.content_part.done" | "content_part.done" => self.on_content_part_done(&v),
            "response.output_text.delta" | "output_text.delta" => self.on_output_text_delta(&v),
            "response.output_text.done" | "output_text.done" => self.on_output_text_done(&v),
            "response.function_call_arguments.delta" | "function_call_arguments.delta" => {
                self.on_function_call_arguments_delta(&v)
            }
            "response.function_call_arguments.done" | "function_call_arguments.done" => {
                self.on_function_call_arguments_done(&v)
            }
            "response.reasoning_summary.delta"
            | "response.reasoning_summary_text.delta"
            | "reasoning_summary.delta" => self.on_reasoning_summary_delta(&v),
            "response.reasoning_summary.done"
            | "response.reasoning_summary_text.done"
            | "reasoning_summary.done" => self.on_reasoning_summary_done(&v),
            // GPT-5.6 / Codex emit part boundaries around the summary
            // deltas the arms above already accumulate. Logged as
            // `sse_unknown_event` on 2026-09-22 request
            // `6ffbe940-22a6-4637-9c39-068a1f73539f` (two events, same
            // timestamp as the empty fold). Treating them as unknown
            // does not collect text; recognising them stops the warn
            // and matches the live translator, which uses added as a
            // thinking-block boundary and ignores done.
            "response.reasoning_summary_part.added"
            | "reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "reasoning_summary_part.done" => Ok(()),
            "response.completed" => self.on_response_completed(&v),
            "response.failed" => {
                self.status = StreamStatus::Failed;
                // Phase G PR-G3: capture `service_tier` from the
                // failed envelope too — the proxy still wants the
                // tier dimension on failed responses so dashboards
                // can attribute failure rate to tiers.
                self.capture_envelope_metadata(&v);
                Ok(())
            }
            "response.incomplete" => {
                self.status = StreamStatus::Incomplete;
                // Phase G PR-G3: pick up incomplete_details.reason
                // from the envelope. Spec field name is
                // `incomplete_details.reason`; we tolerate both
                // the top-level and the nested location, since the
                // OpenAI shape has shifted across SDK versions.
                if let Some(resp) = v.get("response") {
                    if let Some(reason) = resp
                        .get("incomplete_details")
                        .and_then(|d| d.get("reason"))
                        .and_then(|x| x.as_str())
                    {
                        self.incomplete_reason = Some(reason.to_string());
                    }
                }
                self.capture_envelope_metadata(&v);
                Ok(())
            }
            other => {
                tracing::warn!(
                    event = "sse_unknown_event",
                    provider = "openai_responses",
                    event_name = other,
                    payload_preview = %payload_preview(&event.data),
                    "unknown openai responses event; preserving stream but not updating state"
                );
                Ok(())
            }
        }
    }

    fn on_response_created(&mut self, v: &Value) -> Result<(), StateError> {
        let resp = v
            .get("response")
            .ok_or(StateError::MissingField { field: "response" })?;
        if let Some(id) = resp.get("id").and_then(|x| x.as_str()) {
            self.response_id = Some(id.to_string());
        }
        if let Some(m) = resp.get("model").and_then(|x| x.as_str()) {
            self.model = Some(m.to_string());
        }
        Ok(())
    }

    fn note_seen(&mut self, id: &str) {
        if !self.order.iter().any(|s| s == id) {
            self.order.push(id.to_string());
        }
    }

    /// Key for an output item: the wire `id` when present, else the
    /// function-call `call_id` (minimal test fixtures and some gateways
    /// omit `id`). Returns None only when neither exists.
    fn item_key(item: &Value) -> Option<String> {
        item.get("id")
            .and_then(|x| x.as_str())
            .or_else(|| item.get("call_id").and_then(|x| x.as_str()))
            .map(str::to_string)
    }

    /// Find the map key of an existing entry whose metadata carries
    /// `call_id == target` (for correlating an id-less done/added with
    /// an earlier added stored under its item id).
    fn key_by_call_id(&self, target: &str) -> Option<String> {
        self.items.iter().find_map(|(k, state)| {
            state
                .metadata
                .get("call_id")
                .and_then(|v| v.as_str())
                .filter(|c| *c == target)
                .map(|_| k.clone())
        })
    }

    fn on_output_item_added(&mut self, v: &Value) -> Result<(), StateError> {
        let item = v
            .get("item")
            .ok_or(StateError::MissingField { field: "item" })?;
        // Keyless shells carry nothing the fold needs (the old fold ignored
        // added entirely); skip rather than minting a stray empty entry.
        let Some(id) = Self::item_key(item) else {
            return Ok(());
        };
        let has_wire_id = item.get("id").and_then(|x| x.as_str()).is_some();
        let key = if !has_wire_id {
            self.key_by_call_id(&id).unwrap_or_else(|| id.clone())
        } else {
            id.clone()
        };
        let item_type = item
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        self.note_seen(&key);
        // Preserve deltas that arrived before the added (out-of-order):
        // only refresh the shell, never wipe accumulated arguments/text.
        let entry = self.items.entry(key).or_default();
        entry.item_type = item_type;
        entry.metadata = item.clone();
        Ok(())
    }

    fn on_output_item_done(&mut self, v: &Value) -> Result<(), StateError> {
        let item = v
            .get("item")
            .ok_or(StateError::MissingField { field: "item" })?;
        // Keyless dones (bare `{"type":"message",…}` fixtures) still carry
        // the turn's content; keep each under a unique key so none is lost.
        let Some(id) = Self::item_key(item) else {
            let generated = format!("__done_{}", self.order.len());
            self.note_seen(&generated);
            self.items.insert(
                generated,
                ItemState {
                    item_type: item
                        .get("type")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    metadata: item.clone(),
                    complete: true,
                    ..Default::default()
                },
            );
            return Ok(());
        };
        let has_wire_id = item.get("id").and_then(|x| x.as_str()).is_some();
        // An id-less done whose call_id matches an earlier added (stored
        // under its wire id) updates that entry rather than forking a
        // duplicate under the call_id.
        let key = if !has_wire_id {
            self.key_by_call_id(&id).unwrap_or_else(|| id.clone())
        } else {
            id.clone()
        };
        self.note_seen(&key);
        // Out-of-order completion: the item may not have an
        // output_item.added if the producer is exotic; insert-or-update.
        let entry = self.items.entry(key).or_default();
        entry.complete = true;
        // Refresh metadata to the final shape — the `done` payload
        // is authoritative.
        entry.metadata = item.clone();
        if entry.item_type.is_empty() {
            if let Some(t) = item.get("type").and_then(|x| x.as_str()) {
                entry.item_type = t.to_string();
            }
        }
        Ok(())
    }

    fn on_content_part_added(&mut self, v: &Value) -> Result<(), StateError> {
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "item_id" })?
            .to_string();
        self.note_seen(&item_id);
        let part_index = v
            .get("content_index")
            .or_else(|| v.get("part_index"))
            .and_then(|x| x.as_u64())
            .ok_or(StateError::MissingField {
                field: "content_index",
            })? as usize;
        let part = v
            .get("part")
            .ok_or(StateError::MissingField { field: "part" })?;
        let part_type = part
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let item = self.items.entry(item_id).or_default();
        item.content_parts.insert(
            part_index,
            ContentPartState {
                part_type,
                ..Default::default()
            },
        );
        Ok(())
    }

    fn on_content_part_done(&mut self, v: &Value) -> Result<(), StateError> {
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "item_id" })?;
        let part_index = v
            .get("content_index")
            .or_else(|| v.get("part_index"))
            .and_then(|x| x.as_u64())
            .ok_or(StateError::MissingField {
                field: "content_index",
            })? as usize;
        if let Some(item) = self.items.get_mut(item_id) {
            if let Some(part) = item.content_parts.get_mut(&part_index) {
                part.complete = true;
            }
        }
        Ok(())
    }

    fn on_output_text_delta(&mut self, v: &Value) -> Result<(), StateError> {
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "item_id" })?
            .to_string();
        self.note_seen(&item_id);
        let delta = v
            .get("delta")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "delta" })?;
        let item = self.items.entry(item_id).or_default();
        item.output_text.push_str(delta);
        // Also append to the addressed content_part if present.
        if let Some(part_index) = v
            .get("content_index")
            .or_else(|| v.get("part_index"))
            .and_then(|x| x.as_u64())
        {
            if let Some(part) = item.content_parts.get_mut(&(part_index as usize)) {
                part.text.push_str(delta);
            }
        }
        Ok(())
    }

    fn on_output_text_done(&mut self, _v: &Value) -> Result<(), StateError> {
        // The `done` event carries the final aggregated `text`. We
        // could overwrite output_text from this, but the delta-sum
        // is already authoritative and matches the wire — we keep it
        // for robustness.
        Ok(())
    }

    fn on_function_call_arguments_delta(&mut self, v: &Value) -> Result<(), StateError> {
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "item_id" })?
            .to_string();
        self.note_seen(&item_id);
        let delta = v
            .get("delta")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "delta" })?;
        let item = self.items.entry(item_id).or_default();
        item.function_call_arguments.push_str(delta);
        Ok(())
    }

    fn on_function_call_arguments_done(&mut self, v: &Value) -> Result<(), StateError> {
        // The done event carries the final `arguments` string. Replace
        // our accumulator with it iff present and nonempty — guarantees
        // a producer that sends ONLY the done event (no deltas) still
        // ends up with the right value.
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "item_id" })?
            .to_string();
        self.note_seen(&item_id);
        if let Some(args) = v.get("arguments").and_then(|x| x.as_str()) {
            let item = self.items.entry(item_id).or_default();
            // Only overwrite if our delta accumulator is empty;
            // otherwise the deltas are authoritative (they may
            // include trailing content the `done` shape elides).
            if item.function_call_arguments.is_empty() {
                item.function_call_arguments.push_str(args);
            }
        }
        Ok(())
    }

    fn on_reasoning_summary_delta(&mut self, v: &Value) -> Result<(), StateError> {
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "item_id" })?
            .to_string();
        self.note_seen(&item_id);
        let delta = v
            .get("delta")
            .and_then(|x| x.as_str())
            .ok_or(StateError::MissingField { field: "delta" })?;
        let item = self.items.entry(item_id).or_default();
        item.reasoning_summary.push_str(delta);
        Ok(())
    }

    fn on_reasoning_summary_done(&mut self, _v: &Value) -> Result<(), StateError> {
        Ok(())
    }

    fn on_response_completed(&mut self, v: &Value) -> Result<(), StateError> {
        self.status = StreamStatus::Completed;
        if let Some(resp) = v.get("response") {
            if let Some(usage) = resp.get("usage") {
                if !usage.is_null() {
                    self.usage = Some(usage.clone());
                }
            }
            // The completed envelope carries the finished `output[]`, which
            // is authoritative over the incrementally gathered items (a call
            // whose `output_item.done` never arrived is still in here).
            // An empty `output: []` is not that: on 2026-09-22 request
            // `6ffbe940-22a6-4637-9c39-068a1f73539f` a 186544-byte
            // `response.completed` stream folded to zero blocks because
            // an empty array here won over items already gathered from
            // deltas / `output_item.done`. Treat empty as omitted.
            if let Some(items) = resp.get("output").and_then(|o| o.as_array()) {
                if !items.is_empty() {
                    self.completed_output = Some(items.clone());
                }
            }
            if let Some(id) = resp.get("id").and_then(|x| x.as_str()) {
                self.response_id = Some(id.to_string());
            }
        }
        self.capture_envelope_metadata(v);
        Ok(())
    }

    /// Materialize the buffered `output[]` turn the CCR machinery speaks.
    ///
    /// Prefers the authoritative `output[]` from the `response.completed`
    /// envelope when the upstream sent a non-empty one; an empty
    /// `output: []` is treated as omitted so incrementally gathered
    /// items survive. Otherwise rebuilds items from the incremental
    /// events (added / deltas / done) in first-seen order.
    /// Returns the turn and the output-token count the outcome is booked with.
    pub fn to_responses_turn(&self) -> (Value, u64) {
        let output_tokens = self
            .usage
            .as_ref()
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let usage = self.usage.clone().unwrap_or_else(|| {
            serde_json::json!({
                "input_tokens": 0,
                "output_tokens": output_tokens,
            })
        });
        if let Some(items) = self.completed_output.as_ref() {
            let mut turn = serde_json::json!({
                "output": items,
                "usage": usage,
            });
            if let Some(id) = self.response_id.as_deref() {
                turn["id"] = Value::String(id.to_string());
            }
            return (turn, output_tokens);
        }
        let mut output_items: Vec<Value> = Vec::new();
        for id in &self.order {
            let Some(item) = self.items.get(id) else {
                continue;
            };
            let item_type = if item.item_type.is_empty() {
                item.metadata
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
            } else {
                item.item_type.as_str()
            };
            match item_type {
                "function_call" => {
                    // Start from the authoritative final shape when we have
                    // it (output_item.done), else the added shell; fill in
                    // arguments from the accumulated deltas when missing.
                    let mut merged = if item.metadata.is_object() {
                        item.metadata.clone()
                    } else {
                        serde_json::json!({"type": "function_call", "id": id})
                    };
                    let has_args = merged
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .is_some_and(|s| !s.is_empty());
                    if !has_args && !item.function_call_arguments.is_empty() {
                        merged["arguments"] = Value::String(item.function_call_arguments.clone());
                    }
                    // Ensure the round-trip keys exist: CCR keys off
                    // `call_id` first, then `id`.
                    if merged.get("id").is_none() {
                        merged["id"] = Value::String(id.clone());
                    }
                    output_items.push(merged);
                }
                "message" => {
                    // A finished message item carries the whole answer; use
                    // it verbatim. Otherwise rebuild from the text deltas.
                    let complete_message = item.complete
                        && item
                            .metadata
                            .get("content")
                            .and_then(|c| c.as_array())
                            .is_some_and(|c| !c.is_empty());
                    if complete_message {
                        output_items.push(item.metadata.clone());
                    } else if !item.output_text.is_empty() {
                        let mut msg = if item.metadata.is_object() {
                            item.metadata.clone()
                        } else {
                            serde_json::json!({
                                "type": "message",
                                "id": id,
                                "role": "assistant",
                            })
                        };
                        msg["content"] = serde_json::json!([{
                            "type": "output_text",
                            "text": item.output_text,
                        }]);
                        output_items.push(msg);
                    } else if item.metadata.is_object() {
                        output_items.push(item.metadata.clone());
                    }
                }
                _ => {
                    if item.metadata.is_object() {
                        output_items.push(item.metadata.clone());
                    } else if !item.output_text.is_empty() {
                        output_items.push(serde_json::json!({
                            "type": "message",
                            "id": id,
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": item.output_text,
                            }],
                        }));
                    }
                }
            }
        }
        let mut turn = serde_json::json!({
            "output": output_items,
            "usage": usage,
        });
        if let Some(id) = self.response_id.as_deref() {
            turn["id"] = Value::String(id.to_string());
        }
        (turn, output_tokens)
    }

    /// Phase G PR-G3: extract `service_tier` and (when present)
    /// `incomplete_details.reason` from a top-level
    /// `response.{completed,failed,incomplete}` event payload. The
    /// `service_tier` field is on the `response` envelope on every
    /// terminal event per the OpenAI Responses API spec; we keep
    /// the most-recent value (later events override earlier ones).
    fn capture_envelope_metadata(&mut self, v: &Value) {
        if let Some(resp) = v.get("response") {
            if let Some(tier) = resp.get("service_tier").and_then(|x| x.as_str()) {
                self.service_tier = Some(tier.to_string());
            }
            // Best-effort pull of incomplete_details.reason from the
            // completed/failed envelope (in addition to the dedicated
            // arm in `apply` for `response.incomplete`).
            if let Some(reason) = resp
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(|x| x.as_str())
            {
                self.incomplete_reason = Some(reason.to_string());
            }
        }
    }
}

fn parse_json(data: &bytes::Bytes) -> Result<Value, StateError> {
    let s = std::str::from_utf8(data)?;
    Ok(serde_json::from_str(s)?)
}

fn payload_preview(data: &bytes::Bytes) -> String {
    const LIMIT: usize = 96;
    let slice = if data.len() > LIMIT {
        &data[..LIMIT]
    } else {
        &data[..]
    };
    String::from_utf8_lossy(slice).into_owned()
}
