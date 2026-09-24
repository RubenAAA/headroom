//! Upstream addressing and response inspection: overrides, URL joins,
//! beta headers, rate-limit and request-id capture, error descriptions,
//! and in-band SSE error peeking.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Whether an upstream `reqwest` send error is a transient transport
/// failure worth retrying on a fresh connection.
///
/// Ports Python's broadening (commits 2ce19c2c + 5d14080c) from the
/// narrow `(ConnectError, Timeout)` set to any `httpx.TransportError`.
/// The `httpx` transport family includes h2 stream resets
/// (`RemoteProtocolError`/`StreamReset`) and pooled keep-alive
/// connections closed mid-response (`incomplete chunked read`). Under
/// concurrent load a single poisoned HTTP/2 connection would otherwise
/// cascade every in-flight request to a 502 with no reconnect.
///
/// In `reqwest` these surface as connect/timeout errors OR as
/// request/body-level errors (`is_request` covers a stream reset while
/// sending; `is_body` covers an incomplete response body read). We
/// deliberately exclude `is_status`/`is_decode`/`is_builder`, which are
/// not transport-transient and must not be retried.
pub(crate) fn is_retryable_transport_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body()
}

/// Append a token to the `anthropic-beta` header, preserving existing tokens
/// and skipping if already present.
pub(super) fn append_anthropic_beta(headers: &mut http::HeaderMap, beta: &str) {
    const NAME: &str = "anthropic-beta";
    let existing = headers
        .get(NAME)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if existing.split(',').any(|t| t.trim() == beta) {
        return;
    }
    let merged = if existing.is_empty() {
        beta.to_string()
    } else {
        format!("{existing},{beta}")
    };
    if let Ok(val) = http::HeaderValue::from_str(&merged) {
        headers.insert(NAME, val);
    }
}

/// Per-request upstream base override, inserted into request
/// extensions by provider routes that forward to a different upstream
/// than `--upstream` (currently: the Azure AI Foundry route,
/// [`crate::foundry::handle_foundry_messages`], when
/// `Config::foundry_base_url` is configured). `forward_http` reads it
/// back out when building the upstream URL; absent extension means
/// `Config::upstream` as before.
#[derive(Clone, Debug)]
pub struct UpstreamOverride(pub url::Url);

/// A chosen upstream and the only transport permitted to connect to it.
/// Keeping these together prevents later retry/continuation paths from
/// accidentally switching a caller-controlled URL back to the trusted client.
pub(super) struct SelectedUpstream {
    pub(super) base: url::Url,
    pub(super) client: reqwest::Client,
    pub(super) configured_http_proxy: bool,
    pub(super) allow_slow_path_probe: bool,
}

/// Resolve a per-request upstream override from the `x-headroom-base-url`
/// request header. Returns `None` when the header is absent, empty, or
/// whitespace-only (after trimming), or when the value does not parse as a
/// URL — in all those cases the caller falls back to the default upstream.
/// The value is trimmed and a single trailing `/` is stripped, matching the
/// Python proxy's `.strip().rstrip("/")` contract.
pub(super) async fn header_upstream_override(
    headers: &HeaderMap,
) -> Option<crate::upstream_guard::ResolvedCallerUpstream> {
    let raw = headers
        .get(crate::headers::UPSTREAM_OVERRIDE_HEADER)
        .and_then(|v| v.to_str().ok())?;
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match url::Url::parse(trimmed) {
        Ok(url) => {
            let resolved =
                crate::upstream_guard::ResolvedCallerUpstream::resolve(url.clone()).await;
            if resolved.is_none() {
                tracing::warn!(
                    event = "upstream_override_rejected",
                    header = crate::headers::UPSTREAM_OVERRIDE_HEADER,
                    value = %url,
                    "ignoring unsafe x-headroom-base-url; using default upstream"
                );
            }
            resolved
        }
        Err(e) => {
            tracing::warn!(
                event = "upstream_override_parse_failed",
                header = crate::headers::UPSTREAM_OVERRIDE_HEADER,
                value = %trimmed,
                error = %e,
                "ignoring malformed x-headroom-base-url; using default upstream"
            );
            None
        }
    }
}

