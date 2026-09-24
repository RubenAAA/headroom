//! Throttled network-path probe for unusually slow upstream response starts.
//!
//! A request's upstream wait includes both network setup and provider work.
//! When that wait is slow, send a bodyless `HEAD /` through the same reqwest
//! client and proxy configuration. A quick probe alongside a slow model turn
//! points toward provider/model latency; a slow or failed probe points toward
//! the network or proxy path. The probe never carries the user's request body
//! or API authorization header.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const SLOW_REQUEST_DELAY: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_COOLDOWN: Duration = Duration::from_secs(30);

static LAST_PROBE_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Cancels the delayed probe when its upstream request becomes ready first.
pub(crate) struct SlowUpstreamProbe {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SlowUpstreamProbe {
    /// Arm a low-rate probe for a trusted upstream request.
    pub(crate) fn arm(
        client: reqwest::Client,
        upstream_url: &url::Url,
        request_id: &str,
        configured_http_proxy: bool,
    ) -> Option<Self> {
        let probe_url = root_probe_url(upstream_url)?;
        let request_id = request_id.to_owned();
        let (cancel, canceled) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(SLOW_REQUEST_DELAY) => {}
                _ = canceled => return,
            }

            if !claim_probe_slot() {
                return;
            }

            let started = Instant::now();
            match client.head(probe_url).timeout(PROBE_TIMEOUT).send().await {
                Ok(response) => {
                    tracing::info!(
                        target: "headroom.proxy",
                        event = "slow_upstream_path_probe",
                        request_id = %request_id,
                        configured_http_proxy,
                        probe_ms = started.elapsed().as_secs_f64() * 1000.0,
                        probe_status = response.status().as_u16(),
                        probe_peer = ?response.remote_addr(),
                        probe_http_version = ?response.version(),
                        "bodyless upstream path probe received a response"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "headroom.proxy",
                        event = "slow_upstream_path_probe",
                        request_id = %request_id,
                        configured_http_proxy,
                        probe_ms = started.elapsed().as_secs_f64() * 1000.0,
                        probe_error = error_kind(&error),
                        "bodyless upstream path probe failed"
                    );
                }
            }
        });

        Some(Self {
            cancel: Some(cancel),
        })
    }
}

impl Drop for SlowUpstreamProbe {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

/// Keep the diagnostic armed until the first upstream body bytes arrive.
/// This covers both time-to-headers and a delay before the first stream event.
pub(crate) fn cancel_on_first_chunk(
    upstream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
    >,
    probe: Option<SlowUpstreamProbe>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>> {
    use futures_util::StreamExt;

    Box::pin(futures_util::stream::unfold(
        (upstream, probe),
        |(mut upstream, mut probe)| async move {
            match upstream.next().await {
                Some(chunk) => {
                    drop(probe.take());
                    Some((chunk, (upstream, probe)))
                }
                None => None,
            }
        },
    ))
}

fn root_probe_url(upstream_url: &url::Url) -> Option<url::Url> {
    let mut probe_url = upstream_url.clone();
    if !matches!(probe_url.scheme(), "http" | "https") || probe_url.host_str().is_none() {
        return None;
    }
    // Do not forward URL credentials or request-specific query parameters.
    let _ = probe_url.set_username("");
    let _ = probe_url.set_password(None);
    probe_url.set_path("/");
    probe_url.set_query(None);
    probe_url.set_fragment(None);
    Some(probe_url)
}

fn claim_probe_slot() -> bool {
    let now = Instant::now();
    let mut last = LAST_PROBE_AT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.is_some_and(|previous| now.saturating_duration_since(previous) < PROBE_COOLDOWN) {
        return false;
    }
    *last = Some(now);
    true
}

fn error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else {
        "other"
    }
}
