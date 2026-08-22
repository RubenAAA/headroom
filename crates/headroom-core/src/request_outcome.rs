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

    /// Number of times this body was transmitted upstream. Zero means the
    /// caller did not report it and is normalized to one on the failure path.
    pub upstream_attempts: i64,
    /// Provider-reported usage, when an error response actually carried it.
    /// These stay optional so the forwarded estimate below is never presented
    /// as billed usage merely because the provider omitted a usage block.
    pub provider_input_tokens: Option<i64>,
    pub provider_output_tokens: Option<i64>,

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

    /// Rate class the removed input would have occupied on this request.
    ///
    /// Prompt caches are prefixes. `optimized_tokens` is the selected span
    /// left after compression; if that span is larger than the request's
    /// cache-write + uncached tail, it reaches into the cache-read prefix and
    /// the removed tokens are priced at the cache-read rate. This deliberately
    /// favours the proxy at the boundary: a span that fits in the fresh region
    /// is wholly called fresh, even if some selected blocks came from earlier.
    pub fn compression_savings_cost_basis(&self) -> &'static str {
        let fresh_region = self
            .cache_write_tokens
            .max(0)
            .saturating_add(self.uncached_input_tokens.max(0));
        if self.cache_read_tokens > 0 && self.optimized_tokens > fresh_region {
            "cache_read"
        } else {
            "fresh_input"
        }
    }

    /// Dollar counterfactual for this turn's removed input, using the rate
    /// class selected by [`Self::compression_savings_cost_basis`].
    pub fn compression_savings_cost_usd(&self) -> f64 {
        if self.tokens_saved <= 0 {
            return 0.0;
        }
        let fallback = crate::savings_ledger::DEFAULT_FALLBACK_INPUT_COST_PER_TOKEN;
        let rate = match crate::pricing::lookup(&self.model) {
            Some(pricing) if self.compression_savings_cost_basis() == "cache_read" => pricing
                .cache_read_cost_per_token
                .unwrap_or(pricing.input_cost_per_token),
            Some(pricing) => pricing.input_cost_per_token,
            None => fallback,
        };
        self.tokens_saved as f64 * rate
    }

    /// Tokens the forwarded request grew by, if it ended up larger.
    ///
    /// `tokens_saved` is clamped at zero, so a request that leaves the proxy
    /// *bigger* than it arrived is indistinguishable from one the proxy simply
    /// could not compress: both report `tok_saved=0`. That ambiguity hides real
    /// regressions — anything that adds to the body after compression
    /// (proactive context expansion, memory injection) can outweigh the
    /// compression it sits on top of and still look like a neutral turn.
    ///
    /// Diagnostic only: it deliberately does not feed `tokens_saved` or
    /// `attempted_input_tokens`, because `attempted_input_tokens =
    /// optimized_tokens + tokens_saved` is a size, not a signed delta, and
    /// because injection paths already book their own cost through the
    /// retrieval-drawback channel — letting a negative land here too would
    /// count it twice.
    pub fn tokens_inflated(&self) -> i64 {
        (self.optimized_tokens - self.original_tokens).max(0)
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
            upstream_attempts: 1,
            provider_input_tokens: None,
            provider_output_tokens: None,
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
    /// Record a failed request. Invoked (instead of the success funnel) for a
    /// generic upstream `status_code >= 500`, or when a caller explicitly
    /// identifies a forwarded upstream rejection. Default no-op so sinks
    /// without a metrics surface (e.g. tests) opt out.
    fn record_failed(&self, _outcome: &RequestOutcome) {}
    /// Append to the durable savings ledger that backs `headroom savings`.
    /// Called only when the request actually saved tokens. Separate from
    /// [`OutcomeSink::record_request`] (which feeds the in-memory savings
    /// tracker) because the ledger is a flocked disk append: sinks that own a
    /// runtime should push it off the request path. Default no-op.
    fn record_savings_ledger(&self, _outcome: &RequestOutcome) {}
    /// Persist a prefix-cache outcome observed on the response side.
    ///
    /// `reason` is one of `ttl_expiry`, `prefix_change` or `unknown`;
    /// `wasted_tokens` is the re-created prefix and is non-zero only for
    /// `prefix_change`. Separate from [`OutcomeSink::record_request`] because
    /// a bust is detected a turn after the request that caused it, and it is
    /// spelled in primitives so the detector's own types can stay in the proxy
    /// crate. Default no-op.
    fn record_cache_outcome(&self, _provider: &str, _reason: &str, _wasted_tokens: i64) {}
}

