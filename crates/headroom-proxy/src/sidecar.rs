//! Claude Code's spinner-text sidecar: detect it, shrink it, forward it alone.
//!
//! Between real turns Claude Code asks for the line it prints next to the
//! spinner ("Reading runAgent.ts"). It asks by resending the *entire*
//! conversation — same model, same system, same tools, same `max_tokens` — with
//! one extra text block glued to the end of the last user message:
//!
//! ```text
//! Describe your most recent action in 3-5 words using present tense (-ing).
//! ```
//!
//! The block is never marked with `cache_control`, and on the next real turn it
//! is gone again. Two things followed from forwarding it untouched:
//!
//! 1. Opus and Sonnet billed a full prefix read (~80k tokens on a working
//!    session) plus a tail cache write, for four words of output.
//! 2. The proxy filed the sidecar's forwarded prefix in the replay store and
//!    the cache tracker. The next real turn no longer matched it, so it was
//!    logged as `cache_recache_observed` with
//!    `attribution_reason: unexplained_after_replay` — about 300 events and
//!    268k wasted tokens a day.
//!
//! So the sidecar is detected before any of that runs and answered on its own
//! terms: a handful of tail messages, no tools, a one-line system prompt, 64
//! output tokens, and a small model. Nothing about it touches per-conversation
//! state, which is what makes (2) go away; (1) goes away because there is no
//! longer a large prefix to read.
//!
//! Detection keys on the exact opening words of the block, at the start of the
//! last user message's text. A false positive would answer a real turn with 64
//! tokens from a small model, so the predicate matches the opening of a message
//! rather than the phrase anywhere in the body. Over a 2,648-request capture it
//! found all 68 sidecars and nothing else.
//!
//! Nothing here may leave the client worse off than not having it. If the shrunk
//! request fails for any reason — an unreachable model, a rejected body, an
//! exhausted retry budget — the caller is told the request was not handled and
//! sends the client's original bytes down the normal path. The cost of a failure
//! is one wasted upstream call; the outcome is what the proxy did before this
//! module existed.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};

/// The opening of the block Claude Code appends. Matched as a prefix because
/// the rest of the block (examples, "Do not use tools.") has changed between
/// client releases while this sentence has not.
pub const DESCRIBE_ACTION_PREFIX: &str = "Describe your most recent action in 3-5 words";

/// Label values for `proxy_sidecar_total`. A closed set, never request input.
const DESCRIBE_ACTION_KIND: &str = "describe_action";
/// Counted when the shrunk request failed and the original was sent instead.
const FALLBACK_KIND: &str = "fallback";

/// Model the shrunk sidecar is sent to when the operator sets none.
pub const DEFAULT_SIDECAR_MODEL: &str = "claude-haiku-4-5-20251001";

/// System prompt the original is replaced with. The tail messages carry all the
/// context the answer needs; the client's real system prompt is tens of
/// thousands of tokens of tool and project instructions that cannot change a
/// four-word summary.
const SIDECAR_SYSTEM: &str = "You summarise the assistant's most recent action.";

/// Output cap. The client asks for 3-5 words and discards anything longer.
const SIDECAR_MAX_TOKENS: u64 = 64;

/// How many trailing messages the summary is allowed to see.
const SIDECAR_TAIL_MESSAGES: usize = 4;

/// Stand-ins for blocks whose partner was left behind by the trim. Dropping the
/// block outright would tell the model less about the most recent action, which
/// is the one thing it is being asked about.
const ORPHAN_TOOL_RESULT: &str = "[earlier tool result omitted]";

/// Longest run of text any one block may carry into the sidecar request.
///
/// A single `tool_result` in the four-message window is routinely hundreds of
/// kilobytes — a file read, a `git log`, a directory listing. None of it changes
/// a four-word summary, and all of it is billed.
pub const SIDECAR_MAX_BLOCK_CHARS: usize = 2_000;

