//! Rate-limit hold for the OpenCode Zen route.
//!
//! The fast retry loop in [`crate::routed::retry`] spends its ~3s budget on a
//! Zen 429 and would return the 429 to Claude Code — which treats it as fatal
//! and kills the turn (and its subagents). But a Zen 429 is exactly what
//! `zen-rotate-watch.sh` reacts to: it rotates the VPN exit (cooldown 120s,
//! drain up to 90s) so the *next* request lands on a fresh exit. The turn
//! just has to outlive the rotation.
//!
//! So instead of returning the 429, the proxy holds the request: sleep in
//! [`crate::proxy::backoff_ms`]-shaped slices capped by `retry_max_delay_ms`,
//! re-POST-ing the buffered body between sleeps, until the upstream answers
//! non-429. By default (`retry_zen_hold_budget_ms` = 0) there is no budget:
//! on 2026-09-14 the 150s budget ran out five times while Zen kept
//! answering 429 past one rotation, and each time the 429 reached the client
//! and killed the turn. A client that gives up drops this future, so an
//! unbounded hold cannot outlive the request it serves. A positive budget
//! restores the bounded behaviour.
//!
//! Duplication risk is nil: a 429 means the upstream refused the turn, so no
//! generation happened and re-sending is free. The hold sits inside
//! `send_with_retry`, before any byte is forwarded, so the SSE hold/commit
//! bookkeeping downstream is untouched.

use crate::proxy::AppState;
use axum::http::HeaderMap;
use bytes::Bytes;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Turns currently parked in the hold. Exposed on `/debug/inflight` as
/// `zen_held` so `zen-rotate-watch.sh` can subtract them from `in_flight`
/// before draining: a held turn has nothing generating and is itself waiting
/// for the rotation, so draining on it only stalls the rotation until the
/// drain's own deadline (4m40s on 2026-09-14, twice the old hold budget).
static HELD: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn held_count() -> usize {
    HELD.load(Ordering::SeqCst)
}

struct HeldGuard;

