//! Turn hooks: provider mapping, response usage, and the request/response
//! hook runners with the model-call adapter they use.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

// ─── Turn hooks ───────────────────────────────────────────────────────────

/// Provider label for a compressible endpoint, as `turn_hooks::TurnContext`
/// expects it.
pub(super) fn turn_hook_provider(endpoint: compression::CompressibleEndpoint) -> &'static str {
    match endpoint {
        compression::CompressibleEndpoint::AnthropicMessages => "anthropic",
        compression::CompressibleEndpoint::OpenAiChatCompletions
        | compression::CompressibleEndpoint::OpenAiResponses => "openai",
    }
}

/// Read `(input, output, cache_read, cache_write)` out of one upstream
/// response's `usage` block.
///
/// `provider` carries the same labels as `OutcomeContext::provider`
/// (`"anthropic"` / `"openai_responses"` / anything else = OpenAI chat), and
/// the three arms below mirror the outcome block's parsing exactly. They have
/// to: the total this feeds is added to a number that block read, and settled
/// against one it will read, so a different reading here would stop cancelling.
pub(super) fn response_usage(response: &serde_json::Value, provider: &str) -> (i64, i64, i64, i64) {
    let usage = response.get("usage");
    let get = |key: &str| -> i64 {
        usage
            .and_then(|u| u.get(key))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    let cached = |details: &str| -> i64 {
        usage
            .and_then(|u| u.get(details))
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    match provider {
        "anthropic" => (
            get("input_tokens"),
            get("output_tokens"),
            get("cache_read_input_tokens"),
            get("cache_creation_input_tokens"),
        ),
        "openai_responses" => (
            get("input_tokens"),
            get("output_tokens"),
            cached("input_tokens_details"),
            0,
        ),
        _ => (
            get("prompt_tokens"),
            get("completion_tokens"),
            cached("prompt_tokens_details"),
            0,
        ),
    }
}

/// Upstream calls a turn hook made that nothing else accounts for.
///
/// A hook that re-drives the model through `call_model` makes real, billed
/// requests. The outcome block reads exactly one response — whichever the hook
/// handed back — so every other upstream call on that turn is spend no surface
/// records. A tool-search reload is a whole extra model call; count only the
/// last one and the feature hides its own overhead behind the saving it claims.
///
/// The Python original matched the response the usage block would read by
/// object identity and dropped that one entry. Here [`record`](Self::record)
/// takes the running total of every real upstream response and
/// [`settle`](Self::settle) subtracts whatever the outcome block is about to
/// read, so the two always sum back to what the upstream actually billed —
/// including when a hook returns a response it synthesised rather than one it
/// was given, which upstream over-counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TurnHookUsage {
    /// Upstream calls beyond the one the outcome block reads. Zero on the
    /// common path, and zero unless a hook re-drove the model.
    pub(super) calls: i64,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
}

impl TurnHookUsage {
    /// Note one real upstream response, the original included.
    pub(super) fn record(&mut self, response: &serde_json::Value, provider: &str) {
        let (input, output, cache_read, cache_write) = response_usage(response, provider);
        self.calls += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.cache_read_tokens += cache_read;
        self.cache_write_tokens += cache_write;
    }

    /// Drop the contribution of the response the outcome block will read. What
    /// is left is the delta that block has to add.
    ///
    /// A hook is free to return a response carrying larger figures than
    /// anything upstream sent, so each component floors at zero: over-counting
    /// a bill beats under-counting it.
    pub(super) fn settle(&mut self, read: &serde_json::Value, provider: &str) {
        let (input, output, cache_read, cache_write) = response_usage(read, provider);
        self.calls = (self.calls - 1).max(0);
        self.input_tokens = (self.input_tokens - input).max(0);
        self.output_tokens = (self.output_tokens - output).max(0);
        self.cache_read_tokens = (self.cache_read_tokens - cache_read).max(0);
        self.cache_write_tokens = (self.cache_write_tokens - cache_write).max(0);
    }

    /// True when there is nothing extra to account for.
    pub(super) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Pre-send `on_request` seam. Parses the outbound body, lets registered hooks
/// inspect/mutate `messages`/`tools`, and re-serializes only if a hook changed
/// them. Returns the body unchanged on any parse/serialize failure, plus the
/// tool-schema tokens hooks removed (measured on the final tools object, so
/// in-place shrinks count; growth clamps to zero). Callers MUST gate on a
/// non-empty registry so the empty-registry path is a byte-identical no-op
/// (this fn re-serializes and would perturb bytes).
///
/// `booked_deferred_tokens` is the `tool_search_deferred_tokens` credit
/// already booked for this request. When a hook un-defers some of those
/// tools, the third value is the lowered credit to book instead (upstream
/// f8212a39); `None` leaves the tag alone.
pub(crate) fn apply_request_hooks(
    body: bytes::Bytes,
    endpoint: compression::CompressibleEndpoint,
    request_id: &str,
    booked_deferred_tokens: Option<i64>,
) -> (bytes::Bytes, i64, Option<i64>) {
    let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (body, 0, None),
    };
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let messages = parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tools = parsed.get("tools").cloned();

    // A hook may shrink the tool array either by replacing it or in place,
    // so the baseline is counted BEFORE the hooks run and the saving off the
    // FINAL tools object. Deferral-shaped (removes schemas counting never
    // saw), hence a tag rather than a fold — mirrors upstream
    // `handlers/anthropic.py` + the OpenAI chat path.
    let tok = headroom_core::tokenizer::get_tokenizer(&model);
    let count_value = |v: &serde_json::Value| -> i64 {
        serde_json::to_string(v)
            .map(|s| tok.count_text(&s) as i64)
            .unwrap_or(0)
    };
    // Upstream f8212a39: deferral has its own credit
    // (`tool_search_deferred_tokens`), so the flag itself is not measured.
    // Otherwise un-deferring a tool shortens the JSON by the flag text and
    // books a saving although the bill goes up.
    let count_tools = |t: &Option<serde_json::Value>| -> i64 {
        t.as_ref()
            .map_or(0, |v| count_value(&without_deferral_flags(v)))
    };

    let mut ctx = crate::turn_hooks::TurnContext {
        provider: turn_hook_provider(endpoint).to_string(),
        model,
        messages,
        tools,
        config: None,
    };
    let tools_before = count_tools(&ctx.tools);
    let deferred_before = booked_deferred_tokens.map(|_| deferred_by_name(ctx.tools.as_ref()));
    crate::turn_hooks::run_request_hooks(&mut ctx);
    let tools_saved = tools_before.saturating_sub(count_tools(&ctx.tools));
    let deferred_tokens =
        booked_deferred_tokens
            .zip(deferred_before)
            .and_then(|(booked, before)| {
                reconcile_deferred_tokens(booked, &before, ctx.tools.as_ref(), &count_value)
            });

    // Write mutated messages/tools back onto the body.
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert(
            "messages".to_string(),
            serde_json::Value::Array(ctx.messages),
        );
        match ctx.tools {
            Some(t) => {
                obj.insert("tools".to_string(), t);
            }
            None => {
                obj.remove("tools");
            }
        }
    }
    match serde_json::to_vec(&parsed) {
        Ok(v) => (bytes::Bytes::from(v), tools_saved, deferred_tokens),
        Err(e) => {
            tracing::warn!(event = "turn_hooks_reserialize_failed", request_id = %request_id, error = %e, "turn hooks: re-serialize failed; forwarding original body");
            (body, 0, None)
        }
    }
}

