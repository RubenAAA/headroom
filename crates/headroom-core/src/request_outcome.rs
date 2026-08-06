//! `RequestOutcome`: the canonical value type for "what happened during one
//! completed proxy request" (Rust port of `headroom/proxy/outcome.py`).
//!
//! Every handler converges on building a [`RequestOutcome`] at end-of-request
//! and hands it to [`emit_request_outcome`], which owns the downstream effects
//! in a fixed order (metrics → cost tracker → request log → PERF trace) plus
//! the `output_shaper:`-label hook into the output-savings recorder.
//!
//! This standardises only the *observation* about a completed request; provider
//! APIs stay native. Provider-specific concepts (Anthropic 5m/1h cache splits,
//! OpenAI inferred-write flag, Gemini read-only cache count) are optional fields
//! with neutral (`0`/`false`) defaults so a handler that forgets a field
//! produces zeros, never silently-wrong metrics.

use std::collections::HashMap;

use serde_json::Value;

/// Round half-to-even, matching Python's built-in `round`.
fn round_half_even_i64(value: f64) -> i64 {
    value.round_ties_even() as i64
}

/// Immutable, value-equal snapshot of a completed request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOutcome {
    // ── Identity ──
    pub request_id: String,
    pub provider: String,
    pub model: String,

    /// Upstream HTTP status for this request (200 on success or response-cache
    /// hit; `Default` leaves it 0, which is likewise treated as success). When
    /// `>= 500` (e.g. a 529 Overloaded returned after retry exhaustion) the
    /// funnel records a failed request instead of feeding the savings/cost
    /// stats, so an upstream failure can't inflate the save-rate.
    pub status_code: i64,

    // ── Tokens (required — every site has these) ──
    pub original_tokens: i64,
    pub optimized_tokens: i64,
    pub output_tokens: i64,
    pub tokens_saved: i64,
    /// Compressible-portion denominator for active-savings-percent.
    pub attempted_input_tokens: i64,

    // ── Cache (provider-agnostic; unused fields stay 0) ──
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub uncached_input_tokens: i64,
    pub cache_inferred: bool,
    /// Headroom's own semantic response cache served this (distinct from
    /// upstream prompt-cache `cache_read_tokens`).
    pub from_response_cache: bool,

    // ── Timing ──
    pub total_latency_ms: f64,
    pub overhead_ms: f64,
    pub ttfb_ms: f64,
    pub pipeline_timing: Option<Vec<(String, f64)>>,

    // ── Transforms + diagnostics ──
    pub transforms_applied: Vec<String>,
    pub waste_signals: Option<Vec<(String, i64)>>,
    pub num_messages: i64,
    pub turn_id: Option<String>,
    pub request_messages: Option<Vec<Value>>,
    pub compressed_messages: Option<Vec<Value>>,
    pub tags: HashMap<String, String>,
    pub client: Option<String>,
    pub project: Option<String>,
}

impl RequestOutcome {
    /// True iff upstream reported a cache read OR the response was served from
    /// Headroom's own response cache.
    pub fn cache_hit(&self) -> bool {
        self.cache_read_tokens > 0 || self.from_response_cache
    }

    /// Cache-read share of (read + write), rounded to int percent; `0` when no
    /// cache work fired.
    pub fn cache_hit_pct(&self) -> i64 {
        let denom = self.cache_read_tokens + self.cache_write_tokens;
        if denom <= 0 {
            return 0;
        }
        round_half_even_i64(self.cache_read_tokens as f64 / denom as f64 * 100.0)
    }

    /// Compression savings as a percentage of the original request size
    /// (`tokens_saved / original_tokens * 100`).
    pub fn savings_pct(&self) -> f64 {
        if self.original_tokens <= 0 {
            return 0.0;
        }
        self.tokens_saved as f64 / self.original_tokens as f64 * 100.0
    }
}

