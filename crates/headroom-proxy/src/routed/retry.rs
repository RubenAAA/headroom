//! Upstream send with retry for the routed paths.
//!
//! One OAuth-token refresh outside the retry budget (a credential fix, not a
//! transient failure), then backoff on 429/5xx/transport errors honoring
//! Retry-After — the same bounds the Claude path uses, so
//! `--retry-max-attempts` and the backoff window mean one thing on both.

use crate::codex::refresh_codex_token;
use crate::proxy::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

/// The transcript array on the translated wire shape: `messages` on Chat
/// Completions, `input` on Responses. Translation drops the Anthropic
/// `cache_control` markers (neither translator carries them), so replay
/// evidence on the wire is the shape of the array itself, not a marker.
fn wire_transcript(body: &Value) -> Option<&Vec<Value>> {
    body.get("messages")
        .or_else(|| body.get("input"))
        .and_then(|m| m.as_array())
}

/// True when the translated outbound body is big enough that a 413 retry
/// without the cached head is worth one round trip: at least two transcript
/// entries, so dropping the head leaves a non-empty tail. With zero or one
/// entries there is no prefix to strip — the refusal is the turn's own size,
/// and a re-send would just repeat it.
fn prepared_replay_applies(body: &Bytes) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    wire_transcript(&parsed).is_some_and(|t| t.len() > 1)
}

/// Strip the replay prefix from an already-serialized translated body: drop
/// the first transcript entry — the cached head the provider already holds —
/// and resend the tail. The remaining entries keep their order and content,
/// so the turn reads as a continuation, not a new turn. Returns the original
/// body unchanged when there is no head to drop, so a single-entry 413 never
/// pays for a pointless re-send.
fn strip_replay_prefix(body: &Bytes) -> Bytes {
    let Ok(mut parsed) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let transcript_len = wire_transcript(&parsed).map(Vec::len).unwrap_or(0);
    if transcript_len < 2 {
        return body.clone();
    }
    let key = if parsed.get("messages").is_some() {
        "messages"
    } else {
        "input"
    };
    let stripped_len = if let Some(items) = parsed.get_mut(key).and_then(|m| m.as_array_mut()) {
        items.remove(0);
        items.len()
    } else {
        return body.clone();
    };
    if stripped_len == 0 {
        return body.clone();
    }
    serde_json::to_vec(&parsed)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
}

/// A turn the upstream accepted (or refused with a status): the response, how
/// many attempts it took, and the headers as last sent — the 401 refresh may
/// have replaced the bearer token, and continuations must use the live one.
pub(crate) struct UpstreamSend {
    pub resp: reqwest::Response,
    pub headers: HeaderMap,
    pub attempts: u32,
    /// Set when a 413 was answered by re-sending the same turn with the
    /// replay prefix stripped. The handler refreshes the outcome context's
    /// `outbound_bytes` from this so the measured diagnosis reports the
    /// bytes actually refused, not the first attempt's.
    pub retried_without_replay: Option<u64>,
}