/// Marks text the cap cut short, so the model can tell a truncated read from a
/// short one.
const TRUNCATION_SUFFIX: &str = "[truncated]";

/// Keys the outbound sidecar request may carry.
///
/// An allowlist rather than a list of removals: real bodies also carry
/// `output_config`, `metadata`, `tool_choice` and whatever the next client
/// release adds, and a field the sidecar model does not accept — `output_config`
/// with an `effort` it has no tier for, say — is a 400 that shows up as a dead
/// spinner. `metadata` stays because its `user_id` is what the provider rate
/// limits on. `stream` stays because changing it would break the client parser.
const FORWARDED_KEYS: [&str; 6] = [
    "model",
    "messages",
    "system",
    "max_tokens",
    "stream",
    "metadata",
];

/// Cap `text` at [`SIDECAR_MAX_BLOCK_CHARS`], marking it when it bites.
///
/// Counts characters rather than bytes so the cut never lands inside a
/// multi-byte character.
fn truncate_text(text: &str) -> Option<String> {
    let mut chars = text.char_indices();
    let (cut, _) = chars.nth(SIDECAR_MAX_BLOCK_CHARS)?;
    Some(format!("{}{TRUNCATION_SUFFIX}", &text[..cut]))
}

/// Apply the cap in place to whichever field of `block` holds its text.
///
/// `tool_result.content` is either a string or its own array of blocks, so both
/// shapes are walked.
fn truncate_block(block: &mut Value) {
    for key in ["text", "thinking", "content"] {
        match block.get_mut(key) {
            Some(Value::String(s)) => {
                if let Some(cut) = truncate_text(s) {
                    *s = cut;
                }
            }
            Some(Value::Array(inner)) => {
                for nested in inner.iter_mut() {
                    truncate_block(nested);
                }
            }
            _ => {}
        }
    }
}

/// True when `body` is the describe-your-action sidecar.
///
/// The conversation's last user message must open — as a bare string, or in its
/// last `text` block — with [`DESCRIBE_ACTION_PREFIX`]. Nothing but `system`
/// messages may follow it.
///
/// Three shapes appear in a 2,648-request capture, and the narrow reading of
/// any one of them misses a third of the traffic:
///
/// - the block appended to the content array of the last user message;
/// - the block as its own user message with plain string content;
/// - either of those followed by a `system` message, which is a Claude Code
///   hook reminder landing after the block was assembled.
///
/// A trailing message of any other role means the client moved on and this is a
/// real turn.
pub fn is_describe_action_sidecar(body: &Value) -> bool {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let mut idx = messages.len();
    loop {
        if idx == 0 {
            return false;
        }
        idx -= 1;
        match messages[idx].get("role").and_then(|r| r.as_str()) {
            Some("user") => break,
            Some("system") => continue,
            _ => return false,
        }
    }
    opens_with_describe(messages[idx].get("content"))
}

fn opens_with_describe(content: Option<&Value>) -> bool {
    match content {
        Some(Value::String(text)) => text.starts_with(DESCRIBE_ACTION_PREFIX),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .rev()
            .find(|b| block_type(b) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.starts_with(DESCRIBE_ACTION_PREFIX)),
        _ => false,
    }
}