/// Build the upstream URL by joining the configured base with the incoming
/// path-and-query. Preserves '?' and the query string verbatim.
pub(crate) fn build_upstream_url(base: &url::Url, uri: &Uri) -> Result<url::Url, ProxyError> {
    Ok(join_upstream_path(base, uri.path(), uri.query()))
}

/// Shared path-join helper used by HTTP and WebSocket handlers.
/// Appends `path` to `base`, preserving any base path prefix, then sets `query`.
pub(crate) fn join_upstream_path(base: &url::Url, path: &str, query: Option<&str>) -> url::Url {
    let mut joined = base.clone();
    // Strip trailing slash from base path so "http://x:1/api" + "/v1/foo"
    // yields "http://x:1/api/v1/foo" rather than "http://x:1/v1/foo".
    let base_path = joined.path().trim_end_matches('/').to_string();
    let combined = if path.is_empty() || path == "/" {
        if base_path.is_empty() {
            "/".to_string()
        } else {
            base_path
        }
    } else if base_path.is_empty() {
        path.to_string()
    } else {
        format!("{base_path}{path}")
    };
    joined.set_path(&combined);
    joined.set_query(query);
    joined
}

/// Assemble the client-bound upstream byte stream: re-prepend bytes peeked while
/// checking for a leading in-band error (`sse_prefix` is empty on paths that did
/// not peek, so that step is a no-op there), then wrap the stream so an early
/// drop retries from a held-back opening instead of reaching the client. The
/// retry loop above only ever saw the headers; this wrapper sits below CCR and
/// below the telemetry tee, so a discarded attempt is invisible to both.
pub(super) fn assemble_upstream_body(
    upstream_resp: reqwest::Response,
    sse_prefix: bytes::Bytes,
    retry_body: Option<bytes::Bytes>,
    is_sse: bool,
    status: StatusCode,
    retry: UpstreamBodyRetry,
    slow_upstream_probe: Option<crate::upstream_route_probe::SlowUpstreamProbe>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>> {
    let upstream_body = {
        let rest = upstream_resp.bytes_stream();
        let head =
            futures_util::stream::iter((!sse_prefix.is_empty()).then(|| Ok(sse_prefix.clone())));
        Box::pin(head.chain(rest))
    };
    let upstream_body =
        crate::upstream_route_probe::cancel_on_first_chunk(upstream_body, slow_upstream_probe);
    if let Some(body) = retry_body.filter(|_| {
        is_sse
            && status.is_success()
            && retry.enabled
            && retry.hold_bytes > 0
            && retry.max_attempts > 1
    }) {
        Box::pin(crate::sse::stream_retry::retry_on_early_drop(
            upstream_body,
            crate::sse::stream_retry::RetryContext {
                client: retry.client,
                method: retry.method,
                url: retry.url,
                headers: retry.headers,
                body,
                request_id: retry.request_id,
                max_attempts: retry.max_attempts,
                base_delay_ms: retry.base_delay_ms,
                max_delay_ms: retry.max_delay_ms,
                hold_bytes: retry.hold_bytes,
            },
        ))
    } else {
        Box::pin(upstream_body)
    }
}

/// By-value retry inputs for [`assemble_upstream_body`], cloned out of the live
/// request state at the call site so the stream wrapper owns what it needs.
pub(super) struct UpstreamBodyRetry {
    pub(super) enabled: bool,
    pub(super) hold_bytes: usize,
    pub(super) max_attempts: u32,
    pub(super) client: reqwest::Client,
    pub(super) method: reqwest::Method,
    pub(super) url: String,
    pub(super) headers: http::HeaderMap,
    pub(super) request_id: String,
    pub(super) base_delay_ms: u64,
    pub(super) max_delay_ms: u64,
}

/// Phase G PR-G3: extract upstream rate-limit headers from this response and
/// record them as gauges. The `provider` label comes from which upstream
/// `request-id` shape was seen (Anthropic vs OpenAI); when neither was detected
/// emission is skipped rather than guessed ("no silent fallbacks").
///
/// Also parses + records the Subscription/OAuth `unified-*` family, which the
/// `*-remaining` gauges never see on a Claude-subscription plan. A non-empty
/// unified snapshot is self-attributing, so no provider label is needed.
pub(super) fn record_upstream_rate_limits(
    headers: &http::HeaderMap,
    has_anthropic_request_id: bool,
    has_openai_request_id: bool,
    request_path: &str,
    request_id: &str,
) {
    let rate_limit_snapshot = crate::observability::extract_rate_limit_snapshot(headers);
    let rate_limit_provider: Option<&'static str> = if has_anthropic_request_id {
        Some(crate::observability::cache_hit_rate_provider::ANTHROPIC)
    } else if has_openai_request_id {
        // We can't distinguish chat vs responses purely from the
        // request-id header; the `request_path` is more specific.
        Some(if request_path.contains("/v1/responses") {
            crate::observability::cache_hit_rate_provider::OPENAI_RESPONSES
        } else {
            crate::observability::cache_hit_rate_provider::OPENAI_CHAT
        })
    } else {
        None
    };
    if let Some(provider) = rate_limit_provider {
        crate::observability::record_rate_limit_snapshot(
            provider,
            &rate_limit_snapshot,
            request_id,
        );
    } else if rate_limit_snapshot.remaining_requests.is_some()
        || rate_limit_snapshot.remaining_tokens.is_some()
        || rate_limit_snapshot.remaining_input_tokens.is_some()
        || rate_limit_snapshot.remaining_output_tokens.is_some()
    {
        // Headers present but provider unattributable. Log loud so
        // operators see the wire-format drift; do not emit unlabelled
        // metrics.
        tracing::debug!(
            event = "rate_limit_snapshot_unattributable",
            request_id = %request_id,
            path = %request_path,
            "rate-limit headers present but provider couldn't be inferred; skipping gauge emit"
        );
    }

    // Subscription / OAuth traffic carries the `anthropic-ratelimit-
    // unified-*` family instead of `*-remaining` — the headers above
    // stay None on a Claude-subscription plan, so the `*-remaining`
    // gauges never populate. Parse + record the unified family too so
    // subscription headroom (utilization per 5h/7d window) is visible.
    // Provider-agnostic: the unified prefix is Anthropic-specific, so a
    // non-empty snapshot is self-attributing.
    let unified_snapshot = crate::observability::extract_unified_rate_limit(headers);
    if !unified_snapshot.windows.is_empty()
        || unified_snapshot.overall_status.is_some()
        || unified_snapshot.fallback_percentage.is_some()
    {
        crate::observability::record_unified_rate_limit(&unified_snapshot, request_id);
    }
}

