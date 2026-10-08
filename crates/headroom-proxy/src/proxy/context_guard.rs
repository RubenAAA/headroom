//! Context-limit guard: keep client auto-compaction working under compression.
//!
//! Port of upstream `headroom/proxy/context_guard.py` (fix #3998, PR #2942).
//! Compression makes the usage reported back to the client reflect the
//! *forwarded* (compressed) request, not the client's raw transcript. Clients
//! whose auto-compaction keys off reported usage (Claude Code) therefore defer
//! compaction — the selling point — but with no floor: the raw transcript can
//! grow until even the compressed request exceeds the model's real context
//! window. From that point every turn fails with a 400 `prompt is too long`,
//! the client force-compacts on each error, and the session degrades into a
//! compact-every-other-prompt loop (field report 2026-08-12).
//!
//! Three cooperating pieces, all passthrough until the danger zone:
//!
//! 1. [`effective_context_limit`] — the window the forwarded request is really
//!    subject to: the model's registry limit, raised to 1M when the client sent
//!    a `context-1m` beta token, capped by any limit *learned* from an actual
//!    `prompt is too long` error (piece 2).
//! 2. [`note_prompt_too_long`] — parses `prompt is too long: N tokens > M
//!    maximum` from upstream 400 bodies and records M per (model, 1m-beta,
//!    credential-scope) key. This is how a client that *believes* 1M but whose
//!    account is capped at 200k stops looping after exactly one error.
//! 3. [`StreamUsageGuard`] — an SSE transformer for the streamed bytes only:
//!    when the `message_start` usage total reaches [`TRIGGER_FRACTION`] of the
//!    effective limit, it inflates the reported `input_tokens` — in both
//!    `message_start` and the final cumulative-usage `message_delta`, which
//!    clients merge over the former — so the client's gauge reads
//!    [`REPORT_FRACTION`] of the window the client believes in, which trips
//!    the client's own graceful compaction before the 400 wall. [`nudge_response_usage`]
//!    does the same for non-streaming JSON bodies. Savings metrics and
//!    telemetry parse the original upstream bytes, never the nudged ones: the
//!    guard wraps the client-bound leg *below* the parser tee.
//!
//! The nudged number is deliberately unmarked on the wire. It exists to be read
//! by the client's own compaction gauge, which is a third-party parser we do not
//! control, so anything added beside it is either ignored or a wire-shape change
//! against a client we cannot test. Nothing inside headroom needs the marker:
//! savings, telemetry and billing all read the original upstream bytes, never the
//! client-bound ones, and each rewrite emits `event=context_guard_nudge` with
//! the forwarded and reported totals for reconciliation.
//!
//! Kill switch: `HEADROOM_CONTEXT_GUARD=0`.
//!
//! Scope notes vs Python: the guard is Anthropic-only on both sides (the
//! streaming wiring gates on `provider == "anthropic"`). Python also feeds the
//! effective limit into compression pressure; the Rust live zone does not key
//! pressure off a model window, so there is no counterpart to wire — the limit
//! helpers exist here for the guard alone.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Forwarded-usage fraction of the *effective* (real) limit at which the nudge
/// arms. 0.90 leaves one normal turn of headroom before the hard wall.
pub const TRIGGER_FRACTION: f64 = 0.90;
/// Reported-usage fraction of the *believed* limit the nudge inflates to.
/// Claude Code auto-compacts around 92%; 0.95 clears that with margin.
pub const REPORT_FRACTION: f64 = 0.95;
const CONTEXT_1M_PREFIX: &str = "context-1m";
/// A learned maximum is believed only inside this band. Below the floor it is
/// not a context window: gateways return the same "prompt is too long: N > M"
/// shape for per-request quota and per-key throttles, and one such number would
/// clamp every later request on that model for the life of the process. Above
/// the ceiling it cannot cap anything anyway.
const MIN_LEARNED_LIMIT: u64 = 8_000;
const MAX_LEARNED_LIMIT: u64 = 10_000_000;
/// A learned limit is evidence about one account at one moment, not a fact about
/// the model: entitlements get provisioned, gateways get repointed. It expires so
/// a stale cap cannot hold a budget down forever — the cost of expiry is at most
/// one more prompt-too-long error, which is what taught it the first time.
const LEARNED_LIMIT_TTL_SECS: u64 = 3600;
/// Give up scanning for message_start beyond this many buffered bytes.
const MAX_GUARD_BUFFER: usize = 256 * 1024;