/// Send a caller-identified upstream failure through the failure-only sink.
///
/// This is deliberately status-agnostic: forwarding code knows that a 401 or
/// 429 came from the upstream provider and represents failed work, while an
/// arbitrary generic 4xx `RequestOutcome` can still describe a normal client
/// error. Keeping the shared logging here also guarantees one failure record
/// and no success/PERF/savings side effects.
pub fn emit_failed_request_outcome<S: OutcomeSink + ?Sized>(sink: &S, outcome: &RequestOutcome) {
    sink.record_failed(outcome);
    tracing::warn!(
        target: "headroom.proxy",
        event = "request_failed_accounting",
        request_id = %outcome.request_id,
        provider = %outcome.provider,
        model = %outcome.model,
        status_code = outcome.status_code,
        upstream_attempts = outcome.upstream_attempts.max(1),
        num_messages = outcome.num_messages,
        original_tokens = outcome.original_tokens,
        forwarded_tokens = outcome.optimized_tokens,
        forwarded_tokens_at_risk = outcome
            .optimized_tokens
            .max(0)
            .saturating_mul(outcome.upstream_attempts.max(1)),
        provider_input_tokens = ?outcome.provider_input_tokens,
        provider_output_tokens = ?outcome.provider_output_tokens,
        output_tokens = outcome.output_tokens,
        tokens_saved_not_booked = outcome.tokens_saved,
        cache_read_tokens = outcome.cache_read_tokens,
        cache_write_tokens = outcome.cache_write_tokens,
        total_ms = outcome.total_latency_ms,
        transforms = %summarize_transforms(&outcome.transforms_applied),
        "upstream failed: turn excluded from successful savings, cost and PERF stats; failed work booked separately"
    );
}

