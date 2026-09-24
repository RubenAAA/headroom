//! CCR continuation rounds: transport limits, round usage, continuation
//! body reads and truncation checks, and `handle_ccr_response`.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Extra attempts for a CCR continuation before giving up on the retrieval.
/// The content is already fetched by this point, so the only thing a failure
/// costs is the model's answer; three attempts covers the overload bursts that
/// produced every observed continuation failure.
pub(super) const CCR_CONTINUATION_RETRIES: u32 = 2;

/// Ceiling on waiting for a continuation round's response headers, per
/// attempt. The shared client timeout (600s, sized for streams) cannot see a
/// stalled headers wait: measured 2026-09-09, one round hung 43s/31s/26s
/// across its three attempts on flaky egress while the 600s bound sat
/// untouched, holding the client's turn 107s for a retrieval that died.
/// `.send()` resolves at response headers, so on a streamed continuation this
/// cannot cut a slow model short — the body arrives after, bounded by
/// [`CCR_CONTINUATION_IDLE_TIMEOUT`]. A headers wait past this is a stall, not
/// thinking; fail it fast so the retry can actually help instead of re-waiting
/// the same stall.
///
/// On a *buffered* continuation the two are the same wait, and this is a bound
/// on generation: a routed chat-completions backend can still lose a slow round
/// here. Anthropic continuations stream for exactly that reason (see
/// `streamed_continuation_request`); the routed shapes need their own SSE fold
/// before they can follow.
pub(super) const CCR_CONTINUATION_SEND_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Ceiling on the silence between two chunks of a streamed continuation's
/// response body. The generation lives in that body, so the only bound that
/// does not cut a slow model short is one on silence: Anthropic pings while it
/// thinks, so a gap this long is a dead connection rather than a long one. A
/// buffered continuation arrives in one chunk and never waits on this.
pub(super) const CCR_CONTINUATION_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Short classification of a reqwest transport failure for log lines.
/// reqwest's Display names the URL but not the phase; without this every
/// continuation stall reads identically and the next one is undebuggable
/// the same way.
pub(super) fn ccr_transport_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_builder() {
        "builder"
    } else if e.is_body() {
        "body"
    } else if e.is_request() {
        "request"
    } else if e.is_decode() {
        "decode"
    } else {
        "unknown"
    }
}

/// The source chain behind a reqwest error, outermost first, length-capped.
/// This is where the actual cause lives (hyper: connection closed early,
/// TLS alert, DNS) — Display alone never shows it.
pub(super) fn ccr_error_chain(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        parts.push(s.to_string());
        if parts.len() >= 4 {
            break;
        }
        source = s.source();
    }
    let joined = parts.join(" <- ");
    joined.chars().take(500).collect()
}

/// How many times retrieval and memory may hand work back to each other.
///
/// Each resolver only runs the calls standing when it starts, and either one's
/// continuation can come back asking for the other: a `memory_search` answered
/// server-side can leave the model asking for a `headroom_retrieve`, and that
/// call arrives after retrieval has already had its turn. Running the pair once
/// in a fixed order left such a call with nobody to run it, so the splice
/// dropped it and downgraded the turn.
///
/// This bounds the alternation between the two, which is not the same quantity
/// as `--ccr-max-retrieval-rounds` — that one bounds a chain of calls of the
/// *same* kind, and each resolver still applies it internally. Handoffs are
/// rare, so a small fixed number covers them; a pass with nothing to do costs
/// no upstream call, only a parse.
pub(crate) const MAX_RESOLVER_ALTERNATIONS: usize = 4;

/// Read one upstream `usage` block in any wire shape booking accepts.
/// Responses reports `input_tokens`/`output_tokens`, Chat Completions
/// reports `prompt_tokens`/`completion_tokens`, Anthropic carries
/// `cache_read_input_tokens` directly. Max-convention throughout: a body
/// carrying both takes the larger, never the sum — matching the outcome
/// funnel. Shared by round folding and passthrough booking so the two
/// cannot drift into reading different numbers off the same block.
pub(crate) fn usage_counts(usage: &serde_json::Value) -> (i64, i64, i64) {
    let get = |key: &str| usage.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    let input = get("input_tokens").max(get("prompt_tokens"));
    let output = get("output_tokens").max(get("completion_tokens"));
    let cached = get("cache_read_input_tokens").max(
        usage
            .get("input_tokens_details")
            .or_else(|| usage.get("prompt_tokens_details"))
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
    );
    (input, output, cached)
}