/// The trailing slice of `messages` the sidecar is allowed to see.
///
/// At most `max` messages, starting on a user message — the API refuses a
/// conversation that opens on any other role. When the `max`-message window
/// opens mid-turn the start walks forward to the next user message, so the
/// result is the largest valid tail no longer than `max`.
///
/// Trimming alone cannot always produce a legal request: the first kept user
/// message usually answers a `tool_use` that the trim left behind, and an
/// unpaired `tool_result` is a 400. Those blocks are rewritten to text rather
/// than dropped, so the model still sees that a tool ran. Assistant `thinking`
/// blocks go entirely — their signatures are bound to the model that produced
/// them and will not verify against the sidecar model.
///
/// Text is capped at [`SIDECAR_MAX_BLOCK_CHARS`] per block. One `tool_result` in
/// the window can run to hundreds of kilobytes, and four words of status need
/// none of it.
pub fn sidecar_tail(messages: &[Value], max: usize) -> Vec<Value> {
    // The message carrying the block. The walk stops here rather than running
    // off the end, so a run of trailing hook `system` messages longer than the
    // window cannot produce a tail with nothing in it.
    let anchor = messages
        .iter()
        .rposition(|m| role_of(m) == Some("user"))
        .unwrap_or(0);
    let mut start = messages.len().saturating_sub(max).min(anchor);
    while start < anchor && role_of(&messages[start]) != Some("user") {
        start += 1;
    }
    let mut tail: Vec<Value> = messages[start..].to_vec();
    repair_tool_pairs(&mut tail);
    tail
}

fn role_of(message: &Value) -> Option<&str> {
    message.get("role").and_then(|r| r.as_str())
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(|t| t.as_str())
}

