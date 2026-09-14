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

/// True when the serialized outbound body carries evidence the prefix-replay
/// stage rewrote it: an Anthropic `messages` array plus at least one
/// `cache_control` marker. Without that evidence a 413 retry would just
/// re-send the identical body — the refusal is the turn's own size, and the
/// extra round trip buys nothing.
fn prepared_replay_applies(body: &Bytes) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    messages.iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
            || m.get("cache_control").is_some()
    })
}

/// Strip the replay prefix from an already-serialized outbound body: drop the
/// leading messages up to the last `cache_control` marker, keeping the tail
/// the provider has not cached yet. Returns the original body unchanged when
/// it carries no replay evidence, so a non-replay 413 never pays for a
/// pointless re-send.
fn strip_replay_prefix(body: &Bytes) -> Bytes {
    let Ok(mut parsed) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()).cloned() else {
        return body.clone();
    };
    let last_marker = messages.iter().rposition(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
            || m.get("cache_control").is_some()
    });
    let Some(idx) = last_marker else {
        return body.clone();
    };
    let tail: Vec<Value> = messages.into_iter().skip(idx + 1).collect();
    if tail.is_empty() {
        return body.clone();
    }
    parsed["messages"] = Value::Array(tail);
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
    // Bounds come from the same config the Claude path uses, so
    // `--retry-max-attempts` and the backoff window mean one thing across both
    // paths. The 401-refresh is codex-specific and sits outside the budget:
    // it is a credential fix, not a transient failure, and always gets its one
    // shot regardless of how retries are configured.
    let max_attempts = if state.config.retry_enabled {
        state.config.retry_max_attempts.max(1)
    } else {
        1
    };
    let max_delay_ms = state.config.retry_max_delay_ms;
    let mut refreshed = false;
    let mut attempt: u32 = 0;
    // Zen in-flight cap: bound the first wave (concurrent POST starts),
    // which no backoff can stagger because nothing has failed yet. The slot
    // releases when this function returns; the streamed body outlives it by
    // design (streams already accepted need no gate). Fail-open: over-cap
    // turns proceed without a slot rather than stalling.
    let _zen_slot = if is_zen {
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
    // Shared 429 gate: a recent 429 on this host parks it until now+backoff,
    // so turns arriving behind a known-limited upstream wait instead of
    // re-colliding. Capped at `retry_max_delay_ms`; the request is untouched.
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
                    if let Some(auth_file) = state.config.codex_auth_file.as_deref() {
                        if let Some(token) = refresh_codex_token(&state.client, auth_file).await {
                            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}"))
                            {
                                headers.insert(http::header::AUTHORIZATION, val);
                            }
                            refreshed = true;
                            continue;
                        }
                    }
                    break r;
                }
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < max_attempts {
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
                    if !zen_hold_owns_this
                        && retry_after_uncapped.is_some_and(|delay| delay > max_delay_ms as f64)
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
                        break r;
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
                    let backoff =
                        std::time::Duration::from_millis(headroom_core::retry::next_delay_ms(
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
                    continue;
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
                // once with the prefix stripped. The already-computed
                // compressed body is untouched — the shape on the wire stays
                // byte-identical except for the replayed prefix bytes — so
                // this turns a turn-kill into a cache miss, not a new turn.
                // Any error arm downstream still books the outcome once.
                if status.as_u16() == 413
                    && attempt >= max_attempts
                    && prepared_replay_applies(&body)
                {
                    drop(r);
                    let stripped = strip_replay_prefix(&body);
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
                            return Ok(UpstreamSend {
                                resp: r,
                                headers,
                                attempts: attempt + 1,
                                retried_without_replay: Some(stripped.len() as u64),
                            });
                        }
                        Err(e) => {
                            return Err(crate::error::transient_response(format!(
                                "local upstream error: {e}"
                            )));
                        }
                    }
                }
                if status.as_u16() == 429 && is_zen && state.config.retry_zen_hold_enabled {
                    drop(r);
                    match crate::routed::zen_hold::hold_for_rotation(
                        state,
                        upstream_url,
                        headers.clone(),
                        body.clone(),
                        attempt,
                        request_id,
                    )
                    .await
                    {
                        Some(held) => {
                            headers = held.headers;
                            attempt = held.attempts_made;
                            break held.resp;
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
                            Ok(r) => break r,
                            Err(e) => {
                                return Err(crate::error::transient_response(format!(
                                    "local upstream error: {e}"
                                )));
                            }
                        },
                    }
                }
                break r;
            }
            Err(e) => {
                // Same filter the Claude path uses: a decode or builder error
                // is not transient and gets no retry, only the transport-level
                // ones do. This arm used to retry every error alike, which
                // spent the whole budget re-sending a request that could not
                // succeed and delayed the 502 the caller was owed.
                let is_retryable = crate::proxy::is_retryable_transport_error(&e);
                if is_retryable && attempt < max_attempts {
                    // Was a hardcoded 250ms doubling with no ceiling, which
                    // ignored `retry_base_delay_ms` and could outrun
                    // `retry_max_delay_ms`. Same backoff as every other site now.
                    let backoff = std::time::Duration::from_millis(crate::proxy::backoff_ms(
                        state,
                        attempt - 1,
                    ));
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
                    continue;
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
                    return Err(crate::error::transient_response(format!(
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
                return Err(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("local upstream error: {e}")))
                    .expect("static response"));
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
        config.retry_max_attempts = 1;
        config.prefix_replay = true;
        let state = AppState::new(config).expect("app state");
        let pinned = Bytes::from(
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"cached","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":"fresh"}]}"#,
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
