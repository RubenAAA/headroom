//! In-flight and streaming request counters, their RAII guards, and the
//! per-conversation concurrency shed.
//!
//! Moved out of `proxy.rs` without behavior change. `use super::*` keeps
//! the parent's items and imports in reach; the parent re-exports this
//! module, so callers keep their paths.

use super::*;

/// Requests inside `forward_http` right now, for the `inflight` field of
/// `stage_timings`.
///
/// Memory p50 measured 1.7 s with two or fewer requests in flight and 5.6 s
/// with six or seven (2026-09-03). Without the count on the line a slow query
/// and a busy proxy read the same.
pub(super) static INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// SSE bodies actively streaming to the client, after the `Response` was
/// dispatched and the pipeline `InflightGuard` already dropped. Held by
/// `TrackedStream` below, so `GET /debug/inflight` sees streaming turns and
/// the rotation drain defers instead of RSTing them.
pub(super) static STREAMING: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Holds one slot in `STREAMING` for the life of an SSE body. Created when a
/// streaming `Response` is built, dropped when the body ends or is dropped
/// (client gone, rotation RST, upstream error).
pub(crate) struct StreamingGuard;

impl StreamingGuard {
    pub(crate) fn enter() -> Self {
        STREAMING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for StreamingGuard {
    fn drop(&mut self) {
        STREAMING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pin_project_lite::pin_project! {
    /// A stream that keeps one `STREAMING` slot alive until it ends or is
    /// dropped. Wrap every SSE body before `Body::from_stream` so the drain
    /// check covers bytes that flow after the pipeline guard drops.
    pub(crate) struct TrackedStream<S> {
        _guard: StreamingGuard,
        #[pin]
        inner: S,
    }
}

impl<S> TrackedStream<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            _guard: StreamingGuard::enter(),
            inner,
        }
    }
}

impl<S: futures_util::Stream> futures_util::Stream for TrackedStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

/// Wrap an SSE stream so `GET /debug/inflight` counts it until the last byte.
pub(crate) fn track_streaming<S>(inner: S) -> TrackedStream<S> {
    TrackedStream::new(inner)
}

/// Holds one slot in `INFLIGHT` from `forward_http` entry to any exit,
/// including `?` returns.
pub(crate) struct InflightGuard;

impl InflightGuard {
    pub(crate) fn enter() -> Self {
        INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }

    /// Requests in flight, this one included.
    pub(super) fn count(&self) -> usize {
        INFLIGHT.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Process-wide in-flight requests, for the rotation drain check
    /// (`GET /debug/inflight`). Covers the pipeline guards (`forward_http`
    /// and routed `handle_messages`, held until the response is dispatched)
    /// plus `STREAMING` bodies actively flowing to the client. A streaming
    /// turn therefore reads nonzero from headers until the last byte.
    pub(crate) fn count_global() -> usize {
        INFLIGHT.load(std::sync::atomic::Ordering::Relaxed)
            + STREAMING.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Seconds a shed turn is asked to wait before retrying. One commit window:
/// the sibling turn whose overlap tripped the cap is seconds from done, and
/// the client's own 429 backoff stacks on top of this.
pub(super) const CONCURRENCY_SHED_RETRY_AFTER_SECS: u64 = 1;

/// 429 for a turn shed by `--max-conversation-concurrency`, shared by the
/// passthrough and routed paths (both serve Anthropic-shaped clients).
///
/// Status 429 so the client's standard rate-limit retry fires — clients
/// retry on the status, and the forward paths do the same upstream. The body
/// type mirrors Anthropic's own rate-limit shape for that compatibility,
/// while the message and the `x-headroom-shed` header say this is the proxy
/// pacing one conversation's fan-out, not the provider throttling the
/// account: the two must not be confused on a dashboard.
pub(crate) fn conversation_concurrency_shed_response(
    in_flight: usize,
    cap: usize,
) -> Response<Body> {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "rate_limit_error",
            "message": format!(
                "headroom: conversation concurrency cap exceeded ({in_flight} in flight, cap {cap}); retrying shortly lands against a committed prefix"
            ),
        },
    });
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(
            http::header::RETRY_AFTER,
            CONCURRENCY_SHED_RETRY_AFTER_SECS.to_string(),
        )
        .header("content-type", "application/json")
        .header("x-headroom-shed", "conversation-concurrency")
        .body(Body::from(body.to_string()))
        .expect("static shed response")
}