/// True unless explicitly disabled via `HEADROOM_CONTEXT_GUARD=0`.
pub fn context_guard_enabled() -> bool {
    let raw = std::env::var("HEADROOM_CONTEXT_GUARD").unwrap_or_else(|_| "1".to_string());
    let v = if raw.is_empty() { "1" } else { raw.trim() };
    !matches!(
        v.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// True when the anthropic-beta header carries a context-1m token.
///
/// Case-insensitive plain prefix match, exactly like Python's
/// `has_context_1m_beta`: any comma-separated token whose lowercased form
/// starts with `context-1m` counts, including hypothetical `context-1mfoo`
/// spellings. The learned-limit key and the believed-limit raise share this
/// predicate, so both sides must agree with upstream on what counts.
pub fn has_context_1m_beta(beta_header: Option<&str>) -> bool {
    let Some(header) = beta_header else {
        return false;
    };
    header.split(',').any(|token| {
        token
            .trim()
            .to_ascii_lowercase()
            .starts_with(CONTEXT_1M_PREFIX)
    })
}

/// The context window the *client* thinks it has.
pub fn believed_context_limit(model_limit: u64, beta_header: Option<&str>) -> u64 {
    if has_context_1m_beta(beta_header) {
        model_limit.max(1_000_000)
    } else {
        model_limit
    }
}

/// A stable, non-reversible tag for the upstream credential.
pub fn credential_scope(credential: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let Some(credential) = credential else {
        return String::new();
    };
    let trimmed = credential.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(trimmed.as_bytes());
    hex::encode(&digest[..8])
}

/// The credential scope for the outbound (upstream-bound) headers: `x-api-key`
/// first, else the whole `Authorization` value. Matches Python's
/// `credential_scope_from_headers`, which reads the same two in the same order
/// and hashes the value verbatim — including any `Bearer ` scheme prefix.
/// Only the mapping key/value lookup is best-effort here; `HeaderMap` lookups
/// cannot fail the way Python's untyped mapping can.
pub fn credential_scope_from_headers(headers: &http::HeaderMap) -> String {
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    // Like Python, an empty value doesn't count: fall through to
    // `authorization` rather than landing in the unscoped bucket.
    if let Some(key) = api_key.filter(|v| !v.is_empty()) {
        return credential_scope(Some(key));
    }
    let authorization = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    credential_scope(authorization)
}

type LearnedKey = (String, bool, String);

fn learned_store() -> &'static Mutex<HashMap<LearnedKey, (u64, Instant)>> {
    static STORE: OnceLock<Mutex<HashMap<LearnedKey, (u64, Instant)>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn learned_key(model: &str, beta_header: Option<&str>, scope: &str) -> LearnedKey {
    (
        model.to_string(),
        has_context_1m_beta(beta_header),
        scope.to_string(),
    )
}

/// The unexpired learned maximum for `key`, dropping it once stale. A poisoned
/// lock reads as no lesson: the guard must never break the byte path.
fn learned_limit(key: &LearnedKey) -> Option<u64> {
    let mut store = learned_store().lock().ok()?;
    let (maximum, learned_at) = store.get(key)?;
    if learned_at.elapsed().as_secs() >= LEARNED_LIMIT_TTL_SECS {
        store.remove(key);
        return None;
    }
    Some(*maximum)
}

/// The window the forwarded request is actually subject to.
///
/// Optimistically the believed limit; capped by a limit learned from a real
/// `prompt is too long` error for the same (model, beta, credential) shape,
/// and only while that lesson is still fresh.
pub fn effective_context_limit(
    model: &str,
    model_limit: u64,
    beta_header: Option<&str>,
    scope: &str,
) -> u64 {
    let believed = believed_context_limit(model_limit, beta_header);
    let key = learned_key(model, beta_header, scope);
    learned_limit(&key).map_or(believed, |learned| believed.min(learned))
}

fn prompt_too_long_re() -> &'static regex_lite::Regex {
    static RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex_lite::Regex::new(r"(?i)prompt is too long:\s*(\d+)\s*tokens?\s*>\s*(\d+)\s*maximum")
            .expect("context-guard prompt-too-long regex")
    })
}

/// Learn the real context limit from an upstream prompt-too-long error.
///
/// Returns the learned maximum, or `None` when there is nothing to learn.
/// This runs on the error path, where the client is still waiting for the real
/// upstream body, so it accepts raw bytes and never fails: an unparseable body
/// simply teaches nothing.
pub fn note_prompt_too_long(
    model: &str,
    beta_header: Option<&str>,
    error_body: &[u8],
    scope: &str,
) -> Option<u64> {
    // Lossy like Python's `errors="replace"`: undecodable bytes elsewhere in
    // the body must not hide a match in the decodable part.
    let text = String::from_utf8_lossy(error_body);
    let caps = prompt_too_long_re().captures(&text)?;
    let maximum: u64 = caps.get(2)?.as_str().parse().ok()?;
    if !(MIN_LEARNED_LIMIT..=MAX_LEARNED_LIMIT).contains(&maximum) {
        return None;
    }
    let key = learned_key(model, beta_header, scope);
    // Through the shared read so a stale lesson is dropped before the fresh
    // one lands, like Python's `_learned_limit` call here — and no warn when
    // re-learning the same maximum, so a stuck client retrying the same
    // too-long prompt doesn't spam one warn per turn.
    let previous = learned_limit(&key);
    let mut store = learned_store().lock().ok()?;
    store.insert(key, (maximum, Instant::now()));
    if previous != Some(maximum) {
        tracing::warn!(
            event = "context_guard_learned_limit",
            model = %model,
            learned_maximum = maximum,
            "learned effective context limit from prompt-too-long error"
        );
    }
    Some(maximum)
}