/// Rewrite blocks whose partner is outside `tail`, and strip signed thinking.
///
/// A `tool_use` is kept only if some later message in `tail` carries a
/// `tool_result` for it, and a `tool_result` only if some earlier message
/// carries its `tool_use`. Anything else becomes a text block. `cache_control`
/// is stripped throughout: the sidecar is a one-shot request whose prefix is
/// never read again, so a cache write on it is pure cost.
fn repair_tool_pairs(tail: &mut [Value]) {
    let mut use_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for message in tail.iter() {
        let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            match block_type(block) {
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                        use_ids.push(id.to_string());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                        result_ids.push(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    let paired = |id: &str| use_ids.iter().any(|u| u == id) && result_ids.iter().any(|r| r == id);

    for message in tail.iter_mut() {
        let blocks = match message.get_mut("content") {
            Some(Value::Array(blocks)) => blocks,
            // The bare-string shape. Nothing to pair up, but the cap still
            // applies: a string message can be as long as any block.
            Some(text @ Value::String(_)) => {
                if let Some(cut) = text.as_str().and_then(truncate_text) {
                    *text = Value::String(cut);
                }
                continue;
            }
            _ => continue,
        };
        let mut rebuilt: Vec<Value> = Vec::with_capacity(blocks.len());
        for block in blocks.iter() {
            let mut block = block.clone();
            if let Some(obj) = block.as_object_mut() {
                obj.remove("cache_control");
            }
            truncate_block(&mut block);
            match block_type(&block) {
                Some("thinking") | Some("redacted_thinking") => continue,
                Some("tool_use") => {
                    let id = block.get("id").and_then(|i| i.as_str()).unwrap_or_default();
                    if paired(id) {
                        rebuilt.push(block);
                    } else {
                        let name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("a tool");
                        rebuilt.push(json!({"type": "text", "text": format!("[calling {name}]")}));
                    }
                }
                Some("tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default();
                    if paired(id) {
                        rebuilt.push(block);
                    } else {
                        rebuilt.push(json!({"type": "text", "text": ORPHAN_TOOL_RESULT}));
                    }
                }
                _ => rebuilt.push(block),
            }
        }
        // A message stripped down to nothing is a 400 of its own. Only an
        // assistant turn that was pure thinking can reach this.
        if rebuilt.is_empty() {
            rebuilt.push(json!({"type": "text", "text": "[thinking omitted]"}));
        }
        *blocks = rebuilt;
    }
}

/// Build the request that is actually sent for a detected sidecar.
///
/// The body is rebuilt from [`FORWARDED_KEYS`] rather than stripped of known
/// fields, so a key the client adds in its next release cannot ride along into a
/// model that has no tier for it. `system` collapses to one line, `max_tokens`
/// drops to [`SIDECAR_MAX_TOKENS`], and `stream` is copied exactly as the client
/// set it — Claude Code parses the reply with the same code either way, and
/// changing the shape would break it.
pub fn rewrite_sidecar(body: &Value, sidecar_model: &str) -> Value {
    let mut out = serde_json::Map::new();
    for key in FORWARDED_KEYS {
        if let Some(value) = body.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }

    let mut messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|m| sidecar_tail(m, SIDECAR_TAIL_MESSAGES))
        .unwrap_or_default();
    // The tail deliberately keeps Claude Code's trailing hook reminders, whose
    // role is `system`, so the walk can find the block behind them. The API
    // takes only `user` and `assistant` inside `messages` — a `system` entry
    // there is a hard 400, and the sidecar's own system prompt goes in the
    // top-level field below. Every sidecar whose tail caught a reminder failed
    // this way: 336 of 437 in one afternoon, every one a wasted round trip.
    messages.retain(|m| matches!(role_of(m), Some("user") | Some("assistant")));
    out.insert("messages".to_string(), Value::Array(messages));
    out.insert(
        "system".to_string(),
        Value::String(SIDECAR_SYSTEM.to_string()),
    );
    out.insert("max_tokens".to_string(), json!(SIDECAR_MAX_TOKENS));
    out.insert(
        "model".to_string(),
        Value::String(sidecar_model.to_string()),
    );
    Value::Object(out)
}

/// What the log event reports, gathered before and after the rewrite.
pub struct SidecarShape {
    pub original_messages: usize,
    pub forwarded_messages: usize,
    pub model_from: String,
    pub model_to: String,
}

/// Emit the one INFO line and the one counter increment a sidecar produces.
pub fn record_sidecar(request_id: &str, shape: &SidecarShape) {
    tracing::info!(
        event = "sidecar_detected",
        request_id = %request_id,
        kind = DESCRIBE_ACTION_KIND,
        original_messages = shape.original_messages,
        forwarded_messages = shape.forwarded_messages,
        model_from = %shape.model_from,
        model_to = %shape.model_to,
        "answering spinner-text sidecar on a shrunk request"
    );
    crate::observability::sidecar::observe_detected(DESCRIBE_ACTION_KIND);
}

/// Detect, rewrite, and forward in one step.
///
/// `None` means the caller should carry on with its normal pipeline, sending the
/// client's original body untouched. That covers both "this was not a sidecar"
/// and "it was, but the shrunk request failed" — see [`fall_back`]. The caller
/// never has to tell the two apart, because the response to both is the same.
///
/// The forward is a plain pass-through: the client's own credentials and beta
/// headers, the upstream's status and body handed straight back. No replay
/// capture, no tracker update, no offload, no compression — the request must
/// leave no trace in per-conversation state, because the turn that follows it
/// is the one whose prefix has to still match.
pub async fn try_handle(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    request_id: &str,
    client_headers: &HeaderMap,
    body: &Value,
    sidecar_model: &str,
    retry: SidecarRetry,
) -> Option<Response> {
    if !is_describe_action_sidecar(body) {
        return None;
    }
    let model_from = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let rewritten = rewrite_sidecar(body, sidecar_model);
    record_sidecar(
        request_id,
        &SidecarShape {
            original_messages: body
                .get("messages")
                .and_then(|m| m.as_array())
                .map_or(0, |m| m.len()),
            forwarded_messages: rewritten
                .get("messages")
                .and_then(|m| m.as_array())
                .map_or(0, |m| m.len()),
            model_from,
            model_to: sidecar_model.to_string(),
        },
    );
    forward(
        client,
        upstream_url,
        request_id,
        client_headers,
        &rewritten,
        retry,
    )
    .await
}

/// How hard the sidecar tries, and how long it waits between tries.
///
/// Taken from the same `retry_*` config the main path reads, so a sidecar and a
/// turn share one timeout and one backoff curve. The one knob deliberately not
/// shared is `retry_overload_max_attempts`: that budget runs tens of seconds and
/// exists so a real turn survives a provider outage. Spending it on a status
/// line would hold a connection open through exactly the window the turn beside
/// it needs.
#[derive(Clone, Copy)]
pub struct SidecarRetry {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl SidecarRetry {
    /// Read the shared retry knobs off the running config.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            max_attempts: if config.retry_enabled {
                config.retry_max_attempts.max(1)
            } else {
                1
            },
            base_delay_ms: config.retry_base_delay_ms,
            max_delay_ms: config.retry_max_delay_ms,
        }
    }
}