/// Upstream usage from CCR continuation rounds the client never sees.
///
/// `handle_ccr_response` resolves a `headroom_retrieve` call server-side by
/// re-POSTing to the real upstream, up to `--ccr-max-retrieval-rounds` times,
/// and returns only the last response. Every earlier round is a real billed
/// call whose `usage` block would otherwise be dropped on the floor — the
/// client sees one turn, the bill has several.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CcrRoundUsage {
    /// Continuation rounds whose usage this carries. Zero on the common path.
    pub rounds: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Usage of the first upstream response that the proxy replaced with an
    /// internal continuation. This is the cache footprint of the client's
    /// original request; the next client turn does not contain proxy-private
    /// retrieval/tool-result messages and must be compared with this baseline.
    pub client_input_tokens: u64,
    pub client_cache_read_tokens: u64,
    pub client_cache_write_tokens: u64,
}

impl CcrRoundUsage {
    /// Fold in one response's `usage` block.
    pub(crate) fn add_response(&mut self, response: &serde_json::Value) {
        let Some(usage) = response.get("usage") else {
            return;
        };
        let get = |key: &str| {
            usage
                .get(key)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        };
        // Both wire shapes (see `usage_counts`): Responses reports
        // `input_tokens`/`output_tokens`, Chat Completions reports
        // `prompt_tokens`/`completion_tokens`. Same max-convention as the
        // outcome funnel (`book_routed_outcome_with_ccr`); a body carrying
        // both takes the larger, never the sum. Cache reads keep the direct
        // Anthropic key as a fallback: older usage blocks carry no details
        // section.
        let (input, output, cached) = usage_counts(usage);
        if self.rounds == 0 {
            self.client_input_tokens = input.max(0) as u64;
            self.client_cache_read_tokens = cached.max(0) as u64;
            self.client_cache_write_tokens = get("cache_creation_input_tokens").max(0) as u64;
        }
        self.rounds += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.cache_read_tokens += cached;
        self.cache_write_tokens += get("cache_creation_input_tokens");
    }