impl HeldGuard {
    fn enter() -> Self {
        HELD.fetch_add(1, Ordering::SeqCst);
        HeldGuard
    }
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the hold hands back to the retry loop: the last response (recovered
/// or still-429) plus the headers as last sent and the attempt count, so
/// metrics and outcome context stay honest across the two loops.
pub(crate) struct HeldSend {
    pub resp: reqwest::Response,
    pub headers: HeaderMap,
    pub attempts_made: u32,
}

/// Wait out a Zen 429 so the turn lands on the rotated exit.
///
/// Returns `Some` when the caller should `continue` its send loop with the
/// returned response/headers/attempts — either the upstream recovered (the
/// response is non-429) or the hold budget elapsed while still limited (the
/// still-429 response then flows through the normal error path, and the
/// `local_model_upstream_error` line it logs re-arms the watcher).
/// Returns `None` only when a positive budget elapsed while still limited.
/// Transport errors of any kind keep the hold going: they are what a VPN
/// restart looks like from here. The caller answers `None` with one honest
/// re-send rather than the stale 429. A long `Retry-After` is not a reason to stop — Zen sends a
/// constant one (see the still-limited arm below).
pub(crate) async fn hold_for_rotation(
    state: &AppState,
    upstream_url: &str,
    headers: HeaderMap,
    body: Bytes,
    attempts_so_far: u32,
    request_id: &str,
) -> Option<HeldSend> {
    // `0` is the default and means no budget: hold until Zen answers
    // something other than 429. A spent budget hands the 429 to the client,
    // which kills the turn and every subagent under it — the thing the hold
    // exists to prevent. A positive budget is kept for tests and operators
    // who want the old bounded behaviour.
    let budget_ms = match state.config.retry_zen_hold_budget_ms {
        0 => u64::MAX,
        ms => u64::from(ms),
    };
    let started = std::time::Instant::now();
    let _held = HeldGuard::enter();
    let mut attempts_made = attempts_so_far;
    loop {
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        if elapsed_ms >= budget_ms {
            tracing::warn!(
                event = "zen_hold_budget_spent",
                elapsed_ms,
                hold_budget_ms = budget_ms,
                attempts = attempts_made,
                request_id = %request_id,
                "Zen hold budget spent while still limited; returning the 429"
            );
            crate::observability::record_upstream_retry_exhausted(
                "local_model",
                crate::observability::retry_reason::ZEN_HOLD,
            );
            return None;
        }
        // Slice the wait so each probe re-checks the upstream: the same
        // capped backoff shape as the fast loop, never sleeping past the
        // budget so the last probe lands on the edge.
        let remaining = budget_ms.saturating_sub(elapsed_ms);
        let slice_ms =
            crate::proxy::backoff_ms(state, attempts_made).min(state.config.retry_max_delay_ms);
        let slice = std::time::Duration::from_millis(slice_ms.min(remaining).max(1));
        tracing::warn!(
            event = "zen_hold_waiting",
            hold_attempt = attempts_made + 1,
            sleep_ms = slice.as_millis() as u64,
            elapsed_ms,
            hold_budget_ms = budget_ms,
            request_id = %request_id,
            "Zen rate limit holding for VPN rotation"
        );
        crate::observability::record_upstream_retry(
            "local_model",
            crate::observability::retry_reason::ZEN_HOLD,
        );
        tokio::time::sleep(slice).await;
        attempts_made += 1;
        // The 429 refused the turn, so the buffered body replays freely —
        // this is a real re-send, not a cheap probe, so recovery lands the
        // turn immediately instead of needing another loop iteration.
        match state
            .client
            .post(upstream_url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) if r.status().as_u16() != 429 => {
                tracing::warn!(
                    event = "zen_hold_recovered",
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    attempts = attempts_made,
                    status = r.status().as_u16(),
                    request_id = %request_id,
                    "upstream recovered during the rotation hold"
                );
                return Some(HeldSend {
                    resp: r,
                    headers,
                    attempts_made,
                });
            }
            Ok(r) => {
                // Still limited. Zen's Retry-After does not survive contact
                // with the facts: on 2026-09-14 it sat at ~53568 (14.9h read
                // as seconds) for seven hours without counting down, while
                // the same route answered a probe fine minutes later. So on
                // this route the header is a constant, not a wait, and
                // honoring it only returns the fatal 429 early. Keep holding
                // and let the budget decide; log the value once per probe so
                // a real multi-hour window still shows up in the log.
                if let Some(wait) = r
                    .headers()
                    .get(http::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(headroom_core::retry::retry_after_ms_uncapped)
                {
                    if wait > state.config.retry_max_delay_ms as f64 {
                        tracing::warn!(
                            event = "zen_hold_retry_after_ignored",
                            retry_after_ms = wait,
                            probe_cap_ms = state.config.retry_max_delay_ms,
                            request_id = %request_id,
                            "upstream Retry-After outruns the rotation hold; ignoring it (Zen sends a constant) and holding on"
                        );
                    }
                }
                drop(r);
                continue;
            }
            Err(e) if crate::proxy::is_retryable_transport_error(&e) => {
                // Rotation RSTs the tunnel mid-hold: the transport error is
                // the rotation happening under us. Keep holding — the next
                // probe lands on the fresh exit.
                tracing::warn!(
                    event = "zen_hold_transport",
                    error = %e,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    request_id = %request_id,
                    "transport error during the hold (likely the rotation itself); holding on"
                );
                continue;
            }
            Err(e) => {
                // Anything else (decode, redirect, builder) is also seen
                // while the VPN restarts under us. Giving up here handed the
                // client the stale 429 and killed the turn; hold on instead
                // and let the next probe decide.
                tracing::warn!(
                    event = "zen_hold_fatal_transport",
                    error = %e,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    request_id = %request_id,
                    "non-retryable transport error during the hold; holding on"
                );
                continue;
            }
        }
    }
}