/// Send the shrunk request, retrying transport failures and the transient
/// statuses the main path retries: 429, 529, and 5xx.
async fn send_with_retry(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    request_id: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
    retry: SidecarRetry,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = client
            .post(upstream_url.clone())
            .headers(headers.clone())
            .body(payload.clone())
            .send()
            .await;
        let last = attempt >= retry.max_attempts;
        let retryable = match &result {
            Ok(r) => {
                let s = r.status().as_u16();
                s == 429 || s == 529 || (500..600).contains(&s)
            }
            Err(e) => crate::proxy::is_retryable_transport_error(e),
        };
        if last || !retryable {
            return result;
        }
        let backoff = headroom_core::retry::jitter_delay_ms(
            retry.base_delay_ms as i64,
            retry.max_delay_ms as i64,
            attempt - 1,
        ) as u64;
        tracing::warn!(
            event = "sidecar_upstream_retry",
            request_id = %request_id,
            attempt,
            max_attempts = retry.max_attempts,
            backoff_ms = backoff,
            "retrying the spinner-text sidecar"
        );
        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
    }
}

/// Send the shrunk request; `None` hands the turn back to the normal pipeline.
///
/// Every failure falls back rather than surfacing. A transport error or any
/// non-2xx means the shrunk request did not work, and the one thing the sidecar
/// must never do is make the client worse off than not having it: a 502 here
/// would be a dead spinner that the untouched proxy would have filled. Falling
/// back costs one wasted upstream call and lands on exactly today's behaviour.
async fn forward(
    client: &reqwest::Client,
    upstream_url: &url::Url,
    request_id: &str,
    client_headers: &HeaderMap,
    body: &Value,
    retry: SidecarRetry,
) -> Option<Response> {
    let mut headers = HeaderMap::new();
    for (name, value) in client_headers.iter() {
        if crate::headers::is_request_drop(name) || crate::headers::is_internal_header(name) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    // Same override the main path makes for intercepted requests: without it
    // the client's `accept-encoding` rides along, upstream compresses the body,
    // and reading it as text logs the compressed bytes as mojibake.
    headers.insert(
        axum::http::header::ACCEPT_ENCODING,
        axum::http::HeaderValue::from_static("identity"),
    );

    let payload = match serde_json::to_vec(body) {
        Ok(p) => p,
        Err(e) => return fall_back(request_id, None, &format!("serialising sidecar body: {e}")),
    };

    let upstream_resp =
        match send_with_retry(client, upstream_url, request_id, headers, payload, retry).await {
            Ok(r) => r,
            Err(e) => return fall_back(request_id, None, &e.to_string()),
        };

    // Anything but a 2xx means the shrunk request did not work — a model id the
    // deployment cannot reach, a field this model rejects, an exhausted retry
    // budget. Read the body here, and only here: it is small, it is the whole
    // explanation, and the success path must keep streaming.
    if !upstream_resp.status().is_success() {
        let status = upstream_resp.status().as_u16();
        let detail = upstream_resp.text().await.unwrap_or_default();
        return fall_back(request_id, Some(status), &detail);
    }

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if let Some(out) = builder.headers_mut() {
        for (name, value) in upstream_resp.headers().iter() {
            if crate::headers::is_response_drop(name) {
                continue;
            }
            out.append(name.clone(), value.clone());
        }
        // The body is re-framed as a stream, so any length the upstream
        // declared no longer describes what the client will read.
        out.remove(axum::http::header::CONTENT_LENGTH);
    }
    match builder.body(Body::from_stream(upstream_resp.bytes_stream())) {
        Ok(response) => Some(response),
        Err(e) => fall_back(request_id, None, &format!("building sidecar response: {e}")),
    }
}

/// Log the failure, count it, and hand the turn back to the normal pipeline.
///
/// Always returns `None`: the caller reads that as "not handled" and forwards
/// the client's original body untouched.
fn fall_back(request_id: &str, status: Option<u16>, detail: &str) -> Option<Response> {
    tracing::warn!(
        event = "sidecar_fallback",
        request_id = %request_id,
        status = status.unwrap_or_default(),
        error = %detail,
        "spinner-text sidecar failed; forwarding the original request instead"
    );
    crate::observability::sidecar::observe_detected(FALLBACK_KIND);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe_block() -> Value {
        json!({
            "type": "text",
            "text": "Describe your most recent action in 3-5 words using present \
                     tense (-ing). Name the file or function, not the branch. \
                     Do not use tools."
        })
    }

    fn tool_use(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "Read", "input": {}})
    }

    fn tool_result(id: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": "ok"})
    }

    // ---- predicate ----

    #[test]
    fn detects_the_block_on_the_last_user_message() {
        let body = json!({"messages": [
            {"role": "user", "content": [tool_result("a"), describe_block()]}
        ]});
        assert!(is_describe_action_sidecar(&body));
    }

    #[test]
    fn detects_it_when_the_block_stands_alone() {
        let body = json!({"messages": [{"role": "user", "content": [describe_block()]}]});
        assert!(is_describe_action_sidecar(&body));
    }

    #[test]
    fn ignores_a_trailing_assistant_message() {
        let body = json!({"messages": [
            {"role": "user", "content": [describe_block()]},
            {"role": "assistant", "content": [{"type": "text", "text": "Reading lib.rs"}]}
        ]});
        assert!(!is_describe_action_sidecar(&body));
    }

    #[test]
    fn detects_the_block_as_bare_string_content() {
        let body = json!({"messages": [
            {"role": "user", "content": "Describe your most recent action in 3-5 words \
                                         using present tense (-ing)."}
        ]});
        assert!(is_describe_action_sidecar(&body));
    }

    /// A `PreToolUse` hook reminder lands after the block was assembled. The
    /// client still wants a spinner line, so this is still a sidecar.
    #[test]
    fn detects_it_behind_a_trailing_hook_reminder() {
        let body = json!({"messages": [
            {"role": "user", "content": [describe_block()]},
            {"role": "system", "content": "<system-reminder>hook context</system-reminder>"}
        ]});
        assert!(is_describe_action_sidecar(&body));
    }

    #[test]
    fn ignores_content_that_is_neither_string_nor_array() {
        let body = json!({"messages": [{"role": "user", "content": 7}]});
        assert!(!is_describe_action_sidecar(&body));
    }

    /// Trailing `system` messages are skipped when locating the user turn, so
    /// a window of four could otherwise land entirely on them.
    #[test]
    fn a_run_of_trailing_reminders_still_leaves_the_block_in_the_tail() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "0"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "1"}]}),
            json!({"role": "user", "content": [describe_block()]}),
            json!({"role": "system", "content": "r1"}),
            json!({"role": "system", "content": "r2"}),
            json!({"role": "system", "content": "r3"}),
            json!({"role": "system", "content": "r4"}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert_eq!(role_of(&tail[0]), Some("user"));
        assert_eq!(tail.len(), 5, "the block plus its four trailing reminders");
        assert!(is_describe_action_sidecar(&json!({"messages": tail})));
    }

    #[test]
    fn ignores_the_phrase_anywhere_but_the_last_text_block() {
        let body = json!({"messages": [{"role": "user", "content": [
            describe_block(),
            {"type": "text", "text": "and now do the real work"}
        ]}]});
        assert!(!is_describe_action_sidecar(&body));
    }

    #[test]
    fn ignores_an_empty_conversation() {
        assert!(!is_describe_action_sidecar(&json!({"messages": []})));
        assert!(!is_describe_action_sidecar(&json!({})));
    }

    // ---- trimming ----

    #[test]
    fn keeps_at_most_four_messages_and_starts_on_a_user_turn() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "0"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "1"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "2"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "3"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "4"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "5"}]}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        // The four-message window opens on an assistant turn, so the start
        // walks forward one and three messages come through.
        assert_eq!(tail.len(), 3);
        assert_eq!(role_of(&tail[0]), Some("user"));
    }

    #[test]
    fn a_shorter_conversation_survives_whole() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "0"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "1"}]}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        assert_eq!(sidecar_tail(&messages, 4).len(), 3);
    }

    #[test]
    fn an_orphaned_tool_result_becomes_text() {
        let messages = vec![
            json!({"role": "assistant", "content": [tool_use("old")]}),
            json!({"role": "user", "content": [tool_result("old")]}),
            json!({"role": "assistant", "content": [tool_use("new")]}),
            json!({"role": "user", "content": [tool_result("new"), describe_block()]}),
        ];
        // Window of 3 opens on the user message answering `old`, whose
        // `tool_use` is left behind.
        let tail = sidecar_tail(&messages, 3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0]["content"][0]["type"], "text");
        assert_eq!(tail[0]["content"][0]["text"], ORPHAN_TOOL_RESULT);
        // The pair that survives intact is left alone.
        assert_eq!(tail[1]["content"][0]["type"], "tool_use");
        assert_eq!(tail[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn an_unanswered_tool_use_becomes_text_naming_the_tool() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [tool_use("pending")]}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert_eq!(tail[1]["content"][0]["type"], "text");
        assert_eq!(tail[1]["content"][0]["text"], "[calling Read]");
    }

    #[test]
    fn thinking_blocks_and_cache_control_are_stripped() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "...", "signature": "sig"},
                {"type": "text", "text": "done", "cache_control": {"type": "ephemeral"}}
            ]}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert_eq!(tail[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(tail[1]["content"][0]["text"], "done");
        assert!(tail[1]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn a_message_that_was_only_thinking_keeps_a_placeholder() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "...", "signature": "sig"}
            ]}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert_eq!(tail[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(tail[1]["content"][0]["text"], "[thinking omitted]");
    }

    // ---- rewrite ----

    #[test]
    fn rewrite_strips_everything_the_summary_cannot_use() {
        let body = json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "stream": true,
            "thinking": {"type": "adaptive"},
            "context_management": {"edits": []},
            "tools": [{"name": "Read"}],
            "tool_choice": {"type": "auto"},
            "system": [{"type": "text", "text": "a very long preamble"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "0"}]},
                {"role": "assistant", "content": [tool_use("a")]},
                {"role": "user", "content": [tool_result("a"), describe_block()]}
            ]
        });
        let out = rewrite_sidecar(&body, DEFAULT_SIDECAR_MODEL);
        assert_eq!(out["model"], DEFAULT_SIDECAR_MODEL);
        assert_eq!(out["max_tokens"], 64);
        assert_eq!(out["system"], SIDECAR_SYSTEM);
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("thinking").is_none());
        assert!(out.get("context_management").is_none());
        // Streaming is the client's call and is passed through untouched.
        assert_eq!(out["stream"], true);
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn rewrite_forwards_only_the_allowlisted_keys() {
        let body = json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "stream": true,
            "metadata": {"user_id": "device-abc"},
            // Real bodies carry these; Haiku has no tier for `effort`.
            "output_config": {"effort": "medium"},
            "thinking": {"type": "adaptive"},
            "context_management": {"edits": []},
            "tools": [{"name": "Read"}],
            "tool_choice": {"type": "auto"},
            "top_p": 0.9,
            "a_field_a_future_client_adds": true,
            "system": [{"type": "text", "text": "a very long preamble"}],
            "messages": [{"role": "user", "content": [describe_block()]}]
        });
        let out = rewrite_sidecar(&body, DEFAULT_SIDECAR_MODEL);
        let keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        for key in &keys {
            assert!(
                FORWARDED_KEYS.contains(key),
                "{key} is not on the allowlist but was forwarded"
            );
        }
        // The ones that carry the request are all there.
        assert_eq!(out["model"], DEFAULT_SIDECAR_MODEL);
        assert_eq!(out["max_tokens"], 64);
        assert_eq!(out["system"], SIDECAR_SYSTEM);
        assert_eq!(out["stream"], true);
        // `metadata.user_id` is what the provider rate limits on, so it rides along.
        assert_eq!(out["metadata"]["user_id"], "device-abc");
        assert!(out.get("output_config").is_none());
    }

    #[test]
    fn rewrite_drops_a_body_with_no_metadata_without_inventing_one() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": [describe_block()]}]
        });
        let out = rewrite_sidecar(&body, DEFAULT_SIDECAR_MODEL);
        assert!(out.get("metadata").is_none());
        assert!(out.get("stream").is_none(), "absent stays absent");
    }

    // ---- truncation ----

    #[test]
    fn a_long_tool_result_is_capped() {
        let huge = "x".repeat(300_000);
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [tool_use("a")]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": huge},
                describe_block()
            ]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert_eq!(tail.len(), 3);
        let content = tail[2]["content"][0]["content"].as_str().unwrap();
        assert_eq!(
            content.chars().count(),
            SIDECAR_MAX_BLOCK_CHARS + TRUNCATION_SUFFIX.chars().count()
        );
        assert!(content.ends_with(TRUNCATION_SUFFIX));
    }

    #[test]
    fn a_long_text_block_is_capped() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "y".repeat(9_000)}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "short"}]}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert!(tail[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with(TRUNCATION_SUFFIX));
        // Anything already under the cap is left exactly as it was.
        assert_eq!(tail[1]["content"][0]["text"], "short");
    }

    /// `tool_result.content` is sometimes an array of blocks rather than a
    /// string, and the cap has to reach into it.
    #[test]
    fn a_tool_result_holding_blocks_is_capped_block_by_block() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [tool_use("a")]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": [
                    {"type": "text", "text": "z".repeat(50_000)}
                ]},
                describe_block()
            ]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        let inner = tail[2]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(inner.ends_with(TRUNCATION_SUFFIX));
        assert_eq!(
            inner.chars().count(),
            SIDECAR_MAX_BLOCK_CHARS + TRUNCATION_SUFFIX.chars().count()
        );
    }

    #[test]
    fn a_long_bare_string_message_is_capped() {
        let messages = vec![
            json!({"role": "user", "content": "w".repeat(20_000)}),
            json!({"role": "user", "content": [describe_block()]}),
        ];
        let tail = sidecar_tail(&messages, 4);
        assert!(tail[0]["content"]
            .as_str()
            .unwrap()
            .ends_with(TRUNCATION_SUFFIX));
    }

    /// The cut counts characters, so it can never land inside one.
    #[test]
    fn the_cap_never_splits_a_multibyte_character() {
        let messages = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "\u{1f600}".repeat(5_000)},
            describe_block()
        ]})];
        let tail = sidecar_tail(&messages, 4);
        let text = tail[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.ends_with(TRUNCATION_SUFFIX));
        assert_eq!(
            text.chars().count(),
            SIDECAR_MAX_BLOCK_CHARS + TRUNCATION_SUFFIX.chars().count()
        );
    }

    #[test]
    fn rewrite_keeps_a_non_streaming_request_non_streaming() {
        let body = json!({
            "model": "claude-opus-5",
            "stream": false,
            "messages": [{"role": "user", "content": [describe_block()]}]
        });
        let out = rewrite_sidecar(&body, DEFAULT_SIDECAR_MODEL);
        assert_eq!(out["stream"], false);
    }
}
