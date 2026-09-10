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

/// A turn the upstream accepted (or refused with a status): the response, how
/// many attempts it took, and the headers as last sent — the 401 refresh may
/// have replaced the bearer token, and continuations must use the live one.
pub(crate) struct UpstreamSend {
    pub resp: reqwest::Response,
    pub headers: HeaderMap,
    pub attempts: u32,
}

/// Send with retry. `Err` is the 502 to return when the transport itself
/// failed and the budget is spent (or the error was never retryable).
pub(crate) async fn send_with_retry(
    state: &AppState,
    upstream_url: &str,
    mut headers: HeaderMap,
    body: Bytes,
    request_id: &str,
    session_key: Option<&str>,
    is_chatgpt_auth: bool,
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
                    if retry_after_uncapped.is_some_and(|delay| delay > max_delay_ms as f64) {
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
                    let retry_after = retry_after_uncapped;
                    let backoff = retry_after
                        .map(|delay| {
                            std::time::Duration::from_millis(
                                delay.ceil().min(u64::MAX as f64) as u64
                            )
                        })
                        .unwrap_or_else(|| {
                            std::time::Duration::from_millis(crate::proxy::backoff_ms(
                                state,
                                attempt - 1,
                            ))
                        })
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
                    tokio::time::sleep(backoff).await;
                    continue;
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
    })
}