/// Send with retry. `Err` is the 502 to return when the transport itself
/// failed and the budget is spent (or the error was never retryable).
///
/// `is_zen` enables the rate-limit hold: a Zen (opencode.ai) 429 that
/// exhausts the fast budget waits on, bounded by
/// `retry_zen_hold_budget_ms`, instead of returning the fatal 429 — the VPN
/// watcher rotates the exit on the 429 log line, and the turn must outlive
/// the rotation to land on the fresh exit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_with_retry(
    state: &AppState,
    upstream_url: &str,
    mut headers: HeaderMap,
    body: Bytes,
    request_id: &str,
    session_key: Option<&str>,
    is_chatgpt_auth: bool,
    is_zen: bool,
) -> Result<UpstreamSend, Response> {
    let (max_attempts, max_delay_ms) = retry_bounds(state);
    let mut refreshed = false;
    let mut attempt: u32 = 0;
    // Zen in-flight cap: bound the first wave (concurrent POST starts),
    // which no backoff can stagger because nothing has failed yet. The slot
    // releases when this function returns or when a 429 hold begins; the
    // streamed body outlives it by design (streams already accepted need
    // no gate). Fail-open: over-cap turns proceed without a slot rather
    // than stalling.
    let mut zen_slot = if is_zen {
        Some(
            crate::routed::upstream_gate::acquire_global_zen_slot(
                state.config.retry_zen_max_inflight,
                max_delay_ms,
                request_id,
            )
            .await,
        )
    } else {
        None
    };
    wait_behind_parked_host(upstream_url, max_delay_ms, request_id).await;
    let upstream_resp = loop {
        attempt += 1;
        let result = state
            .client
            .post(upstream_url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;

        match result {
            Ok(r) => {
                let status = r.status();
                if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed && is_chatgpt_auth {
                    let Some(r) =
                        try_codex_token_refresh(state, &mut headers, &mut refreshed, r).await
                    else {
                        continue;
                    };
                    break r;
                }
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < max_attempts {
                    match backoff_retryable_status(
                        state,
                        r,
                        attempt,
                        max_attempts,
                        max_delay_ms,
                        upstream_url,
                        request_id,
                        session_key,
                        is_zen,
                    )
                    .await
                    {
                        StatusRetry::Waited => continue,
                        StatusRetry::Break(r) => break r,
                    }
                }
                // Zen rate-limit hold: the fast budget above spent itself
                // (the 3-attempt default is ~3s) on a Zen 429, and returning
                // it kills the turn before the watcher can rotate. Hand the
                // turn to the hold: it sleeps in capped backoff slices and
                // re-sends, and the loop breaks with the hold's answer —
                // recovered, or the last still-429 (whose error-arm log
                // line re-arms the watcher).
                // 413 replay-strip: the replay prefix pins the outbound body at
                // the session maximum, so a body the gateway refuses is retried
                // once with the cached head dropped. The tail keeps its order
                // and content, so the turn reads as a continuation that cache-
                // misses the head — not a new turn. Any error arm downstream
                // still books the outcome once.
                if status.as_u16() == 413 && prepared_replay_applies(&body) {
                    drop(r);
                    return try_replay_stripped_resend(
                        state,
                        upstream_url,
                        &headers,
                        &body,
                        attempt,
                        request_id,
                        session_key,
                    )
                    .await;
                }
                if status.as_u16() == 429 && is_zen && state.config.retry_zen_hold_enabled {
                    drop(r);
                    match try_zen_hold(
                        state,
                        upstream_url,
                        &mut headers,
                        &body,
                        &mut attempt,
                        &mut zen_slot,
                        request_id,
                    )
                    .await
                    {
                        Ok(r) => break r,
                        Err(resp) => return Err(resp),
                    }
                }
                break r;
            }
            Err(e) => {
                // Same filter the Claude path uses: a decode or builder error
                // is not transient and gets no retry, only the transport-level
                // ones do.
                match handle_transport_error(
                    state,
                    e,
                    attempt,
                    max_attempts,
                    upstream_url,
                    request_id,
                )
                .await
                {
                    TransportOutcome::Retry => continue,
                    TransportOutcome::Fail(resp) => return Err(resp),
                }
            }
        }
    };
    Ok(UpstreamSend {
        resp: upstream_resp,
        headers,
        attempts: attempt,
        retried_without_replay: None,
    })
}

/// Retry bounds from the same config the Claude path uses, so
/// `--retry-max-attempts` and the backoff window mean one thing across both
/// paths. The 401-refresh is codex-specific and sits outside the budget:
/// it is a credential fix, not a transient failure, and always gets its one
/// shot regardless of how retries are configured.
/// Extracted from `send_with_retry` without behavior change.
fn retry_bounds(state: &AppState) -> (u32, u64) {
    let max_attempts = if state.config.retry_enabled {
        state.config.retry_max_attempts.max(1)
    } else {
        1
    };
    (max_attempts, state.config.retry_max_delay_ms)
}

/// Shared 429 gate: a recent 429 on this host parks it until now+backoff,
/// so turns arriving behind a known-limited upstream wait instead of
/// re-colliding. Capped at `retry_max_delay_ms`; the request is untouched.
/// Extracted from `send_with_retry` without behavior change.
async fn wait_behind_parked_host(upstream_url: &str, max_delay_ms: u64, request_id: &str) {
    if let Some(host) = crate::routed::upstream_gate::upstream_host(upstream_url) {
        let wait_ms = crate::routed::upstream_gate::gate_wait_ms(&host).min(max_delay_ms);
        if wait_ms > 0 {
            tracing::warn!(
                event = "upstream_backoff_gate_wait",
                upstream = %host,
                wait_ms,
                request_id = %request_id,
                "upstream is parked by a recent 429; waiting behind it instead of re-colliding"
            );
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
    }
}

/// One-shot Codex OAuth refresh outside the retry budget. Returns `None`
/// when the token was refreshed (caller updates headers and re-sends), or
/// `Some(r)` to break the loop with the 401 when no refresh applied.
/// Extracted from `send_with_retry` without behavior change.
async fn try_codex_token_refresh(
    state: &AppState,
    headers: &mut HeaderMap,
    refreshed: &mut bool,
    r: reqwest::Response,
) -> Option<reqwest::Response> {
    if let Some(auth_file) = state.config.codex_auth_file.as_deref() {
        if let Some(token) = refresh_codex_token(&state.client, auth_file).await {
            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
                headers.insert(http::header::AUTHORIZATION, val);
            }
            *refreshed = true;
            return None;
        }
    }
    Some(r)
}

/// What the 429/5xx backoff arm decided: the turn slept and must re-send,
/// or the loop breaks with this response.
/// Extracted from `send_with_retry` without behavior change.
enum StatusRetry {
    Waited,
    Break(reqwest::Response),
}

/// Backoff on a retryable 429/5xx inside the attempt budget: honor
/// Retry-After (except the Zen-owned 429, which belongs to the hold), clamp
/// to the cap, park the host for parallel turns, sleep. Breaks with the
/// response when Retry-After exceeds the cap — unless the Zen hold owns it.
/// Extracted from `send_with_retry` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn backoff_retryable_status(
    state: &AppState,
    r: reqwest::Response,
    attempt: u32,
    max_attempts: u32,
    max_delay_ms: u64,
    upstream_url: &str,
    request_id: &str,
    session_key: Option<&str>,
    is_zen: bool,
) -> StatusRetry {
    let status = r.status();
    let retry_after_uncapped = r
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(headroom_core::retry::retry_after_ms_uncapped);
    // A Zen 429 belongs to the hold below, which re-probes
    // until `retry_zen_hold_budget_ms`. Breaking out here on
    // a long Retry-After skipped the hold entirely, and Zen's
    // header is a constant (~53568, read as 14.9h) that keeps
    // its value while the route serves other turns fine.
    let zen_hold_owns_this =
        is_zen && status.as_u16() == 429 && state.config.retry_zen_hold_enabled;
    if !zen_hold_owns_this && retry_after_uncapped.is_some_and(|delay| delay > max_delay_ms as f64)
    {
        tracing::warn!(
            event = "local_model_retry_after_exceeds_cap",
            status = status.as_u16(),
            attempt,
            max_attempts,
            retry_after_ms = retry_after_uncapped.unwrap_or_default(),
            retry_max_delay_ms = max_delay_ms,
            request_id = %request_id,
            session_key_hash = %session_key.map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
            "upstream Retry-After exceeds the internal wait cap; returning the response without an early retry"
        );
        return StatusRetry::Break(r);
    }
    // Same reason: a Zen Retry-After past the cap is not a
    // wait, so it must not stretch the fast loop's ~1s slices
    // into cap-length sleeps before the hold takes over.
    let retry_after = match retry_after_uncapped {
        Some(delay) if zen_hold_owns_this && delay > max_delay_ms as f64 => None,
        other => other,
    };
    // Shared selection (C5); the outer min to the cap below
    // stays here — this loop clamps, `forward_http` does not.
    let backoff = std::time::Duration::from_millis(headroom_core::retry::next_delay_ms(
        retry_after,
        crate::proxy::backoff_ms(state, attempt - 1),
    ))
    .min(std::time::Duration::from_millis(max_delay_ms));
    tracing::warn!(
        event = "local_model_upstream_retry",
        status = status.as_u16(),
        attempt,
        backoff_ms = backoff.as_millis() as u64,
        retry_after_header = r.headers().contains_key(http::header::RETRY_AFTER),
        delay_source = if retry_after.is_some() { "header" } else { "backoff" },
        retry_after_clamped = false,
        request_id = %request_id,
        session_key_hash = %session_key.map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
        "retrying transient upstream error"
    );
    crate::observability::record_upstream_retry(
        "local_model",
        crate::observability::retry_reason::from_status(status.as_u16()),
    );
    // Share the pain: park this host until now+backoff so
    // parallel turns queue behind the limit instead of each
    // sleeping privately and re-colliding on the same wake.
    if let Some(host) = crate::routed::upstream_gate::upstream_host(upstream_url) {
        crate::routed::upstream_gate::gate_hold(
            &host,
            backoff.as_millis().min(u64::MAX as u128) as u64,
        );
    }
    tokio::time::sleep(backoff).await;
    StatusRetry::Waited
}