    /// Fold another set of rounds in. A turn can spend rounds on more than one
    /// proxy-owned tool family, and both were billed.
    pub fn absorb(&mut self, other: CcrRoundUsage) {
        if self.rounds == 0 && other.rounds > 0 {
            self.client_input_tokens = other.client_input_tokens;
            self.client_cache_read_tokens = other.client_cache_read_tokens;
            self.client_cache_write_tokens = other.client_cache_write_tokens;
        }
        self.rounds += other.rounds;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// True when there is nothing extra to account for.
    pub fn is_empty(&self) -> bool {
        self.rounds == 0
    }

    /// Cache counters that describe the request the client actually made.
    /// Hidden continuation rounds still remain in the billing totals above.
    pub(super) fn client_cache_baseline(
        &self,
        final_input: u64,
        final_cache_read: u64,
        final_cache_write: u64,
    ) -> (u64, u64, u64) {
        if self.rounds > 0 {
            (
                self.client_input_tokens,
                self.client_cache_read_tokens,
                self.client_cache_write_tokens,
            )
        } else {
            (final_input, final_cache_read, final_cache_write)
        }
    }
}

/// How long to wait before retry number `attempt` (0-based), in milliseconds.
///
/// Every retry site in the proxy used to inline this formula, and the copies
/// had drifted: two jittered, the in-band-SSE branch did not, and the local
/// model's transport branch ignored the configured base entirely and applied no
/// ceiling. `headroom_core::retry::jitter_delay_ms` was written to be the one
/// copy and had no caller at all. Jitter is not decoration here — an overloaded
/// upstream returns 529 to every in-flight request at once, and unjittered
/// backoff sends them all back in the same millisecond.
pub(crate) fn backoff_ms(state: &AppState, attempt: u32) -> u64 {
    headroom_core::retry::jitter_delay_ms(
        state.config.retry_base_delay_ms as i64,
        state.config.retry_max_delay_ms as i64,
        attempt,
    ) as u64
}

// ─── CCR Response Handling ────────────────────────────────────────────────

/// Detect CCR tool calls in a buffered upstream response, fetch original
/// content from the CCR store, and continue the conversation until the
/// LLM produces a response without CCR calls (or max rounds is hit).
///
/// Returns the final response body bytes. Only operates on non-streaming,
/// Anthropic-shaped responses for now (the primary CCR path).
///
/// Move the message breakpoint onto the tail of a continuation body.
///
/// The first round's marker sits on what was then the last block. Each round
/// appends an assistant turn and its tool results behind it, so without this
/// the marker drifts backwards through the request and everything after it is
/// written fresh on the round that follows.
///
/// `push_newest_marker_to_tail` relocates the existing marker object rather
/// than adding one, so calling it every round cannot breach Anthropic's cap of
/// four and carries whatever TTL the pin gave it. With two tail slots only the
/// newer marker moves; the older one stays on the prefix the provider already
/// holds. It returns false when the marker is already at the tail.
///
/// Anthropic only: no other shape here has `cache_control`.
pub(super) fn retail_continuation_breakpoint(
    request: &mut serde_json::Value,
    provider: &str,
    config: &Config,
    request_id: &str,
    round: usize,
) {
    if provider != "anthropic" || !config.cache_tail_breakpoint {
        return;
    }
    if cache_stabilization::message_breakpoints::push_newest_marker_to_tail(request) {
        tracing::debug!(
            request_id = %request_id,
            event = "continuation_tail_breakpoint",
            round = round,
            "moved the message breakpoint onto the continuation tail"
        );
    }
}

/// Map a continuation handler's provider label onto the drift detector's
/// shape enum. `None` for a label neither knows, which skips the check
/// rather than hashing a body under the wrong shape rules.
pub(super) fn continuation_api_kind(provider: &str) -> Option<ApiKind> {
    match provider {
        "anthropic" => Some(ApiKind::Anthropic),
        "openai" => Some(ApiKind::OpenAiChat),
        "openai_responses" => Some(ApiKind::OpenAiResponses),
        _ => None,
    }
}

/// Append a CCR continuation entry to the running item array.
///
/// Most provider shapes return a single message dict, which is pushed as-is.
/// Some providers (OpenAI chat-completions tool results, OpenAI Responses
/// turns/tool results) return a sentinel-keyed wrapper `{ "_sentinel": [..] }`
/// whose list must be spliced into the array. If `entry` is such a wrapper for
/// any of `sentinel_keys`, its list is extended in; otherwise `entry` is pushed.
pub(super) fn extend_or_push(
    items: &mut Vec<serde_json::Value>,
    entry: serde_json::Value,
    sentinel_keys: &[&str],
) {
    if let Some(obj) = entry.as_object() {
        for key in sentinel_keys {
            if let Some(list) = obj.get(*key).and_then(|v| v.as_array()) {
                items.extend(list.iter().cloned());
                return;
            }
        }
    }
    items.push(entry);
}

/// Read a continuation response into the turn JSON the CCR machinery speaks.
///
/// A routed continuation comes back as JSON, which parses directly. Anthropic
/// continuations stream, and so do those on a Responses backend that mandates
/// it (the chatgpt codex gateway answers `stream: false` with `400 Stream must
/// be set to true`); both answer SSE, which `serde_json` cannot read — fold it
/// back into a turn first, in the shape the provider speaks. JSON-first, so
/// a JSON body never changes shape no matter its content type; the fold only
/// runs when plain parsing already failed. Returns `None` when the body is
/// neither, and the caller ends the round as it always has.
/// True when a body opens like an SSE stream (`event:` or `data:` field,
/// after any leading blank lines), for responses that carry no Content-Type.
pub(super) fn looks_like_sse(body: &[u8]) -> bool {
    let head = &body[..body.len().min(64)];
    let head = std::string::String::from_utf8_lossy(head);
    let first = head.trim_start_matches(['\r', '\n']);
    first.starts_with("event:") || first.starts_with("data:")
}

/// Whether a continuation body arrived as SSE (vs buffered JSON): the
/// Content-Type header wins when present, otherwise sniff the body. Shared
/// by the fold and the cut-stream check so they agree on what "should have
/// folded" for a given body.
pub(super) fn continuation_body_is_sse(body: &[u8], content_type: Option<&str>) -> bool {
    match content_type.map(str::trim).filter(|ct| !ct.is_empty()) {
        Some(ct) => ct.contains("text/event-stream"),
        None => looks_like_sse(body),
    }
}

/// Truncation signature for buffered-JSON continuations (the `openai` chat
/// shape folds JSON only): a body that is not valid JSON and does not end
/// like one was cut mid-write. A complete-but-unparseable body (ends with
/// `}` or `]`) is deterministic garbage — resending it would fail the same
/// way. An empty body is never a valid turn.
pub(super) fn continuation_json_looks_truncated(body: &[u8]) -> bool {
    match body.iter().rposition(|b| !b.is_ascii_whitespace()) {
        None => true,
        Some(i) => !matches!(body[i], b'}' | b']'),
    }
}

/// Whether a continuation body that failed to fold is worth resending
/// identical, same round: terminal-less SSE streams and truncated JSON are
/// transport cuts in substance. Explicit verdicts, deterministic garbage,
/// and unknown provider shapes are not. Budget is enforced by the caller —
/// CCR and memory continuations keep separate per-turn counters.
pub(super) fn continuation_cut_retryable(body: &[u8], content_type: &str, provider: &str) -> bool {
    match provider {
        "openai_responses" | "anthropic" => {
            continuation_stream_terminal(body, provider).is_none()
                && continuation_body_is_sse(body, Some(content_type))
        }
        // Chat completions fold JSON only: truncated JSON (not ending like
        // a complete value) reads as cut mid-write.
        "openai" => continuation_json_looks_truncated(body),
        _ => false,
    }
}

/// Terminal marker of a streamed continuation body, if any. A 200 whose SSE
/// body carries no terminal event ended mid-generation (cut stream): on
/// 2026-09-17 a gpt-5.6-luna reasoning continuation landed as ~200 KB of
/// reasoning deltas followed by EOF, folded to zero blocks, and the turn
/// went quiet on a fallback splice. That shape is transport failure in
/// substance and worth resending. An explicit failed/incomplete verdict is
/// deterministic — the identical re-send would fail the same way — so it
/// must NOT retry. Returns None for non-SSE-fold providers and for bodies
/// with no terminal marker.
pub(crate) fn continuation_stream_terminal(body: &[u8], provider: &str) -> Option<&'static str> {
    if !matches!(provider, "openai_responses" | "anthropic") {
        return None;
    }
    // N.B. the caller already established SSE; this only classifies it.
    // `event:` names and `"type":` values share these strings, so one
    // substring scan covers both framings (including the bare/Codex forms).
    let text = std::string::String::from_utf8_lossy(body);
    if provider == "openai_responses" {
        for marker in [
            "response.failed",
            "response.incomplete",
            "response.completed",
        ] {
            if text.contains(marker) {
                return Some(match marker {
                    "response.failed" => "failed",
                    "response.incomplete" => "incomplete",
                    _ => "completed",
                });
            }
        }
        return None;
    }
    // Anthropic turns end on message_delta; without it the fold has no
    // stop_reason and yields nothing usable.
    if text.contains("message_delta") {
        Some("message_delta")
    } else {
        None
    }
}