/// Inputs to [`RequestOutcome::from_stream`], mirroring the Python classmethod's
/// keyword arguments. Neutral defaults match the Python signature.
#[derive(Default)]
pub struct StreamParams<'a> {
    pub body: &'a Value,
    pub provider: String,
    pub model: String,
    pub request_id: String,
    pub original_tokens: i64,
    pub optimized_tokens: i64,
    pub output_tokens: i64,
    pub tokens_saved: i64,
    pub transforms_applied: Vec<String>,
    pub total_latency_ms: f64,
    pub overhead_ms: f64,
    pub tags: Option<HashMap<String, String>>,
    pub client: Option<String>,
    pub log_full_messages: bool,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_5m_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub uncached_input_tokens: i64,
    pub cache_inferred: bool,
    pub ttfb_ms: f64,
    pub pipeline_timing: Option<Vec<(String, f64)>>,
    pub waste_signals: Option<Vec<(String, i64)>>,
    pub original_messages: Option<Vec<Value>>,
}

impl RequestOutcome {
    /// Construct an outcome from the locals available at streaming finalize.
    ///
    /// Centralises the six Python derivations: `attempted_input_tokens`,
    /// `num_messages`, `request_messages`/`compressed_messages` gating, `turn_id`
    /// (via [`crate::turn_id::compute_turn_id`]), transforms normalisation, and
    /// Gemini `contents` → messages normalisation for the turn hash.
    pub fn from_stream(p: StreamParams) -> RequestOutcome {
        // request_items is body["messages"], else Gemini body["contents"] (or []).
        // turn_messages is the same, but for Gemini it's the normalised
        // role/content shape the turn hash expects.
        let messages = p.body.get("messages");
        let (request_items, turn_messages): (Vec<Value>, Vec<Value>) = match messages {
            Some(Value::Array(arr)) => (arr.clone(), arr.clone()),
            Some(_) => (Vec::new(), Vec::new()),
            None => {
                // Gemini path: normalise `contents` → [{role, content}].
                match p.body.get("contents") {
                    Some(Value::Array(arr)) => {
                        let mut turn = Vec::with_capacity(arr.len());
                        for item in arr {
                            let Some(obj) = item.as_object() else {
                                continue;
                            };
                            let text = match obj.get("parts") {
                                Some(Value::Array(parts)) => parts
                                    .iter()
                                    .filter_map(|part| {
                                        part.get("text").and_then(|t| {
                                            // Python: str(part["text"]) when truthy.
                                            if t.is_null() {
                                                None
                                            } else if let Some(s) = t.as_str() {
                                                if s.is_empty() {
                                                    None
                                                } else {
                                                    Some(s.to_string())
                                                }
                                            } else {
                                                Some(value_to_str(t))
                                            }
                                        })
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            let role = if obj.get("role").and_then(Value::as_str) == Some("model") {
                                "assistant"
                            } else {
                                "user"
                            };
                            turn.push(serde_json::json!({"role": role, "content": text}));
                        }
                        (arr.clone(), turn)
                    }
                    _ => (Vec::new(), Vec::new()),
                }
            }
        };

        let system = p
            .body
            .get("system")
            .or_else(|| p.body.get("systemInstruction"))
            .cloned()
            .unwrap_or(Value::Null);

        // request_messages / compressed_messages logging policy.
        let (request_messages, compressed_messages) = if !p.log_full_messages {
            (None, None)
        } else if let Some(orig) = p.original_messages {
            (Some(orig), Some(request_items.clone()))
        } else {
            (Some(request_items.clone()), None)
        };

        let turn_id = crate::turn_id::compute_turn_id(&p.model, &system, &turn_messages);

        RequestOutcome {
            request_id: p.request_id,
            provider: p.provider,
            model: p.model,
            // Streaming finalize implies the upstream returned a 200 SSE stream.
            status_code: 200,
            original_tokens: p.original_tokens,
            optimized_tokens: p.optimized_tokens,
            output_tokens: p.output_tokens,
            tokens_saved: p.tokens_saved,
            attempted_input_tokens: p.optimized_tokens + p.tokens_saved,
            cache_read_tokens: p.cache_read_tokens,
            cache_write_tokens: p.cache_write_tokens,
            cache_write_5m_tokens: p.cache_write_5m_tokens,
            cache_write_1h_tokens: p.cache_write_1h_tokens,
            uncached_input_tokens: p.uncached_input_tokens,
            cache_inferred: p.cache_inferred,
            from_response_cache: false,
            total_latency_ms: p.total_latency_ms,
            overhead_ms: p.overhead_ms,
            ttfb_ms: p.ttfb_ms,
            pipeline_timing: p.pipeline_timing,
            transforms_applied: p.transforms_applied,
            waste_signals: p.waste_signals,
            num_messages: request_items.len() as i64,
            turn_id,
            request_messages,
            compressed_messages,
            tags: p.tags.unwrap_or_default(),
            client: p.client,
            project: None,
        }
    }
}

/// Best-effort stringify for non-string Gemini `parts[].text` values, matching
/// Python's `str(...)` on JSON scalars.
fn value_to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            // Python str(True) == "True".
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// Collapse repeated transforms into a counted summary, e.g.
/// `['a', 'a', 'b'] → "a*2 b"`. Returns `"none"` when empty. Port of
/// `cost._summarize_transforms` (insertion-ordered like Python's dict).
pub fn summarize_transforms(transforms: &[String]) -> String {
    if transforms.is_empty() {
        return "none".to_string();
    }
    let mut order: Vec<&str> = Vec::new();
    let mut counts: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for t in transforms {
        let e = counts.entry(t.as_str()).or_insert(0);
        if *e == 0 {
            order.push(t.as_str());
        }
        *e += 1;
    }
    order
        .iter()
        .map(|k| {
            let v = counts[k];
            if v > 1 {
                format!("{k}*{v}")
            } else {
                (*k).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Downstream bookkeeping surface for [`emit_request_outcome`]. Each method has
/// a default no-op so a sink implements only the effects it owns (cost tracker
/// and request logger are optional in Python — `--no-cost` / `--no-request-
/// logging`).
pub trait OutcomeSink {
    /// Step 1: Prometheus / savings-tracker.
    fn record_request(&self, outcome: &RequestOutcome);
    /// Step 2: cost dashboard (optional).
    fn record_tokens(&self, _outcome: &RequestOutcome) {}
    /// Step 3: per-request log feed (optional).
    fn log_request(&self, _outcome: &RequestOutcome) {}
    /// Output-shaping counterfactual recorder, driven by `output_shaper:` labels
    /// on `transforms_applied`. Called before step 1, matching Python.
    fn record_output_savings(&self, _transforms: &[String], _output_tokens: i64) {}
    /// Record a failed request. Invoked (instead of the success funnel) when the
    /// outcome carries an upstream `status_code >= 500`. Default no-op so sinks
    /// without a metrics surface (e.g. tests) opt out.
    fn record_failed(&self, _outcome: &RequestOutcome) {}
    /// Append to the durable savings ledger that backs `headroom savings`.
    /// Called only when the request actually saved tokens. Separate from
    /// [`OutcomeSink::record_request`] (which feeds the in-memory savings
    /// tracker) because the ledger is a flocked disk append: sinks that own a
    /// runtime should push it off the request path. Default no-op.
    fn record_savings_ledger(&self, _outcome: &RequestOutcome) {}
}

/// Single funnel for per-request bookkeeping. Preserves Python's ordering:
/// output-shaper hook → metrics → cost tracker → request log → PERF trace line.
pub fn emit_request_outcome<S: OutcomeSink + ?Sized>(sink: &S, outcome: &RequestOutcome) {
    // Upstream failure (>= 500, e.g. a 529 Overloaded surfaced after retry
    // exhaustion) must not feed the savings/cost/log success stats; that would
    // let a failed request inflate the save-rate. Record it as failed and stop.
    // 4xx stay on the normal funnel: they are client errors the proxy served.
    if outcome.status_code >= 500 {
        sink.record_failed(outcome);
        return;
    }

    // Output-shaping savings ledger, gated on `output_shaper:` labels.
    if outcome
        .transforms_applied
        .iter()
        .any(|t| t.starts_with("output_shaper:"))
    {
        sink.record_output_savings(&outcome.transforms_applied, outcome.output_tokens);
    }

    sink.record_request(outcome); // 1
                                  // Durable savings ledger, immediately after the in-memory tracker — the
                                  // same position Python writes it from inside `record_request`. Gated on a
                                  // real saving so uncompressed requests never touch the disk.
    if outcome.tokens_saved > 0 {
        sink.record_savings_ledger(outcome);
    }
    sink.record_tokens(outcome); // 2
    sink.log_request(outcome); // 3

    // 4. Structured PERF trace line. `client=X` appended only when identified.
    let client_part = outcome
        .client
        .as_deref()
        .map(|c| format!(" client={c}"))
        .unwrap_or_default();
    tracing::info!(
        target: "headroom.proxy",
        "[{}] PERF model={} msgs={} tok_before={} tok_after={} tok_saved={} \
         cache_read={} cache_write={} cache_hit_pct={} opt_ms={:.0} total_ms={:.0} \
         tok_out={} ttfb_ms={:.0} transforms={}{}",
        outcome.request_id,
        outcome.model,
        outcome.num_messages,
        outcome.original_tokens,
        outcome.optimized_tokens,
        outcome.tokens_saved,
        outcome.cache_read_tokens,
        outcome.cache_write_tokens,
        outcome.cache_hit_pct(),
        outcome.overhead_ms,
        outcome.total_latency_ms,
        outcome.output_tokens,
        outcome.ttfb_ms,
        summarize_transforms(&outcome.transforms_applied),
        client_part,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn cache_hit_from_read_or_response_cache() {
        let mut o = RequestOutcome {
            cache_read_tokens: 5,
            ..Default::default()
        };
        assert!(o.cache_hit());
        o.cache_read_tokens = 0;
        assert!(!o.cache_hit());
        o.from_response_cache = true;
        assert!(o.cache_hit());
    }

    #[test]
    fn cache_hit_pct_arithmetic() {
        let o = RequestOutcome {
            cache_read_tokens: 75,
            cache_write_tokens: 25,
            ..Default::default()
        };
        assert_eq!(o.cache_hit_pct(), 75);
        // No cache work → 0, not a divide-by-zero.
        assert_eq!(RequestOutcome::default().cache_hit_pct(), 0);
    }

    #[test]
    fn savings_pct_arithmetic() {
        let o = RequestOutcome {
            original_tokens: 1000,
            tokens_saved: 250,
            ..Default::default()
        };
        assert!((o.savings_pct() - 25.0).abs() < 1e-9);
        assert_eq!(RequestOutcome::default().savings_pct(), 0.0);
    }

    #[test]
    fn from_stream_derives_attempted_and_num_messages() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "yo"},
            ]
        });
        let o = RequestOutcome::from_stream(StreamParams {
            body: &body,
            model: "claude-sonnet-4".into(),
            optimized_tokens: 800,
            tokens_saved: 200,
            transforms_applied: vec!["a".into(), "a".into()],
            ..Default::default()
        });
        assert_eq!(o.attempted_input_tokens, 1000);
        assert_eq!(o.num_messages, 2);
        assert!(o.turn_id.is_some());
    }

    #[test]
    fn from_stream_gemini_contents_normalized() {
        // No `messages`; Gemini `contents` drives num_messages + turn hash.
        let body = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hello"}]},
                {"role": "model", "parts": [{"text": "hi there"}]},
            ],
            "systemInstruction": "be brief"
        });
        let o = RequestOutcome::from_stream(StreamParams {
            body: &body,
            model: "gemini-2.5-pro".into(),
            ..Default::default()
        });
        assert_eq!(o.num_messages, 2);
        // turn_id computed off the normalised messages (has user text).
        assert!(o.turn_id.is_some());
    }