/// 413 replay-strip: the replay prefix pins the outbound body at the session
/// maximum, so a body the gateway refuses is retried once with the cached
/// head dropped. Caller checked `prepared_replay_applies`; the tail keeps its
/// order and content, so the turn reads as a continuation that cache-misses
/// the head — not a new turn.
/// Extracted from `send_with_retry` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn try_replay_stripped_resend(
    state: &AppState,
    upstream_url: &str,
    headers: &HeaderMap,
    body: &Bytes,
    attempt: u32,
    request_id: &str,
    session_key: Option<&str>,
) -> Result<UpstreamSend, Response> {
    let stripped = strip_replay_prefix(body);
    match state
        .client
        .post(upstream_url)
        .headers(headers.clone())
        .body(stripped.clone())
        .send()
        .await
    {
        Ok(r) => {
            tracing::warn!(
                event = "local_model_413_replay_stripped",
                request_id = %request_id,
                session_key_hash = %session_key.map(crate::cache_stabilization::drift_detector::session_key_log_prefix).unwrap_or_default(),
                stripped_bytes = stripped.len() as u64,
                stripped_status = r.status().as_u16(),
                "upstream refused the replay-pinned body; retried once without the replay prefix"
            );
            Ok(UpstreamSend {
                resp: r,
                headers: headers.clone(),
                attempts: attempt + 1,
                retried_without_replay: Some(stripped.len() as u64),
            })
        }
        Err(e) => Err(crate::error::transient_response(format!(
            "local upstream error: {e}"
        ))),
    }
}