/// The tool array with every `defer_loading` key dropped, as it is measured
/// for a hook's tool delta.
fn without_deferral_flags(tools: &serde_json::Value) -> serde_json::Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    serde_json::Value::Array(
        arr.iter()
            .map(|t| {
                let mut t = t.clone();
                if let Some(obj) = t.as_object_mut() {
                    obj.remove("defer_loading");
                }
                t
            })
            .collect(),
    )
}

/// Deferred tools (`defer_loading: true`) keyed by name.
fn deferred_by_name(
    tools: Option<&serde_json::Value>,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    tools
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter(|t| t.get("defer_loading").and_then(|d| d.as_bool()) == Some(true))
        .filter_map(|t| Some((t.get("name")?.as_str()?.to_string(), t.clone())))
        .collect()
}

/// Cap the deferral credit at the booked tools still deferred after the
/// hooks. A tool a hook un-deferred is billed, so its share goes. Each tool
/// still deferred counts at the smaller of its booked and final schema, so
/// growing one schema cannot keep another's credit alive, and a schema a
/// hook shortened (already booked as the hook's own saving) is not credited
/// twice. Only ever lowers: `None` when nothing is released.
fn reconcile_deferred_tokens(
    booked: i64,
    before: &std::collections::BTreeMap<String, serde_json::Value>,
    tools_after: Option<&serde_json::Value>,
    count: &dyn Fn(&serde_json::Value) -> i64,
) -> Option<i64> {
    let after = deferred_by_name(tools_after);
    let one = |t: &serde_json::Value| count(&serde_json::Value::Array(vec![t.clone()]));
    let kept: Vec<serde_json::Value> = before
        .iter()
        .filter_map(|(name, booked_schema)| {
            let now = after.get(name)?;
            Some(
                if one(now) < one(booked_schema) {
                    now
                } else {
                    booked_schema
                }
                .clone(),
            )
        })
        .collect();
    let measured = if kept.is_empty() {
        0
    } else {
        count(&serde_json::Value::Array(kept))
    };
    (measured < booked).then_some(measured)
}

