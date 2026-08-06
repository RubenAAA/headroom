//! Timeout-debt quarantine — **deliberately not implemented in Rust.**
//!
//! This module exists to record a decision, not to provide behaviour. If you
//! came here because `headroom_compression_quarantine_total` is always zero, or
//! because you are auditing Python→Rust parity, read on: the gap is intentional
//! and re-deriving it from scratch is the thing this file prevents.
//!
//! # What Python does
//!
//! `headroom/proxy/server.py` tracks compression workers that exceeded their
//! timeout. When one or more are still running, the next compression request is
//! refused outright with `CompressionQuarantinedError` and
//! `record_compression_quarantine("skipped")`; entering that state emits
//! `record_compression_quarantine("activated")`.
//!
//! # Why it exists there
//!
//! Python cannot preempt a thread-pool worker. When `asyncio.wait_for` times
//! out, only the *awaiter* unblocks — the executor thread keeps running the
//! compression to completion, holding a pool slot. Python's own comment is
//! explicit that this is a language limitation. Under sustained load each
//! timeout permanently consumes a worker until the whole pool is full of
//! timed-out jobs and no compression can be scheduled at all. The quarantine is
//! a circuit breaker for that failure mode: refuse fast rather than queue behind
//! threads that will never come back.
//!
//! # Why it does not apply here
//!
//! Rust's compression runs on `tokio::task::spawn_blocking` under
//! `tokio::time::timeout` (see `websocket_codex::compress_frame_bounded`). A
//! timed-out task also keeps running to completion — that part is the same — but
//! the consequence is not. The blocking pool is not a fixed small set of slots
//! that a straggler starves; tokio grows it up to `max_blocking_threads` and new
//! work is scheduled onto other threads. There is no GIL, so a running straggler
//! does not contend for an interpreter lock with everything else either.
//!
//! So the specific failure the quarantine defends against — new compression work
//! becoming unschedulable because previous timeouts hold every slot — does not
//! arise from the same cause. Porting it would import a workaround for a
//! constraint this runtime does not have, and would add a refusal path that can
//! reject requests Rust is perfectly able to serve.
//!
//! # What Rust does instead
//!
//! Per-request bounding rather than global circuit breaking:
//!
//! - `tokio::time::timeout` bounds each attempt, and
//!   [`crate::compression_failure::decide_compression_failure_action`] decides
//!   fail-open vs fail-closed per request, recording the reason on
//!   `headroom_compression_failed_total`.
//! - The Kompress size gate
//!   ([`headroom_core::transforms::content_router::kompress_size_gate_exceeded`])
//!   keeps the largest blocks off the ML path entirely, which is the main way a
//!   compression call reached the timeout in the first place.
//!
//! # When to revisit
//!
//! This reasoning is about *why* timeouts pile up, not about whether they can.
//! Reopen the decision if any of these hold:
//!
//! - `max_blocking_threads` is set low enough that stragglers can realistically
//!   exhaust it, or blocking-pool saturation shows up in production.
//! - Compression moves off `spawn_blocking` onto a fixed-size pool.
//! - `headroom_compression_failed_total{reason="timeout"}` climbs steadily,
//!   which would mean stragglers accumulate faster than they drain.
//!
//! The metric family stays registered so a future implementation has a name to
//! emit on, and so the scrape shape matches Python's.

/// Whether a timeout-debt quarantine is active.
///
/// Always `false` — see the module docs. Provided so a call site can express
/// "check the quarantine" without an implementation existing, and so anything
/// that grows a real dependency on it fails loudly at the type level rather than
/// silently skipping a check that was never there.
pub fn is_quarantined() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the documented decision. If someone implements the quarantine, this
    /// test should fail and force them to read the module docs first.
    #[test]
    fn quarantine_is_never_active() {
        assert!(
            !is_quarantined(),
            "the quarantine is deliberately unimplemented — see the module docs \
             before changing this"
        );
    }

    /// The Python-parity metric stays registered even though nothing emits it,
    /// so the scrape shape matches and a future implementation has a name ready.
    #[test]
    fn the_parity_metric_is_still_exposed() {
        use prometheus::{Encoder, TextEncoder};
        let registry = crate::observability::prometheus::registry();
        crate::observability::proxy_counters::force_register_all(registry);
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&registry.gather(), &mut buf)
            .expect("registry encodes");
        let text = String::from_utf8(buf).expect("utf-8");
        assert!(text.contains("# TYPE headroom_compression_quarantine_total counter"));
    }
}