/// Zen rate-limit hold: the fast budget spent itself on a Zen 429, and
/// returning it kills the turn before the watcher can rotate. Hands the
/// turn to the hold and applies its answer to the caller's headers/attempt.
/// Returns the response to break the loop with, or the 502 to fail with.
/// Extracted from `send_with_retry` without behavior change.
#[allow(clippy::too_many_arguments)]
async fn try_zen_hold(
    state: &AppState,
    upstream_url: &str,
    headers: &mut HeaderMap,
    body: &Bytes,
    attempt: &mut u32,
    zen_slot: &mut Option<crate::routed::upstream_gate::ZenSlot>,
    request_id: &str,
) -> Result<reqwest::Response, Response> {
    // A hold sleeps for up to the hold budget (187 s seen
    // 2026-09-14) with nothing in flight. Holding the Zen
    // slot through it pinned every slot behind 429s, so the
    // 1,152 over-cap turns that day each waited the full
    // 30 s and then went without one anyway. Give it back;
    // the per-host gate already staggers arrivals behind
    // the same 429.
    zen_slot.take();
    match crate::routed::zen_hold::hold_for_rotation(
        state,
        upstream_url,
        headers.clone(),
        body.clone(),
        *attempt,
        request_id,
    )
    .await
    {
        Some(held) => {
            *headers = held.headers;
            *attempt = held.attempts_made;
            Ok(held.resp)
        }
        // The hold declined (non-retryable error inside):
        // the fast loop's last 429 is already dropped, so
        // re-send once for the honest answer rather than
        // inventing a status.
        None => match state
            .client
            .post(upstream_url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => Err(crate::error::transient_response(format!(
                "local upstream error: {e}"
            ))),
        },
    }
}