/// PR-A8 / P5-57: capture the upstream request id BEFORE the caller moves
/// `upstream_resp.headers()` into the response filter. Anthropic emits
/// `request-id` (lowercase, no `x-`); OpenAI emits `x-request-id`.
/// Returns `(anthropic, openai, preferred)`; when both are present the
/// Anthropic one wins since it is the path-shape the cache invariants lock down.
pub(super) fn capture_upstream_request_ids(
    headers: &http::HeaderMap,
) -> (Option<String>, Option<String>, Option<String>) {
    let anthropic = headers
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let openai = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // Prefer the provider-specific id whichever was set. Both
    // present is unusual but legal; prefer Anthropic since it's the
    // path-shape we lockdown with cache invariants.
    let preferred = anthropic.clone().or_else(|| openai.clone());
    (anthropic, openai, preferred)
}

/// The provider's own words for why it refused a request, as
/// `(error type, message)`.
///
/// Reads the two error envelopes the proxy forwards to — Anthropic's
/// `{"error": {"type", "message"}}` and OpenAI's `{"error": {"code", "message"}}`
/// — and returns those fields only. The raw body never reaches the log: an
/// unrecognised shape yields empty strings rather than whatever bytes the
/// upstream happened to send, because this runs on every failed request and the
/// log is not a place to spill unknown payloads.
pub(super) fn describe_upstream_error(body: &[u8]) -> (String, String) {
    const MAX_MESSAGE_CHARS: usize = 400;
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (String::from("unparsed"), String::new());
    };
    let Some(error) = value.get("error") else {
        return (String::from("no_error_field"), String::new());
    };
    let kind = error
        .get("type")
        .or_else(|| error.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let message: String = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(MAX_MESSAGE_CHARS)
        .collect();
    (kind, message)
}

