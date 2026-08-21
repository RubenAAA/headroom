//! Re-issue an upstream request whose body drops before the client is committed.
//!
//! The status-code and in-band-error retries in `proxy.rs` both rely on the same
//! guarantee: nothing has reached the client yet, so a second attempt cannot
//! duplicate output. A body that dies *mid-stream* breaks that guarantee, and
//! the logs say it dies late enough to matter — across 18 drops the parser had a
//! content block open every time, with 1 to 20 output tokens already through.
//! Re-sending blind would splice two different generations together.
//!
//! So this holds the opening bytes back instead of forwarding them. While the
//! held buffer is under `hold_bytes` the response is uncommitted: a transport
//! error there discards the buffer and starts a new request, and the client sees
//! one clean stream. Once the buffer fills it is flushed, the response is
//! committed, and any later error propagates exactly as it did before.
//!
//! The cost is that the first `hold_bytes` of every stream arrive in one burst
//! rather than token by token, so `hold_bytes` buys robustness with time to
//! first paint. The observed drops sit inside 2 KiB.
//!
//! This wraps the body closest to the upstream, below both the CCR rewriter and
//! the telemetry tee, so neither ever sees the discarded attempt.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// Everything needed to issue the request again.
pub(crate) struct RetryContext {
    pub client: reqwest::Client,
    pub method: reqwest::Method,
    pub url: String,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    pub request_id: String,
    /// Total attempts, including the one already in flight. `1` disables retry.
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Bytes held back before the response counts as committed.
    pub hold_bytes: usize,
}

/// Depth of the hand-off queue to the client. Matches `ccr_stream`.
const CLIENT_QUEUE_DEPTH: usize = 64;

/// Whether a body that stopped part-way is worth asking for again.
///
/// The shared `is_retryable_transport_error` covers a request that never got
/// off the ground. A body that dies later fails a different way: reqwest calls
/// it `Decode` — "error decoding response body" — because the chunked encoding
/// ended where it should not have. That is the error every observed drop
/// carried, and it is the reason this predicate is not the shared one.
fn is_retryable_drop(e: &reqwest::Error) -> bool {
    e.is_decode() || crate::proxy::is_retryable_transport_error(e)
}

/// Wrap `first` so an early transport drop re-issues the request.
///
/// `first` is the body of the response already in flight, with any peeked
/// prefix chained back on.
pub(crate) fn retry_on_early_drop<S>(
    first: S,
    ctx: RetryContext,
) -> impl Stream<Item = reqwest::Result<Bytes>>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<reqwest::Result<Bytes>>(CLIENT_QUEUE_DEPTH);

    tokio::spawn(async move {
        let mut stream: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
            Box::pin(first);
        let mut attempt: u32 = 0;

        loop {
            // Bytes read but not yet forwarded. Empty once the response commits.
            let mut held: Vec<Bytes> = Vec::new();
            let mut held_len: usize = 0;
            let mut committed = false;
            // Set when the body dropped early and a fresh attempt is due.
            let mut drop_err: Option<reqwest::Error> = None;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(b) => {
                        if committed {
                            if tx.send(Ok(b)).await.is_err() {
                                return; // client hung up
                            }
                            continue;
                        }
                        held_len += b.len();
                        held.push(b);
                        if held_len >= ctx.hold_bytes {
                            for h in held.drain(..) {
                                if tx.send(Ok(h)).await.is_err() {
                                    return;
                                }
                            }
                            committed = true;
                        }
                    }
                    Err(e) => {
                        if !committed && attempt + 1 < ctx.max_attempts && is_retryable_drop(&e) {
                            drop_err = Some(e);
                            break;
                        }
                        // Committed, or out of budget: the client keeps whatever
                        // it was owed, then the error, as before.
                        for h in held.drain(..) {
                            if tx.send(Ok(h)).await.is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }

            let Some(e) = drop_err else {
                // Stream ended. A response shorter than `hold_bytes` still owes
                // the client every byte of it.
                for h in held.drain(..) {
                    if tx.send(Ok(h)).await.is_err() {
                        return;
                    }
                }
                return;
            };

            // `held` is dropped here: those bytes belong to the dead attempt and
            // the replacement response brings its own preamble.
            //
            // Asking again can itself fail to get off the ground — a burst of
            // parallel turns had every one of these come back "error sending
            // request" — and one failed send used to end the turn with the
            // budget untouched. So the ask repeats until a body is in hand or
            // the attempts run out.
            let replacement = loop {
                attempt += 1;
                let delay_ms = ctx
                    .base_delay_ms
                    .saturating_mul(1u64 << (attempt - 1))
                    .min(ctx.max_delay_ms);
                tracing::warn!(
                    request_id = %ctx.request_id,
                    error = %e,
                    cause = ?e,
                    attempt = attempt,
                    max_attempts = ctx.max_attempts,
                    delay_ms = delay_ms,
                    held_bytes = held_len,
                    "upstream stream dropped before the client was committed; retrying"
                );
                crate::observability::record_upstream_retry(
                    "anthropic",
                    crate::observability::retry_reason::TRANSPORT,
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

                let resp = ctx
                    .client
                    .request(ctx.method.clone(), &ctx.url)
                    .headers(ctx.headers.clone())
                    .body(ctx.body.clone())
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => break Some(r),
                    // A status the upstream chose is an answer, not a stumble.
                    // Asking again would only spend the budget to hear it twice.
                    Ok(r) => {
                        tracing::warn!(
                            request_id = %ctx.request_id,
                            status = r.status().as_u16(),
                            "retry of a dropped stream came back non-success; giving up"
                        );
                        break None;
                    }
                    Err(e2) => {
                        // `Display` on a reqwest error stops at "error sending
                        // request"; the cause — DNS, connect, TLS — only shows
                        // in `Debug`, and the cause is the whole question when
                        // a fresh request cannot get off the ground.
                        tracing::warn!(
                            request_id = %ctx.request_id,
                            error = %e2,
                            cause = ?e2,
                            attempt = attempt,
                            "retry of a dropped stream failed to send"
                        );
                        if attempt < ctx.max_attempts && is_retryable_drop(&e2) {
                            continue;
                        }
                        break None;
                    }
                }
            };
            let Some(r) = replacement else {
                // Nothing better to hand back than the drop that started this,
                // so the client sees the failure it would have seen.
                crate::observability::record_upstream_retry_exhausted(
                    "anthropic",
                    crate::observability::retry_reason::TRANSPORT,
                );
                let _ = tx.send(Err(e)).await;
                return;
            };
            stream = Box::pin(r.bytes_stream());
        }
    });

    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}