/// What the transport-error arm decided: sleep and re-send, or fail now.
/// Extracted from `send_with_retry` without behavior change.
enum TransportOutcome {
    Retry,
    Fail(Response),
}

/// Transport-error arm with the same filter the Claude path uses: a decode
/// or builder error is not transient and gets no retry, only the
/// transport-level ones do. Exhaustion answers 503 + Retry-After so the
/// client retries; a non-retryable error answers a bare 502.
/// Extracted from `send_with_retry` without behavior change.
async fn handle_transport_error(
    state: &AppState,
    e: reqwest::Error,
    attempt: u32,
    max_attempts: u32,
    upstream_url: &str,
    request_id: &str,
) -> TransportOutcome {
    // This arm used to retry every error alike, which
    // spent the whole budget re-sending a request that could not
    // succeed and delayed the 502 the caller was owed.
    let is_retryable = crate::proxy::is_retryable_transport_error(&e);
    if is_retryable && attempt < max_attempts {
        // Was a hardcoded 250ms doubling with no ceiling, which
        // ignored `retry_base_delay_ms` and could outrun
        // `retry_max_delay_ms`. Same backoff as every other site now.
        let backoff =
            std::time::Duration::from_millis(crate::proxy::backoff_ms(state, attempt - 1));
        tracing::warn!(
            event = "local_model_upstream_retry",
            error = %e,
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            delay_source = "transport_backoff",
            request_id = %request_id,
            "retrying failed upstream connection"
        );
        crate::observability::record_upstream_retry(
            "local_model",
            crate::observability::retry_reason::TRANSPORT,
        );
        tokio::time::sleep(backoff).await;
        return TransportOutcome::Retry;
    }
    if is_retryable {
        crate::observability::record_upstream_retry_exhausted(
            "local_model",
            crate::observability::retry_reason::TRANSPORT,
        );
        tracing::warn!(
            event = "local_model_upstream_error",
            error = %e,
            retryable = is_retryable,
            attempts = attempt,
            upstream = %upstream_url,
            "failed to connect to local model upstream"
        );
        // Transient (rotation RST, wifi flap, corpse-pool
        // first-write miss): 503 + Retry-After so the client
        // retries — the same contract `ProxyError::Upstream`
        // upholds in `crate::error`. A bare 502 here would stall
        // the session until a human nudges it.
        return TransportOutcome::Fail(crate::error::transient_response(format!(
            "local upstream error: {e}"
        )));
    }
    tracing::warn!(
        event = "local_model_upstream_error",
        error = %e,
        retryable = is_retryable,
        attempts = attempt,
        upstream = %upstream_url,
        "failed to connect to local model upstream"
    );
    TransportOutcome::Fail(
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from(format!("local upstream error: {e}")))
            .expect("static response"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transport exhaustion on the routed path must answer 503 + Retry-After
    /// (not a bare 502): rotation RSTs, wifi flaps, and corpse-pool misses
    /// are transient, and the client should retry instead of stalling.
    /// Single attempt so the test never sleeps in backoff.
    #[tokio::test]
    async fn transport_exhaustion_answers_503_with_retry_after() {
        let upstream: url::Url = "http://127.0.0.1:1/unreachable".parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        let state = AppState::new(config).expect("app state");
        let err = match send_with_retry(
            &state,
            "http://127.0.0.1:1/unreachable",
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-exhaustion",
            None,
            false,
            false,
        )
        .await
        {
            Ok(_) => panic!("closed port must fail"),
            Err(resp) => resp,
        };
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            err.headers().get(http::header::RETRY_AFTER).unwrap(),
            "2",
            "client must be told to retry shortly"
        );
    }

    /// Zen hold recovers a flapping 429 at the `send_with_retry` level
    /// (`is_zen` forced — the classifier keys on the opencode.ai host,
    /// unreachable from a unit test). One fast attempt, 1ms slices, so the
    /// test holds milliseconds, not seconds.
    #[tokio::test]
    async fn zen_hold_recovers_flapping_429() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let n = hits_clone.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    wiremock::ResponseTemplate::new(429).set_body_string("rate limited")
                } else {
                    wiremock::ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
                }
            })
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        config.retry_base_delay_ms = 1;
        config.retry_max_delay_ms = 1;
        config.retry_zen_hold_enabled = true;
        config.retry_zen_hold_budget_ms = 30_000;
        let state = AppState::new(config).expect("app state");
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-zen-hold",
            None,
            false,
            true,
        )
        .await
        .expect("hold must recover into Ok");
        assert_eq!(send.resp.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "one 429 then recovery");
    }

    /// The default budget (0) holds through a long run of 429s instead of
    /// giving up: 40 refusals at 1ms slices would have outrun any small
    /// positive budget, and the client sees only the eventual 200.
    #[tokio::test]
    async fn zen_hold_default_budget_never_gives_up() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let n = hits_clone.fetch_add(1, Ordering::SeqCst);
                if n < 40 {
                    wiremock::ResponseTemplate::new(429)
                        .insert_header("retry-after", "12123")
                        .set_body_string("rate limited")
                } else {
                    wiremock::ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
                }
            })
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        config.retry_base_delay_ms = 1;
        config.retry_max_delay_ms = 1;
        config.retry_zen_hold_enabled = true;
        config.retry_zen_hold_budget_ms = 0;
        let state = AppState::new(config).expect("app state");
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-zen-hold-unbounded",
            None,
            false,
            true,
        )
        .await
        .expect("hold must recover into Ok");
        assert_eq!(send.resp.status(), 200, "the 429 never reaches the caller");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            41,
            "forty refusals then recovery"
        );
    }

    /// A Zen 429 carrying a Retry-After far past the cap still reaches the
    /// hold. Zen sends ~53568 (14.9h read as seconds) as a constant while the
    /// route keeps serving, so the header must not short-circuit the hold —
    /// that is what killed subagent turns on 2026-09-14.
    #[tokio::test]
    async fn zen_hold_ignores_retry_after_past_the_cap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let n = hits_clone.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    wiremock::ResponseTemplate::new(429)
                        .insert_header("retry-after", "53568")
                        .set_body_string("rate limited")
                } else {
                    wiremock::ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
                }
            })
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        config.retry_base_delay_ms = 1;
        config.retry_max_delay_ms = 1;
        config.retry_zen_hold_enabled = true;
        config.retry_zen_hold_budget_ms = 30_000;
        let state = AppState::new(config).expect("app state");
        let started = std::time::Instant::now();
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-zen-long-retry-after",
            None,
            false,
            true,
        )
        .await
        .expect("hold must recover into Ok");
        assert_eq!(send.resp.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 3, "two 429s then recovery");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the header must not stretch the waits either"
        );
    }

    /// A Zen upstream that never recovers must still terminate: the hold
    /// gives up once `retry_zen_hold_budget_ms` elapses and returns the
    /// still-429 response rather than looping forever or firing an
    /// unbounded number of re-POSTs. This is the volume guard for the
    /// worst case — a rotation that never lands a fresh exit within budget.
    #[tokio::test]
    async fn zen_hold_gives_up_after_budget_when_never_recovering() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                hits_clone.fetch_add(1, Ordering::SeqCst);
                wiremock::ResponseTemplate::new(429).set_body_string("rate limited")
            })
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        config.retry_base_delay_ms = 1;
        config.retry_max_delay_ms = 1;
        config.retry_zen_hold_enabled = true;
        config.retry_zen_hold_budget_ms = 50;
        let state = AppState::new(config).expect("app state");
        let started = std::time::Instant::now();
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-zen-hold-exhaust",
            None,
            false,
            true,
        )
        .await
        .expect("still-429 flows through as an Ok response, not an Err");
        assert_eq!(send.resp.status(), 429, "budget spent, still limited");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "must give up near the budget, not hang: took {elapsed:?}"
        );
        let hit_count = hits.load(Ordering::SeqCst);
        assert!(
            (1..100).contains(&hit_count),
            "bounded by the budget and the 1ms-floor slice, not a tight loop: {hit_count} hits"
        );
    }

    /// Non-zen upstreams keep honoring a long Retry-After: no hold owns them,
    /// so the response goes back without burning the retry budget.
    #[tokio::test]
    async fn non_zen_upstream_honors_long_retry_after() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(429)
                    .insert_header("retry-after", "53568")
                    .set_body_string("rate limited"),
            )
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 3;
        config.retry_base_delay_ms = 1;
        config.retry_max_delay_ms = 1;
        config.retry_zen_hold_enabled = true;
        config.retry_zen_hold_budget_ms = 30_000;
        let state = AppState::new(config).expect("app state");
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-non-zen-long-retry-after",
            None,
            false,
            false,
        )
        .await
        .expect("429 comes back as Ok");
        assert_eq!(send.resp.status(), 429);
        assert_eq!(send.attempts, 1, "no early retry against a long window");
    }

    /// Non-zen upstreams never enter the hold: the honest 429 passes
    /// through even with the hold enabled and budget to spare.
    #[tokio::test]
    async fn non_zen_upstream_skips_hold() {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        config.retry_zen_hold_enabled = true;
        config.retry_zen_hold_budget_ms = 30_000;
        let state = AppState::new(config).expect("app state");
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from("{}"),
            "test-no-hold",
            None,
            false,
            false,
        )
        .await
        .expect("passthrough is still Ok");
        assert_eq!(send.resp.status(), 429);
    }

    /// A 413 on a replay-pinned body retries once with the cached prefix
    /// stripped: the mock answers 413 to the pinned body and 200 to the
    /// smaller one. The reported bytes are the retried body's, and the gate
    /// fires exactly once even with budget to spare.
    #[tokio::test]
    async fn replay_strip_retries_413_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |req: &wiremock::Request| {
                let n = hits_clone.fetch_add(1, Ordering::SeqCst);
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let pinned = body
                    .get("messages")
                    .or_else(|| body.get("input"))
                    .and_then(|m| m.as_array())
                    .is_some_and(|a| a.len() > 1);
                if n == 0 {
                    assert!(pinned, "first attempt carries the replay prefix");
                    wiremock::ResponseTemplate::new(413).set_body_string("too large")
                } else {
                    assert!(!pinned, "retry arrives without the replay prefix");
                    wiremock::ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
                }
            })
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.prefix_replay = true;
        assert_eq!(
            config.retry_max_attempts, 3,
            "regression must exercise the default retry budget"
        );
        let state = AppState::new(config).expect("app state");
        // Translated wire shape: no Anthropic `cache_control` markers survive
        // translation, so the pinned body is a two-entry transcript and the
        // retry must arrive as its one-entry tail.
        let pinned = Bytes::from(
            r#"{"model":"m","input":[{"type":"message","role":"user","content":"cached head"},{"type":"message","role":"user","content":"fresh tail"}]}"#,
        );
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            pinned.clone(),
            "test-413-strip",
            None,
            false,
            false,
        )
        .await
        .expect("strip retry must recover into Ok");
        assert_eq!(send.resp.status(), 200);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "one 413 then the stripped retry"
        );
        let stripped = send
            .retried_without_replay
            .expect("retry reports the stripped byte count");
        assert!(
            stripped < pinned.len() as u64,
            "stripped body is smaller than the refused one"
        );
    }

    /// A 413 with no replay evidence is not retried: re-sending the identical
    /// body can only be refused again, so the single attempt flows through to
    /// the measured error arm.
    #[tokio::test]
    async fn plain_413_without_replay_is_not_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                hits_clone.fetch_add(1, Ordering::SeqCst);
                wiremock::ResponseTemplate::new(413).set_body_string("too large")
            })
            .mount(&mock)
            .await;
        let upstream: url::Url = mock.uri().parse().unwrap();
        let mut config = crate::config::Config::for_test(upstream);
        config.retry_enabled = true;
        config.retry_max_attempts = 1;
        config.prefix_replay = true;
        let state = AppState::new(config).expect("app state");
        let send = send_with_retry(
            &state,
            &mock.uri(),
            HeaderMap::new(),
            Bytes::from(r#"{"model":"m","messages":[{"role":"user","content":"fresh"}]}"#),
            "test-413-plain",
            None,
            false,
            false,
        )
        .await
        .expect("plain 413 still returns Ok with the refusal");
        assert_eq!(send.resp.status(), 413);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no pointless re-send");
        assert_eq!(send.retried_without_replay, None);
    }
}