/// The inflated `input_tokens` for this usage, or `None` to leave it be.
///
/// One implementation for every client-bound surface — `message_start`, the
/// final `message_delta`, and the non-streaming JSON body — so a request
/// cannot get the raised compression budget on one path and the un-nudged
/// gauge on another.
pub fn nudged_input_tokens(
    usage: &serde_json::Map<String, serde_json::Value>,
    believed_limit: u64,
    effective_limit: u64,
) -> Option<u64> {
    if believed_limit == 0 || effective_limit == 0 {
        return None;
    }
    // Present-but-non-numeric reads as 0 where Python's `int()` would raise
    // and skip the rewrite. Upstream usage is always integer-typed, so the
    // only divergence is on malformed wire data, where either outcome — a
    // nudge or none — is harmless: nudging only ever pushes the client's
    // gauge toward compaction, never away.
    let num = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let input_tokens = num("input_tokens");
    let cache_read = num("cache_read_input_tokens");
    let cache_creation = num("cache_creation_input_tokens");
    let total = input_tokens + cache_read + cache_creation;
    if (total as f64) < TRIGGER_FRACTION * (effective_limit as f64) {
        return None;
    }
    let target_total = (believed_limit as f64 * REPORT_FRACTION) as u64;
    if total >= target_total {
        // Already reads as full to the client — never deflate.
        return None;
    }
    Some(
        target_total
            .saturating_sub(cache_read)
            .saturating_sub(cache_creation),
    )
}

/// Nudge the usage of a non-streaming Anthropic message body.
///
/// The buffered path raises the compression budget exactly like the streaming
/// one, so it has to move the client's gauge exactly like the streaming one
/// too; otherwise a non-streaming client gets the larger forwarded request and
/// none of the warning that it is near the wall.
///
/// Returns the rewritten body when the usage genuinely needed nudging, `None`
/// otherwise, so byte-faithful forwarding still holds for every response that
/// is not near the wall.
pub fn nudge_response_usage(
    body: &[u8],
    believed_limit: u64,
    effective_limit: u64,
    request_id: &str,
) -> Option<bytes::Bytes> {
    let mut payload: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = payload.get_mut("usage")?.as_object_mut()?;
    let new_input = nudged_input_tokens(usage, believed_limit, effective_limit)?;
    let num = |usage: &serde_json::Map<String, serde_json::Value>, key: &str| {
        usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
    };
    let forwarded_total = num(usage, "input_tokens")
        + num(usage, "cache_read_input_tokens")
        + num(usage, "cache_creation_input_tokens");
    usage.insert(
        "input_tokens".to_string(),
        serde_json::Value::from(new_input),
    );
    let reported_total = new_input
        + num(usage, "cache_read_input_tokens")
        + num(usage, "cache_creation_input_tokens");
    tracing::warn!(
        request_id = %request_id,
        event = "context_guard_nudge",
        path = "non_streaming",
        forwarded_total,
        effective_limit,
        reported_total,
        believed_limit,
        "nudging client to compact before the prompt-too-long wall"
    );
    serde_json::to_vec(&payload).ok().map(bytes::Bytes::from)
}