/// Single funnel for per-request bookkeeping. Preserves Python's ordering:
/// output-shaper hook → metrics → cost tracker → request log → PERF trace line.
pub fn emit_request_outcome<S: OutcomeSink + ?Sized>(sink: &S, outcome: &RequestOutcome) {
    // Upstream failure (>= 500, e.g. a 529 Overloaded surfaced after retry
    // exhaustion) must not feed the savings/cost/log success stats; that would
    // let a failed request inflate the save-rate. Record it as failed and stop.
    // 4xx stay on the normal funnel: they are client errors the proxy served.
    if outcome.status_code >= 500 {
        emit_failed_request_outcome(sink, outcome);
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
         tok_inflated={} \
         cache_read={} cache_write={} cache_hit_pct={} opt_ms={:.0} total_ms={:.0} \
         tok_out={} ttfb_ms={:.0} transforms={}{}",
        outcome.request_id,
        outcome.model,
        outcome.num_messages,
        outcome.original_tokens,
        outcome.optimized_tokens,
        outcome.tokens_saved,
        outcome.tokens_inflated(),
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
    fn compression_savings_uses_cache_read_rate_inside_cached_prefix() {
        let outcome = RequestOutcome {
            model: "claude-opus-5".into(),
            tokens_saved: 1_000,
            optimized_tokens: 2_176,
            cache_read_tokens: 480_000,
            cache_write_tokens: 1_013,
            uncached_input_tokens: 2,
            ..Default::default()
        };

        assert_eq!(outcome.compression_savings_cost_basis(), "cache_read");
        // 1,000 saved tokens at Opus 5's cache-read rate of $0.50/MTok.
        assert!((outcome.compression_savings_cost_usd() - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn compression_savings_uses_fresh_rate_past_cache_boundary() {
        let outcome = RequestOutcome {
            model: "claude-opus-5".into(),
            tokens_saved: 1_000,
            optimized_tokens: 900,
            cache_read_tokens: 10_000,
            cache_write_tokens: 4_000,
            uncached_input_tokens: 2,
            ..Default::default()
        };

        assert_eq!(outcome.compression_savings_cost_basis(), "fresh_input");
        // 1,000 saved tokens at Opus 5's fresh-input rate of $5/MTok.
        assert!((outcome.compression_savings_cost_usd() - 0.005).abs() < 1e-12);
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
    fn explicit_forwarded_rejections_record_4xx_and_5xx_once() {
        for status in [401, 429, 503] {
            let sink = RecordingSink::default();
            let outcome = RequestOutcome {
                status_code: status,
                ..Default::default()
            };
            emit_failed_request_outcome(&sink, &outcome);
            assert_eq!(
                *sink.calls.borrow(),
                vec!["record_failed"],
                "status {status} must reach only the failed-work sink"
            );
        }
    }

    /// Make every callsite in this binary emit, whichever test reaches it
    /// first.
    ///
    /// `tracing` settles a callsite's `Interest` the first time that call site
    /// fires and caches it for the life of the process. Several tests here
    /// reach the `request_failed_accounting` callsite with no subscriber
    /// installed, and the no-op subscriber answers `Interest::never` — after
    /// which the macro short-circuits before building the event and no
    /// subscriber ever sees it again. That is a race on test order, and it
    /// cost roughly 4% of runs.
    ///
    /// `with_default` cannot prevent it: it sets a thread-local dispatcher
    /// that the global rebuild path never walks. Rebuilding the cache is not
    /// enough either — another thread can re-register the callsite against no
    /// subscriber in the gap before the event fires, which is why that fix
    /// left the failure rate unchanged.
    ///
    /// Registering a permissive global dispatcher does hold. Every later
    /// registration answers against it rather than the no-op subscriber, so
    /// the callsite cannot be poisoned again; the rebuild then clears any
    /// poisoning from before this ran. Events still reach whatever
    /// `with_default` subscriber a test installs on its own thread.
    fn permit_every_callsite() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Fails only if something else already claimed the global slot,
            // which is just as good for our purpose.
            let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
            tracing::callsite::rebuild_interest_cache();
        });
    }

    /// Item 6: a failed turn must leave a trace naming what it cost. Skipping
    /// the success funnel is correct; skipping it *silently* is what made the
    /// savings figures improve as behaviour got worse.
    #[test]
    fn emit_5xx_logs_what_the_failed_turn_forwarded() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};

        #[derive(Default)]
        struct Sink(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Sink {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct V(String);
                impl Visit for V {
                    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
                        self.0.push_str(&format!("{}={:?} ", f.name(), v));
                    }
                    fn record_i64(&mut self, f: &Field, v: i64) {
                        self.0.push_str(&format!("{}={} ", f.name(), v));
                    }
                    fn record_str(&mut self, f: &Field, v: &str) {
                        self.0.push_str(&format!("{}={} ", f.name(), v));
                    }
                }
                let mut v = V(String::new());
                event.record(&mut v);
                self.0.lock().unwrap().push(v.0);
            }
        }

        use tracing_subscriber::layer::SubscriberExt;
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::registry().with(Sink(lines.clone()));
        permit_every_callsite();
        tracing::subscriber::with_default(sub, || {
            emit_request_outcome(
                &RecordingSink::default(),
                &RequestOutcome {
                    status_code: 529,
                    request_id: "failed-turn".into(),
                    optimized_tokens: 41_000,
                    original_tokens: 97_000,
                    tokens_saved: 56_000,
                    ..Default::default()
                },
            );
        });

        let joined = lines.lock().unwrap().join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("request_failed_accounting"))
            .unwrap_or_else(|| panic!("no failure accounting event; captured:\n{joined}"));
        assert!(line.contains("request_id=failed-turn"), "{line}");
        assert!(line.contains("forwarded_tokens=41000"), "{line}");
        // The saving is reported as explicitly *not* booked, so nobody adds it
        // to a total by reading the field name alone.
        assert!(line.contains("tokens_saved_not_booked=56000"), "{line}");
    }

    #[test]
    fn generic_4xx_outcome_stays_on_success_funnel() {
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

    /// A request that leaves the proxy bigger than it arrived reports
    /// `tok_saved=0`, same as one that simply could not be compressed. The
    /// inflation amount distinguishes them.
    #[test]
    fn tokens_inflated_surfaces_what_the_clamp_swallows() {
        let grew = RequestOutcome {
            original_tokens: 1000,
            optimized_tokens: 1200,
            tokens_saved: 0,
            ..Default::default()
        };
        assert_eq!(grew.tokens_inflated(), 200);

        // Incompressible: same tok_saved=0, but nothing was added.
        let flat = RequestOutcome {
            original_tokens: 1000,
            optimized_tokens: 1000,
            tokens_saved: 0,
            ..Default::default()
        };
        assert_eq!(flat.tokens_inflated(), 0);

        // A genuinely compressed request never reports inflation.
        let shrank = RequestOutcome {
            original_tokens: 1000,
            optimized_tokens: 750,
            tokens_saved: 250,
            ..Default::default()
        };
        assert_eq!(shrank.tokens_inflated(), 0);
    }
}