pub(super) fn continuation_turn_from_body(
    body: &bytes::Bytes,
    content_type: Option<&str>,
    provider: &str,
) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        return Some(v);
    }
    if !matches!(provider, "openai_responses" | "anthropic") {
        return None;
    }
    // A missing Content-Type is not "not SSE": on 2026-09-14 eleven
    // continuations on the gpt-5.6 Responses route came back with no
    // Content-Type at all and a 150-180 KB non-JSON body, and refusing the
    // fold here replaced the whole turn with the retrieval-failure note.
    // Sniff the body when the header is absent; a wrong header still wins.
    if !continuation_body_is_sse(body, content_type) {
        return None;
    }
    if provider == "anthropic" {
        return crate::sse::ccr_stream::anthropic_stream_to_turn(body);
    }
    let text = std::string::String::from_utf8_lossy(body);
    let (turn, _) = crate::openai::response::responses_stream_to_turn(&text);
    let has_blocks = turn
        .get("output")
        .and_then(|o| o.as_array())
        .is_some_and(|o| !o.is_empty());
    if has_blocks { Some(turn) } else { None }
}

/// Read a continuation's response body, failing on silence rather than on
/// elapsed time. `reqwest::Response::bytes` has neither bound, so a streamed
/// continuation that dies mid-body would otherwise sit until the 600s client
/// timeout. The error string is for the log line at the call site.
pub(super) async fn read_continuation_body(
    mut resp: reqwest::Response,
) -> Result<bytes::Bytes, String> {
    let mut buf = bytes::BytesMut::new();
    loop {
        match tokio::time::timeout(CCR_CONTINUATION_IDLE_TIMEOUT, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => buf.extend_from_slice(&chunk),
            Ok(Ok(None)) => return Ok(buf.freeze()),
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => {
                return Err(format!(
                    "no body chunk within {}s",
                    CCR_CONTINUATION_IDLE_TIMEOUT.as_secs()
                ));
            }
        }
    }
}