/// Earliest complete SSE event terminator in `buf`: `(start, length)`,
/// supporting LF and CRLF framing. Mirrors Python's
/// `find_sse_event_terminator`.
fn find_sse_event_terminator(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subslice(buf, b"\n\n").map(|i| (i, 2));
    let crlf = find_subslice(buf, b"\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some((li, _)), Some((ci, _))) if ci < li => crlf,
        (Some(_), _) => lf,
        (None, crlf) => crlf,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The event type of one SSE frame, or `None` when it carries none.
///
/// Read from the `event:` line, not matched as a substring of the frame: a
/// substring match reads the *payload* too, so an assistant that writes the
/// words `message_delta` into its own reply ended the scan and lost the
/// final nudge.
///
/// `None` means the frame is not part of the message protocol at all —
/// an SSE comment, a `retry:` field, the keepalives an intermediary shim
/// injects. Those pass through without arming or disarming anything; treating
/// one as an unknown event disabled the guard for the whole stream.
fn sse_event_type(event: &[u8]) -> Option<Vec<u8>> {
    let mut saw_data = false;
    for line in event.split(|b| *b == b'\n') {
        if line.starts_with(b"event:") {
            return Some(trim_ascii(&line[b"event:".len()..]).to_vec());
        }
        if line.starts_with(b"data:") {
            saw_data = true;
        }
    }
    if !saw_data {
        return None;
    }
    // A data frame with no event: line defaults to the type inside the payload.
    for line in event.split(|b| *b == b'\n') {
        if !line.starts_with(b"data:") {
            continue;
        }
        let payload: serde_json::Value =
            serde_json::from_slice(trim_ascii(&line[b"data:".len()..])).unwrap_or_default();
        let value = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        return Some(value.as_bytes().to_vec());
    }
    Some(Vec::new())
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

/// A rewritten `data:` line keeping the original's CR ending.
fn data_line(payload: &serde_json::Value, original: &[u8]) -> Vec<u8> {
    let mut out = b"data: ".to_vec();
    out.extend_from_slice(&serde_json::to_vec(payload).unwrap_or_default());
    if original.ends_with(b"\r") {
        out.push(b'\r');
    }
    out
}

/// Rewrites near-the-wall usage in the client-bound SSE bytes.
///
/// Feed every chunk headed to the client through [`feed`](Self::feed); call
/// [`flush`](Self::flush) when the stream ends. The first `message_start`
/// decides whether the nudge is active. When it is not (the common case), the
/// guard goes inert immediately and chunks pass through untouched — steady-state
/// overhead is one `if`. When it is, the guard rewrites the `message_start`
/// usage AND stays armed for the final `message_delta`: since mid-2025 that
/// event carries cumulative input/cache usage and clients (verified live with
/// Claude Code 2026-08-12) let it override the `message_start` values, so
/// nudging only the first event loses the merge. Events are released as soon as
/// their `\n\n` terminator arrives, so no latency is added beyond SSE's own
/// event framing.
pub struct StreamUsageGuard {
    believed_limit: u64,
    effective_limit: u64,
    request_id: String,
    buf: Vec<u8>,
    seen_message_start: bool,
    target_total: Option<u64>,
    start_cache_read: u64,
    start_cache_creation: u64,
    done: bool,
}

impl StreamUsageGuard {
    pub fn new(believed_limit: u64, effective_limit: u64, request_id: &str) -> Self {
        Self {
            believed_limit,
            effective_limit,
            request_id: request_id.to_string(),
            buf: Vec::new(),
            seen_message_start: false,
            target_total: None,
            start_cache_read: 0,
            start_cache_creation: 0,
            // Inert immediately when limits are unusable.
            done: believed_limit == 0 || effective_limit == 0,
        }
    }

    /// The limits this guard nudges toward, for the armed-site debug log.
    pub fn believed_limit(&self) -> u64 {
        self.believed_limit
    }

    /// The window the forwarded request is actually subject to.
    pub fn effective_limit(&self) -> u64 {
        self.effective_limit
    }

    pub fn feed(&mut self, chunk: &[u8]) -> bytes::Bytes {
        if self.done {
            return bytes::Bytes::copy_from_slice(chunk);
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while !self.done {
            // LF or CRLF framing: a CRLF stream never contains `\n\n` and
            // was held until the buffer cap, so it stopped streaming.
            let Some((start, len)) = find_sse_event_terminator(&self.buf) else {
                break;
            };
            let end = start + len;
            let event = self.buf[..end].to_vec();
            self.buf.drain(..end);
            out.extend_from_slice(&self.process_event(&event));
        }
        // Capped on what is still unframed, so one large chunk carrying
        // message_start is read before the cap can disarm the guard.
        if self.done || self.buf.len() > MAX_GUARD_BUFFER {
            out.extend_from_slice(&self.finish());
        }
        bytes::Bytes::from(out)
    }

    /// Return any held-back bytes; the guard goes inert.
    pub fn flush(&mut self) -> bytes::Bytes {
        self.finish()
    }

    fn finish(&mut self) -> bytes::Bytes {
        self.done = true;
        bytes::Bytes::from(std::mem::take(&mut self.buf))
    }

    fn process_event(&mut self, event: &[u8]) -> Vec<u8> {
        let event_type = sse_event_type(event);
        // Pings, comments, retry fields, keepalives: not part of the message
        // protocol, so they neither arm nor disarm the guard.
        if event_type.is_none() || event_type.as_deref() == Some(b"ping".as_slice()) {
            return event.to_vec();
        }
        if !self.seen_message_start {
            // Anything data-bearing that isn't message_start (error events,
            // protocol changes) ends the scan.
            self.seen_message_start = true;
            if event_type.as_deref() != Some(b"message_start".as_slice()) {
                self.done = true;
                return event.to_vec();
            }
            let rewritten = self
                .rewrite_message_start(event)
                .unwrap_or_else(|| event.to_vec());
            if self.target_total.is_none() {
                // Below trigger (or already reads full): nothing to nudge.
                self.done = true;
            }
            return rewritten;
        }
        // Armed: pass content deltas through, rewrite the final usage delta.
        if event_type.as_deref() != Some(b"message_delta".as_slice()) {
            return event.to_vec();
        }
        self.done = true;
        self.rewrite_message_delta(event)
            .unwrap_or_else(|| event.to_vec())
    }

    fn rewrite_message_start(&mut self, event: &[u8]) -> Option<Vec<u8>> {
        let mut lines: Vec<&[u8]> = event.split(|b| *b == b'\n').collect();
        for (i, line) in lines.clone().iter().enumerate() {
            if !line.starts_with(b"data:") {
                continue;
            }
            let mut payload: serde_json::Value =
                serde_json::from_slice(trim_ascii(&line[b"data:".len()..])).ok()?;
            if payload.get("type").and_then(|v| v.as_str()) != Some("message_start") {
                return Some(event.to_vec());
            }
            let usage = payload
                .get_mut("message")?
                .get_mut("usage")?
                .as_object_mut()?;
            let num = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_read = num("cache_read_input_tokens");
            let cache_creation = num("cache_creation_input_tokens");
            let total = num("input_tokens") + cache_read + cache_creation;
            let new_input = nudged_input_tokens(usage, self.believed_limit, self.effective_limit)?;
            let target_total = new_input + cache_read + cache_creation;
            self.target_total = Some(target_total);
            self.start_cache_read = cache_read;
            self.start_cache_creation = cache_creation;
            usage.insert(
                "input_tokens".to_string(),
                serde_json::Value::from(new_input),
            );
            tracing::warn!(
                request_id = %self.request_id,
                event = "context_guard_nudge",
                forwarded_total = total,
                effective_limit = self.effective_limit,
                reported_total = target_total,
                believed_limit = self.believed_limit,
                "nudging client to compact before the prompt-too-long wall"
            );
            lines[i] = &[];
            let mut joined = Vec::new();
            for (j, l) in lines.iter().enumerate() {
                if j > 0 {
                    joined.push(b'\n');
                }
                if j == i {
                    joined.extend_from_slice(&data_line(&payload, line));
                } else {
                    joined.extend_from_slice(l);
                }
            }
            return Some(joined);
        }
        Some(event.to_vec())
    }

    fn rewrite_message_delta(&mut self, event: &[u8]) -> Option<Vec<u8>> {
        let target_total = self.target_total?;
        let lines: Vec<&[u8]> = event.split(|b| *b == b'\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.starts_with(b"data:") {
                continue;
            }
            let mut payload: serde_json::Value =
                serde_json::from_slice(trim_ascii(&line[b"data:".len()..])).ok()?;
            if payload.get("type").and_then(|v| v.as_str()) != Some("message_delta") {
                return Some(event.to_vec());
            }
            let usage = payload.get_mut("usage")?.as_object_mut()?;
            // A delta without cumulative input usage can't override the
            // message_start values we already rewrote — leave it alone.
            if !usage.contains_key("input_tokens") {
                return Some(event.to_vec());
            }
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(self.start_cache_read);
            let cache_creation = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(self.start_cache_creation);
            let new_input = target_total
                .saturating_sub(cache_read)
                .saturating_sub(cache_creation);
            if usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                >= new_input
            {
                // Never deflate.
                return Some(event.to_vec());
            }
            usage.insert(
                "input_tokens".to_string(),
                serde_json::Value::from(new_input),
            );
            let mut joined = Vec::new();
            for (j, l) in lines.iter().enumerate() {
                if j > 0 {
                    joined.push(b'\n');
                }
                if j == i {
                    joined.extend_from_slice(&data_line(&payload, line));
                } else {
                    joined.extend_from_slice(l);
                }
            }
            return Some(joined);
        }
        Some(event.to_vec())
    }
}

/// The (believed, effective) limits for one Anthropic request, or `None` when
/// the guard is off.
///
/// `outbound_beta` is the upstream-bound `anthropic-beta` header (post-auth
/// rewrite); `client_beta` is what the client sent — the client's gauge follows
/// the beta the client sent, while session-sticky merging can add context-1m
/// to the outbound header after the client dropped it. `None` falls back to the
/// outbound header.
pub fn guard_limits_for(
    model: &str,
    outbound_beta: Option<&str>,
    client_beta: Option<&str>,
    scope: &str,
) -> Option<(u64, u64)> {
    if !context_guard_enabled() {
        return None;
    }
    let model_limit = u64::from(crate::compression::model_limits::context_window_for(model));
    if model_limit == 0 {
        return None;
    }
    let believed = believed_context_limit(model_limit, client_beta.or(outbound_beta));
    let effective = effective_context_limit(model, model_limit, outbound_beta, scope);
    Some((believed, effective))
}

/// A [`StreamUsageGuard`] for one Anthropic SSE response, or `None` when the
/// guard is off or the model is unknown (pure-passthrough path: byte-faithful
/// by contract, no request parse to read the model from).
pub fn stream_guard_for(
    model: Option<&str>,
    outbound_beta: Option<&str>,
    client_beta: Option<&str>,
    scope: &str,
    request_id: &str,
) -> Option<StreamUsageGuard> {
    let (believed, effective) = guard_limits_for(model?, outbound_beta, client_beta, scope)?;
    Some(StreamUsageGuard::new(believed, effective, request_id))
}

/// The `model` field of a buffered Anthropic request body.
pub fn request_model(body: &[u8]) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    parsed
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// Wraps a client-bound stream with an optional [`StreamUsageGuard`].
///
/// `None` is a pure passthrough, so the streaming assembly always wraps and
/// the guard-or-not decision stays at the construction site. Upstream errors
/// pass through untouched; the guard's held-back tail is released when the
/// stream ends.
pub struct GuardedStream<S> {
    inner: std::pin::Pin<Box<S>>,
    guard: Option<StreamUsageGuard>,
}

impl<S> GuardedStream<S> {
    pub fn new(inner: S, guard: Option<StreamUsageGuard>) -> Self {
        Self {
            inner: Box::pin(inner),
            guard,
        }
    }
}

impl<S> futures_util::Stream for GuardedStream<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send,
{
    type Item = Result<bytes::Bytes, reqwest::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                let out = this
                    .guard
                    .as_mut()
                    .map_or_else(|| chunk.clone(), |g| g.feed(&chunk));
                std::task::Poll::Ready(Some(Ok(out)))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => {
                let rest = this.guard.as_mut().map(|g| g.flush()).unwrap_or_default();
                if rest.is_empty() {
                    std::task::Poll::Ready(None)
                } else {
                    // The guard held back an unterminated tail: emit it now,
                    // end the stream on the next poll.
                    this.guard = None;
                    std::task::Poll::Ready(Some(Ok(rest)))
                }
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_start_event(input_tokens: u64, cache_read: u64, cache_creation: u64) -> Vec<u8> {
        let payload = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "content": [],
                "usage": {
                    "input_tokens": input_tokens,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": cache_creation,
                    "output_tokens": 1,
                },
            },
        });
        format!("event: message_start\ndata: {}\n\n", payload).into_bytes()
    }

    fn message_delta_event(input_tokens: u64, cache_read: u64, cache_creation: u64) -> Vec<u8> {
        let payload = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": serde_json::Value::Null},
            "usage": {
                "input_tokens": input_tokens,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_creation,
                "output_tokens": 42,
            },
        });
        format!("event: message_delta\ndata: {}\n\n", payload).into_bytes()
    }

    fn start_usage(out: &[u8]) -> serde_json::Map<String, serde_json::Value> {
        for line in out.split(|b| *b == b'\n') {
            if line.starts_with(b"data:") {
                let payload: serde_json::Value =
                    serde_json::from_slice(trim_ascii(&line[5..])).unwrap();
                return payload["message"]["usage"].as_object().unwrap().clone();
            }
        }
        panic!("no data line in {out:?}");
    }

    #[test]
    fn has_context_1m_beta_detects_token() {
        assert!(has_context_1m_beta(Some("context-1m-2025-08-07")));
        assert!(has_context_1m_beta(Some("other, context-1m-2025-08-07")));
        assert!(has_context_1m_beta(Some("context-1m")));
        // Case-insensitive, plain prefix match — exactly like Python, warts
        // included: `context-1mx` counts upstream too.
        assert!(has_context_1m_beta(Some("Context-1M-2025-08-07")));
        assert!(has_context_1m_beta(Some("CONTEXT-1M")));
        assert!(has_context_1m_beta(Some("context-1mx")));
        assert!(!has_context_1m_beta(Some("other-beta")));
        assert!(!has_context_1m_beta(None));
        assert!(!has_context_1m_beta(Some("")));
    }

    #[test]
    fn believed_limit_raised_by_1m_beta() {
        assert_eq!(
            believed_context_limit(200_000, Some("context-1m-2025-08-07")),
            1_000_000
        );
        assert_eq!(believed_context_limit(200_000, None), 200_000);
    }

    #[test]
    fn believed_limit_never_lowered() {
        assert_eq!(
            believed_context_limit(2_000_000, Some("context-1m-2025-08-07")),
            2_000_000
        );
    }

    #[test]
    fn effective_limit_optimistic_until_learned() {
        assert_eq!(
            effective_context_limit("model-0", 200_000, None, ""),
            200_000
        );
    }

    #[test]
    fn learned_limit_caps_effective() {
        let learned = note_prompt_too_long(
            "model-1",
            Some("context-1m-2025-08-07"),
            b"prompt is too long: 900000 tokens > 200000 maximum",
            "",
        );
        assert_eq!(learned, Some(200_000));
        // Believed 1M, learned 200k: effective is the learned cap.
        assert_eq!(
            effective_context_limit("model-1", 200_000, Some("context-1m-2025-08-07"), ""),
            200_000
        );
    }

    #[test]
    fn learned_limit_keyed_by_beta_presence() {
        note_prompt_too_long(
            "model-2",
            None,
            b"prompt is too long: 5 tokens > 200000 maximum",
            "",
        );
        // The beta dimension differs: no cap inherited.
        assert_eq!(
            effective_context_limit("model-2", 200_000, Some("context-1m-2025-08-07"), ""),
            1_000_000
        );
    }

    #[test]
    fn learned_limit_keyed_by_credential_scope() {
        note_prompt_too_long(
            "model-3",
            None,
            b"prompt is too long: 5 tokens > 200000 maximum",
            "scope-a",
        );
        assert_eq!(
            effective_context_limit("model-3", 1_000_000, None, "scope-b"),
            1_000_000
        );
        assert_eq!(
            effective_context_limit("model-3", 1_000_000, None, "scope-a"),
            200_000
        );
    }

    #[test]
    fn note_prompt_too_long_ignores_other_errors() {
        assert_eq!(
            note_prompt_too_long("model-4", None, b"overloaded_error: try again", ""),
            None
        );
        assert_eq!(note_prompt_too_long("model-4", None, b"", ""), None);
        assert_eq!(
            note_prompt_too_long("model-4", None, b"\xff\xfe binary", ""),
            None
        );
    }

    #[test]
    fn note_prompt_too_long_rejects_out_of_band_maxima() {
        // Per-request quota shaped like the error: not a context window.
        assert_eq!(
            note_prompt_too_long(
                "model-5",
                None,
                b"prompt is too long: 9 tokens > 100 maximum",
                ""
            ),
            None
        );
        assert_eq!(
            effective_context_limit("model-5", 200_000, None, ""),
            200_000
        );
    }

    #[test]
    fn credential_scope_is_stable_and_non_reversible() {
        let a = credential_scope(Some("sk-ant-secret"));
        assert_eq!(a, credential_scope(Some("  sk-ant-secret  ")));
        assert_eq!(a.len(), 16);
        assert!(!a.contains("secret"));
        assert_ne!(a, credential_scope(Some("other-key")));
        assert!(credential_scope(None).is_empty());
        assert!(credential_scope(Some("   ")).is_empty());
    }

    #[test]
    fn credential_scope_from_headers_prefers_api_key_and_hashes_authorization_verbatim() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-api-key", "api-key-value".parse().unwrap());
        headers.insert("authorization", "Bearer oauth-token".parse().unwrap());
        assert_eq!(
            credential_scope_from_headers(&headers),
            credential_scope(Some("api-key-value"))
        );
        headers.remove("x-api-key");
        // The whole Authorization value is hashed, Bearer scheme included —
        // exactly like Python. The digest only needs stability, not secrecy
        // of the scheme word.
        assert_eq!(
            credential_scope_from_headers(&headers),
            credential_scope(Some("Bearer oauth-token"))
        );
        assert_ne!(
            credential_scope_from_headers(&headers),
            credential_scope(Some("oauth-token"))
        );
        headers.remove("authorization");
        assert!(credential_scope_from_headers(&headers).is_empty());
        // An empty x-api-key is not a credential: Python falls through to
        // `authorization` instead of landing in the unscoped bucket.
        headers.insert("x-api-key", "".parse().unwrap());
        headers.insert("authorization", "Bearer tok".parse().unwrap());
        assert_eq!(
            credential_scope_from_headers(&headers),
            credential_scope(Some("Bearer tok"))
        );
    }

    #[test]
    fn note_prompt_too_long_tolerates_undecodable_bodies() {
        // Lossy decode like Python's errors="replace": binary garbage
        // elsewhere must not hide a match in the decodable part.
        let mut body = b"\xff\xfe".to_vec();
        body.extend_from_slice(b"prompt is too long: 5 tokens > 200000 maximum");
        assert_eq!(
            note_prompt_too_long("model-7", None, &body, ""),
            Some(200_000)
        );
    }

    #[test]
    fn guard_passthrough_below_trigger() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let event = message_start_event(10_000, 0, 0);
        let out = guard.feed(&event);
        assert_eq!(out.as_ref(), event.as_slice());
        // Inert now: later bytes pass through untouched.
        let delta = message_delta_event(10_000, 0, 0);
        assert_eq!(guard.feed(&delta).as_ref(), delta.as_slice());
    }

    #[test]
    fn guard_nudges_start_and_final_delta() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let out = guard.feed(&message_start_event(185_000, 0, 0));
        let usage = start_usage(&out);
        // 95% of 200k.
        assert_eq!(usage["input_tokens"], 190_000);
        // Content deltas pass through; the final cumulative delta is nudged too.
        let content = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n";
        assert_eq!(guard.feed(content).as_ref(), content);
        let out = guard.feed(&message_delta_event(185_000, 0, 0));
        let payload: serde_json::Value = serde_json::from_slice(trim_ascii(
            out.split(|b| *b == b'\n')
                .find(|l| l.starts_with(b"data:"))
                .unwrap()[5..]
                .as_ref(),
        ))
        .unwrap();
        assert_eq!(payload["usage"]["input_tokens"], 190_000);
    }

    #[test]
    fn guard_includes_cache_tokens_in_trigger_total() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        // 100k input + 85k cache read = 185k ≥ 180k trigger.
        let out = guard.feed(&message_start_event(100_000, 85_000, 0));
        let usage = start_usage(&out);
        assert_eq!(usage["input_tokens"], 190_000 - 85_000);
    }

    #[test]
    fn guard_never_deflates() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let event = message_start_event(195_000, 0, 0);
        assert_eq!(guard.feed(&event).as_ref(), event.as_slice());
    }

    #[test]
    fn guard_delta_without_input_usage_left_alone() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let _ = guard.feed(&message_start_event(185_000, 0, 0));
        let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n";
        assert_eq!(guard.feed(delta).as_ref(), delta);
    }

    #[test]
    fn guard_first_event_not_start_disarms() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let err = b"event: error\ndata: {\"type\":\"error\"}\n\n";
        assert_eq!(guard.feed(err).as_ref(), err);
        // Disarmed: a later start passes through untouched.
        let start = message_start_event(185_000, 0, 0);
        assert_eq!(guard.feed(&start).as_ref(), start.as_slice());
    }

    #[test]
    fn guard_ignores_pings_and_comments() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let ping = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
        assert_eq!(guard.feed(ping).as_ref(), ping);
        let comment = b": keepalive\n\n";
        assert_eq!(guard.feed(comment).as_ref(), comment);
        // Still scanning: the start is nudged.
        let out = guard.feed(&message_start_event(185_000, 0, 0));
        assert_eq!(start_usage(&out)["input_tokens"], 190_000);
    }

    #[test]
    fn guard_handles_split_chunks_and_crlf() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let event = message_start_event(185_000, 0, 0);
        let (a, b) = event.split_at(event.len() / 2);
        assert!(guard.feed(a).is_empty());
        let out = guard.feed(b);
        assert_eq!(start_usage(&out)["input_tokens"], 190_000);

        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let crlf: Vec<u8> = message_start_event(185_000, 0, 0)
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        // feed in one chunk: CRLF framing must still terminate the event.
        let out = guard.feed(&crlf);
        assert_eq!(start_usage(&out)["input_tokens"], 190_000);
    }

    #[test]
    fn guard_gives_up_past_buffer_cap() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let big = vec![b'x'; MAX_GUARD_BUFFER + 1];
        // No terminator and over the cap: flushed untouched, guard inert.
        assert_eq!(guard.feed(&big).as_ref(), big.as_slice());
        let start = message_start_event(185_000, 0, 0);
        assert_eq!(guard.feed(&start).as_ref(), start.as_slice());
    }

    #[test]
    fn guard_payload_mentioning_delta_does_not_disarm() {
        let mut guard = StreamUsageGuard::new(200_000, 200_000, "r1");
        let _ = guard.feed(&message_start_event(185_000, 0, 0));
        // Assistant text containing the words "message_delta": no event: line
        // naming it, so the guard stays armed for the real delta.
        let text = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"message_delta\"}}\n\n";
        assert_eq!(guard.feed(text).as_ref(), text);
        let out = guard.feed(&message_delta_event(185_000, 0, 0));
        let payload: serde_json::Value = serde_json::from_slice(trim_ascii(
            out.split(|b| *b == b'\n')
                .find(|l| l.starts_with(b"data:"))
                .unwrap()[5..]
                .as_ref(),
        ))
        .unwrap();
        assert_eq!(payload["usage"]["input_tokens"], 190_000);
    }

    #[test]
    fn nudge_response_usage_rewrites_near_wall_body() {
        let body = serde_json::json!({
            "type": "message",
            "usage": {"input_tokens": 185_000, "output_tokens": 5},
        });
        let out = nudge_response_usage(&serde_json::to_vec(&body).unwrap(), 200_000, 200_000, "r1")
            .expect("nudged");
        let payload: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(payload["usage"]["input_tokens"], 190_000);
    }

    #[test]
    fn nudge_response_usage_leaves_calm_body_alone() {
        for body in [
            serde_json::json!({"type": "message", "usage": {"input_tokens": 10}}),
            serde_json::json!({"type": "message"}),
            serde_json::json!([1, 2]),
        ] {
            assert!(
                nudge_response_usage(&serde_json::to_vec(&body).unwrap(), 200_000, 200_000, "r1")
                    .is_none()
            );
        }
        assert!(nudge_response_usage(b"not json", 200_000, 200_000, "r1").is_none());
    }

    #[test]
    fn request_model_reads_model_field() {
        assert_eq!(
            request_model(br#"{"model":"claude-opus-4-5","stream":true}"#),
            Some("claude-opus-4-5".to_string())
        );
        assert_eq!(request_model(b"{}"), None);
        assert_eq!(request_model(b"nope"), None);
    }
}