    #[test]
    fn from_stream_log_full_messages_gating() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        // Off → both None.
        let o = RequestOutcome::from_stream(StreamParams {
            body: &body,
            log_full_messages: false,
            ..Default::default()
        });
        assert!(o.request_messages.is_none() && o.compressed_messages.is_none());
        // On, no original snapshot → sent body under request_messages only.
        let o = RequestOutcome::from_stream(StreamParams {
            body: &body,
            log_full_messages: true,
            ..Default::default()
        });
        assert!(o.request_messages.is_some() && o.compressed_messages.is_none());
        // On, with original snapshot → orig under request, sent under compressed.
        let orig = vec![json!({"role": "user", "content": "original"})];
        let o = RequestOutcome::from_stream(StreamParams {
            body: &body,
            log_full_messages: true,
            original_messages: Some(orig.clone()),
            ..Default::default()
        });
        assert_eq!(o.request_messages, Some(orig));
        assert!(o.compressed_messages.is_some());
    }

    #[test]
    fn summarize_transforms_counts_and_order() {
        assert_eq!(summarize_transforms(&[]), "none");
        let t = vec![
            "router:excluded:tool".to_string(),
            "router:excluded:tool".to_string(),
            "read_lifecycle:stale".to_string(),
        ];
        assert_eq!(
            summarize_transforms(&t),
            "router:excluded:tool*2 read_lifecycle:stale"
        );
    }

    // A recording sink that captures the funnel's call order.
    #[derive(Default)]
    struct RecordingSink {
        calls: RefCell<Vec<String>>,
    }
    impl OutcomeSink for RecordingSink {
        fn record_request(&self, _o: &RequestOutcome) {
            self.calls.borrow_mut().push("record_request".into());
        }
        fn record_tokens(&self, _o: &RequestOutcome) {
            self.calls.borrow_mut().push("record_tokens".into());
        }
        fn log_request(&self, _o: &RequestOutcome) {
            self.calls.borrow_mut().push("log_request".into());
        }
        fn record_output_savings(&self, _t: &[String], _out: i64) {
            self.calls.borrow_mut().push("record_output_savings".into());
        }
        fn record_failed(&self, _o: &RequestOutcome) {
            self.calls.borrow_mut().push("record_failed".into());
        }
        fn record_savings_ledger(&self, _o: &RequestOutcome) {
            self.calls.borrow_mut().push("record_savings_ledger".into());
        }
    }

    #[test]
    fn emit_writes_savings_ledger_when_tokens_were_saved() {
        let sink = RecordingSink::default();
        let o = RequestOutcome {
            status_code: 200,
            original_tokens: 1000,
            optimized_tokens: 600,
            tokens_saved: 400,
            ..Default::default()
        };
        emit_request_outcome(&sink, &o);
        let calls = sink.calls.borrow();
        assert!(
            calls.contains(&"record_savings_ledger".to_string()),
            "a compressed request must reach the durable ledger: {calls:?}"
        );
        // Ordering matters: the ledger write follows the in-memory tracker,
        // the same position Python writes it from.
        let tracker = calls.iter().position(|c| c == "record_request").unwrap();
        let ledger = calls
            .iter()
            .position(|c| c == "record_savings_ledger")
            .unwrap();
        assert!(ledger > tracker, "ledger must follow record_request");
    }

    #[test]
    fn emit_skips_savings_ledger_when_nothing_was_saved() {
        let sink = RecordingSink::default();
        let o = RequestOutcome {
            status_code: 200,
            original_tokens: 1000,
            optimized_tokens: 1000,
            tokens_saved: 0,
            ..Default::default()
        };
        emit_request_outcome(&sink, &o);
        assert!(
            !sink
                .calls
                .borrow()
                .contains(&"record_savings_ledger".to_string()),
            "an uncompressed request must not touch the disk"
        );
    }

    #[test]
    fn emit_5xx_records_failed_and_skips_success_funnel() {
        for status in [500, 503, 529] {
            let sink = RecordingSink::default();
            let o = RequestOutcome {
                status_code: status,
                ..Default::default()
            };
            emit_request_outcome(&sink, &o);
            // Only the failure path fires; the success funnel is skipped.
            assert_eq!(*sink.calls.borrow(), vec!["record_failed"]);
        }
    }

    #[test]
    fn emit_4xx_stays_on_success_funnel() {
        let sink = RecordingSink::default();
        let o = RequestOutcome {
            status_code: 429,
            ..Default::default()
        };
        emit_request_outcome(&sink, &o);
        assert_eq!(
            *sink.calls.borrow(),
            vec!["record_request", "record_tokens", "log_request"]
        );
    }

    #[test]
    fn emit_order_without_output_shaper() {
        let sink = RecordingSink::default();
        emit_request_outcome(&sink, &RequestOutcome::default());
        assert_eq!(
            *sink.calls.borrow(),
            vec!["record_request", "record_tokens", "log_request"]
        );
    }

    #[test]
    fn emit_output_shaper_hook_fires_first() {
        let sink = RecordingSink::default();
        let o = RequestOutcome {
            transforms_applied: vec!["output_shaper:arm=a:stratum=x".into()],
            ..Default::default()
        };
        emit_request_outcome(&sink, &o);
        assert_eq!(
            *sink.calls.borrow(),
            vec![
                "record_output_savings",
                "record_request",
                "record_tokens",
                "log_request"
            ]
        );
    }

    #[test]
    fn emit_default_sink_methods_are_noops() {
        // A sink implementing only the required method must still run cleanly.
        struct Minimal;
        impl OutcomeSink for Minimal {
            fn record_request(&self, _o: &RequestOutcome) {}
        }
        emit_request_outcome(&Minimal, &RequestOutcome::default());
    }
}