/// Error types Anthropic reports in-band that a retry can plausibly clear.
/// `invalid_request_error` and friends are excluded: resending an identical
/// body gets an identical refusal.
pub(super) const RETRYABLE_IN_BAND_ERRORS: &[&str] =
    &["overloaded_error", "rate_limit_error", "api_error"];

/// Read just far enough into a streamed body to see whether it opens with an
/// error event, and hand back every byte consumed so the caller can put them
/// in front of the rest of the stream.
///
/// Returns `(prefix, Some(error_type))` when the first complete SSE event is a
/// retryable error, `(prefix, None)` otherwise. The prefix is always the exact
/// bytes read — on the ordinary path that is one `message_start` chunk, which
/// then leads the client's stream unchanged.
///
/// Bounded twice over: it stops at the first event terminator and gives up
/// after `MAX_PEEK_BYTES`. A body that never produces a blank line is a body
/// this proxy should not be buffering.
pub(super) async fn peek_leading_sse_error(
    resp: &mut reqwest::Response,
) -> (bytes::Bytes, Option<&'static str>) {
    /// One SSE event is a few hundred bytes; 16 KiB is slack, not a budget.
    const MAX_PEEK_BYTES: usize = 16 * 1024;

    let is_sse = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if !is_sse {
        return (bytes::Bytes::new(), None);
    }

    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        // A transport error here is not ours to classify — hand back what we
        // have and let the normal stream path surface it.
        let Ok(chunk) = resp.chunk().await else {
            return (bytes::Bytes::from(buf), None);
        };
        let Some(chunk) = chunk else {
            // Body ended before a complete event. Nothing to retry on.
            return (bytes::Bytes::from(buf), None);
        };
        buf.extend_from_slice(&chunk);

        if let Some(end) = find_event_end(&buf) {
            let kind = leading_event_error_type(&buf[..end]);
            return (bytes::Bytes::from(buf), kind);
        }
        if buf.len() >= MAX_PEEK_BYTES {
            return (bytes::Bytes::from(buf), None);
        }
    }
}

/// Offset just past the first event terminator, tolerating CRLF.
pub(super) fn find_event_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| i + 2)
        .or_else(|| buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4))
}

/// The retryable error type carried by one framed SSE event, if any.
pub(super) fn leading_event_error_type(event: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(event).ok()?;
    let data = text
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .map(str::trim)?;
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    if parsed.get("type").and_then(serde_json::Value::as_str) != Some("error") {
        return None;
    }
    let kind = parsed
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(serde_json::Value::as_str)?;
    RETRYABLE_IN_BAND_ERRORS
        .iter()
        .find(|known| **known == kind)
        .copied()
}

/// Leading bytes of an upstream error body, for a log line that has to stay
/// one line. Upstream puts the useful part first.
pub(super) fn first_bytes(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.replace('\n', " ");
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…[{} more bytes]",
        &s[..end].replace('\n', " "),
        s.len() - end
    )
}

/// The item an upstream 400 points at (`input[N]` / `messages[N]` in the
/// error text), with string values cut to 80 chars, or `-` when the error
/// names no index or the index is out of range.
pub(super) fn rejected_item_summary(
    detail: &str,
    request: &serde_json::Value,
    items_field: &str,
) -> String {
    let idx = detail
        .find(&format!("{items_field}["))
        .map(|start| start + items_field.len() + 1)
        .and_then(|start| {
            let digits: String = detail[start..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse::<usize>().ok()
        });
    let Some(item) = idx.and_then(|i| request.get(items_field)?.get(i)) else {
        return "-".to_string();
    };
    fn shorten(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) if s.chars().count() > 80 => {
                serde_json::Value::String(format!("{}…", s.chars().take(80).collect::<String>()))
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(shorten).collect())
            }
            serde_json::Value::Object(o) => {
                serde_json::Value::Object(o.iter().map(|(k, v)| (k.clone(), shorten(v))).collect())
            }
            other => other.clone(),
        }
    }
    format!("{}[{}]={}", items_field, idx.unwrap_or(0), shorten(item))
}