/// The compression pipeline is not re-applied to continuation requests, and
/// must not be: `forwarded_request` is the body that already went upstream,
/// transforms and all, so each round only appends the assistant turn and its
/// tool results to a prefix the provider has already cached.
///
/// It has to be the forwarded body for that to hold. Passing the client's raw
/// request instead — which this did until 2026-08-22 — drops every injected
/// tool, every offloaded block and any routed model, so the continuation
/// presents a prefix the provider never saw and every round after a
/// transformed turn misses cache.
/// Prefix for content rebuilt from the FTS index after the CCR store expired
/// it. Indexing splits a block into chunks and keeps no separator, so a source
/// that chunked into more than one piece rejoins approximately. Saying so is
/// the difference between the model treating a near-copy as exact and it
/// knowing to re-read when the exact bytes matter.
pub(super) const CCR_INDEX_RECOVERY_NOTE: &str = "[Recovered from the context index. \
     The original expired from the retrieval store, so this was rebuilt from \
     the indexed copy: the text is complete but whitespace between sections \
     may differ from what you first read. Re-read the source if you need the \
     exact bytes.]";

// Shared CCR hash check lives in `headroom_core::ccr::response_handler`
// so the proxy, batch, and streaming paths agree on what counts as
// malformed (see `is_plausible_ccr_hash` there). No local copy.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_ccr_response(
    body_bytes: &bytes::Bytes,
    forwarded_request: &bytes::Bytes,
    upstream_url: &url::Url,
    client: &reqwest::Client,
    ccr_store: &dyn headroom_core::ccr::CcrStore,
    stores: Option<&std::sync::Arc<crate::ctx::projects::ProjectStores>>,
    config: &Config,
    request_id: &str,
    outgoing_headers: &http::HeaderMap,
    provider: &str,
    redact: Option<crate::redact::RedactRef>,
) -> (bytes::Bytes, CcrRoundUsage) {
    // Usage from every response this function replaces. The caller parses the
    // usage of the body we return, so accounting for that one here too would
    // double-count it.
    let mut round_usage = CcrRoundUsage::default();
    // The continuation-array field name varies by provider shape: Anthropic
    // and OpenAI chat-completions both use `messages`; OpenAI Responses uses
    // `input`.
    let items_field = if provider == "openai_responses" {
        "input"
    } else {
        "messages"
    };
    use headroom_core::ccr::response_handler::{CCRResponseHandler, CcrToolResult};

    let handler = CCRResponseHandler::new(Some(
        headroom_core::ccr::response_handler::ResponseHandlerConfig {
            enabled: true,
            max_retrieval_rounds: config.ccr_max_retrieval_rounds,
            strip_ccr_from_response: false,
        },
    ));

    // Parse the response body.
    let Some(response) = ccr_response::parse_ccr_response(body_bytes, request_id) else {
        return (body_bytes.clone(), round_usage);
    };

    if !handler.has_ccr_tool_calls(&response, provider) {
        return (body_bytes.clone(), round_usage);
    }

    let mut current_response = response.clone();
    let Some(mut current_request) = ccr_response::parse_ccr_request(forwarded_request, request_id)
    else {
        return (body_bytes.clone(), round_usage);
    };

    // The hot zone as the provider cached it on the first round. Every
    // continuation is checked against this, since none of them reach the
    // forwarding path where outbound drift is observed.
    let base_kind = continuation_api_kind(provider);
    let base_hash = base_kind.map(|kind| compute_structural_hash(&current_request, kind));

    // Which offloaded Reads the conversation has already invalidated. Computed
    // once from the request as it arrived: continuation rounds only append tool
    // results, so no later round can make a Read stale that was not stale here.
    let stale_reads = current_request
        .get(items_field)
        .and_then(|m| m.as_array())
        .map(|messages| crate::compression::ctx_offload::stale_offloaded_reads(messages))
        .unwrap_or_default();

    let max_rounds = config.ccr_max_retrieval_rounds;
    let mut rounds = 0;
    // Cut-stream retries spent resending an identical continuation whose SSE
    // body ended with no terminal event (see continuation_stream_terminal).
    // Per-turn, not per-round: a sick route must fail fast to fallback (a)
    // rather than multiply re-sends across rounds.
    let mut cut_attempts: u32 = 0;
    // Last successfully fetched retrieval content, kept across rounds for
    // fallback (a): if a later round's upstream continuation dies, the turn
    // still resolves with what the store already returned.
    let mut last_fetched: Vec<headroom_core::ccr::response_handler::CcrToolResult> = Vec::new();

    loop {
        if !ccr_response::check_ccr_round_budget(rounds, max_rounds, request_id) {
            break;
        }

        let (ccr_calls, other_calls) = handler.parse_ccr_tool_calls(&current_response, provider);

        if ccr_calls.is_empty() {
            break;
        }

        // Fetch original content for each CCR call.
        let mut results: Vec<CcrToolResult> = Vec::new();
        for call in &ccr_calls {
            results.push(
                ccr_response::fetch_one_ccr_call(
                    call,
                    ccr_store,
                    stores,
                    outgoing_headers,
                    &current_request,
                    config,
                    &stale_reads,
                    &redact,
                    request_id,
                    rounds,
                )
                .await,
            );
        }

        // Mixed CCR + real tool calls, and all-failed rounds, splice in
        // place when possible; otherwise the loop continues below.
        if ccr_response::check_ccr_round_fate(
            &handler,
            &mut current_response,
            &results,
            ccr_calls.len(),
            other_calls.len(),
            provider,
            request_id,
        ) {
            break;
        }

        // Snapshot successes for fallback (a): `results` is rebuilt each
        // round, but the splice site below is outside the loop.
        last_fetched = results
            .iter()
            .filter(|r| r.success && !r.content.is_empty())
            .cloned()
            .collect();
        // Build continuation messages: append assistant message + tool results.
        let assistant_msg = handler.extract_assistant_message(&current_response, provider);
        let tool_result_msg = handler.create_tool_result_message(&results, provider);

        let Some(continuation_body) = ccr_response::build_ccr_continuation(
            &mut current_request,
            items_field,
            assistant_msg,
            tool_result_msg,
            provider,
            config,
            request_id,
            rounds,
        ) else {
            break;
        };

        // A continuation that fails is not a soft outcome: the retrieval is
        // already parsed and the content already fetched, and giving up here
        // leaves the model with an unanswered `headroom_retrieve`.
        let continuation_started = std::time::Instant::now();
        let base = base_hash.as_ref().zip(base_kind);
        let send = ccr_response::send_ccr_continuation(
            client,
            upstream_url,
            outgoing_headers,
            continuation_body,
            &results,
            max_rounds,
            base,
            &current_request,
            request_id,
            rounds,
        )
        .await;
        let attempts = send.attempts;
        let Some(resp) = ccr_response::unwrap_ccr_send(
            send.outcome,
            attempts,
            &continuation_started,
            request_id,
        ) else {
            break;
        };
        let Some(resp) = ccr_response::check_ccr_response_status(
            resp,
            provider,
            rounds,
            attempts,
            &continuation_started,
            request_id,
        )
        .await
        else {
            break;
        };

        // `bytes()` consumes the response; the fold needs the body.
        match ccr_response::read_ccr_round_body(
            resp,
            provider,
            &mut cut_attempts,
            &mut round_usage,
            &current_response,
            request_id,
        )
        .await
        {
            ccr_response::CcrRoundRead::Advance(next) => {
                current_response = next;
            }
            ccr_response::CcrRoundRead::Retry => continue,
            ccr_response::CcrRoundRead::Done => break,
        }

        rounds += 1;
    }

    // Classify the outcome before returning. Only claim success when no
    // `headroom_retrieve` remains; a retrieve left standing is a real failure.
    ccr_response::resolve_ccr_residual(
        &handler,
        &mut current_response,
        provider,
        &last_fetched,
        request_id,
    );

    match serde_json::to_vec(&current_response) {
        Ok(bytes) => (bytes::Bytes::from(bytes), round_usage),
        Err(_) => (body_bytes.clone(), round_usage),
    }
}

/// Retries for a memory continuation that fails for a reason that may pass.
pub(super) const MEMORY_CONTINUATION_RETRIES: u32 = 2;

/// Backoff before continuation attempt `attempt` (1-based): 250ms, then 500ms.
pub(super) fn memory_continuation_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(250u64 << (attempt.saturating_sub(1)).min(4))
}