/// `call_model` implementation for turn hooks: re-drives the upstream model via
/// the same buffered POST path the CCR continuation loop uses. Built from the
/// original request body (used as a template — its `messages` array is replaced
/// with whatever the hook passes) plus the live upstream url/client/headers.
pub(super) struct ProxyCallModel {
    pub(super) template: serde_json::Value,
    pub(super) upstream_url: url::Url,
    pub(super) client: reqwest::Client,
    pub(super) headers: http::HeaderMap,
    pub(super) request_id: String,
    /// Usage of every re-drive made through this handle. Shared with
    /// `apply_response_hooks`, and behind a lock because `CallModel::call`
    /// only has `&self`.
    pub(super) usage: Arc<std::sync::Mutex<TurnHookUsage>>,
    /// Provider label for reading those responses' `usage` blocks.
    pub(super) usage_provider: String,
}

impl ProxyCallModel {
    /// Note one re-drive's usage. Only the calls that came back with a body
    /// count: a request that never left, or died on the wire, was not billed.
    pub(super) fn record(&self, response: &serde_json::Value) {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(response, &self.usage_provider);
    }
}

#[async_trait::async_trait]
impl crate::turn_hooks::CallModel for ProxyCallModel {
    async fn call(&self, messages: Vec<serde_json::Value>) -> serde_json::Value {
        let mut body = self.template.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("messages".to_string(), serde_json::Value::Array(messages));
        }
        let body_bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(event = "turn_hooks_call_model_serialize_failed", request_id = %self.request_id, error = %e, "turn hooks call_model: serialize failed");
                return serde_json::Value::Null;
            }
        };
        let resp = self
            .client
            .post(self.upstream_url.clone())
            .headers(self.headers.clone())
            .body(body_bytes)
            .send()
            .await;
        match resp {
            Ok(r) => match r.bytes().await {
                Ok(bytes) => {
                    let parsed: serde_json::Value =
                        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                    self.record(&parsed);
                    parsed
                }
                Err(e) => {
                    tracing::warn!(event = "turn_hooks_call_model_read_failed", request_id = %self.request_id, error = %e, "turn hooks call_model: read body failed");
                    serde_json::Value::Null
                }
            },
            Err(e) => {
                tracing::warn!(event = "turn_hooks_call_model_upstream_failed", request_id = %self.request_id, error = %e, "turn hooks call_model: upstream request failed");
                serde_json::Value::Null
            }
        }
    }
}

/// Post-response `on_response` seam. Runs registered hooks over the buffered
/// upstream response, giving them a `call_model` that re-drives this same turn.
/// Returns the (possibly replaced) body bytes, unchanged on parse failure,
/// along with the usage of any upstream call the caller's outcome block will
/// not see. Callers MUST gate on a non-empty registry (byte-identical no-op).
///
/// `provider` is the hook-facing label (`"anthropic"` / `"openai"`);
/// `usage_provider` is the finer one the outcome block parses `usage` by.
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_response_hooks(
    body_bytes: bytes::Bytes,
    original_request: &bytes::Bytes,
    provider: &str,
    usage_provider: &str,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    headers: &http::HeaderMap,
    request_id: &str,
) -> (bytes::Bytes, TurnHookUsage) {
    let response: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => return (body_bytes, TurnHookUsage::default()),
    };
    let template: serde_json::Value = match serde_json::from_slice(original_request) {
        Ok(v) => v,
        Err(_) => return (body_bytes, TurnHookUsage::default()),
    };
    let model = template
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let messages = template
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tools = template.get("tools").cloned();

    let ctx = crate::turn_hooks::TurnContext {
        provider: provider.to_string(),
        model,
        messages,
        tools,
        config: None,
    };
    // The call we already made counts too: if a hook replaces the response,
    // this original is the one nobody else will read.
    let usage = Arc::new(std::sync::Mutex::new(TurnHookUsage::default()));
    usage
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(&response, usage_provider);

    let call_model = ProxyCallModel {
        template,
        upstream_url: upstream_url.clone(),
        client: client.clone(),
        headers: headers.clone(),
        request_id: request_id.to_string(),
        usage: Arc::clone(&usage),
        usage_provider: usage_provider.to_string(),
    };
    let out = crate::turn_hooks::run_response_hooks(&ctx, response, &call_model).await;
    let mut hook_usage = *usage.lock().unwrap_or_else(|e| e.into_inner());

    // Settle against the body the caller will actually go on to read, which on
    // a serialize failure is still the original response.
    let (final_bytes, final_response) = match serde_json::to_vec(&out) {
        Ok(v) => (bytes::Bytes::from(v), out),
        Err(_) => {
            let original = serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
            (body_bytes, original)
        }
    };
    hook_usage.settle(&final_response, usage_provider);
    (final_bytes, hook_usage)
}
