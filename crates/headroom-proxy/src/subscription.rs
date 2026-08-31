//! Proxy-side wiring for the Anthropic subscription-window tracker.
//!
//! The pure tracker state machine, models, persistence, and OAuth token
//! resolution live in `headroom-core` (`headroom_core::subscription`). This
//! module supplies the two pieces that need heavy deps the core crate avoids:
//!
//! * [`HttpSubscriptionFetcher`] — the reqwest-backed `GET /api/oauth/usage`
//!   call (port of `SubscriptionClient.fetch`).
//! * [`spawn_poll_loop`] — the tokio background poll loop (port of
//!   `SubscriptionTracker._poll_loop`), driving the core's synchronous
//!   `poll_once` off the async runtime via `spawn_blocking`.

use std::sync::Arc;
use std::time::Duration;

use headroom_core::subscription::client::{
    read_cached_oauth_token, SubscriptionFetcher, BETA_HEADER, USAGE_URL,
};
use headroom_core::subscription::models::SubscriptionSnapshot;
use headroom_core::subscription::tracker::SubscriptionTracker;

/// reqwest-backed implementation of the usage-API fetch.
pub struct HttpSubscriptionFetcher {
    client: reqwest::blocking::Client,
}

impl HttpSubscriptionFetcher {
    pub fn new(timeout: Duration) -> Self {
        let client = crate::ssl_context::blocking_client_builder()
            .timeout(timeout)
            .build()
            .expect("failed to build subscription HTTP client");
        Self { client }
    }
}

impl Default for HttpSubscriptionFetcher {
    fn default() -> Self {
        Self::new(Duration::from_secs(10))
    }
}

impl SubscriptionFetcher for HttpSubscriptionFetcher {
    fn fetch(&self, token: Option<&str>) -> Option<SubscriptionSnapshot> {
        let resolved = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .or_else(read_cached_oauth_token)?;

        let resp = self
            .client
            .get(USAGE_URL)
            .header("Authorization", format!("Bearer {resolved}"))
            .header("anthropic-beta", BETA_HEADER)
            .header("Content-Type", "application/json")
            .send()
            .ok()?;

        let status = resp.status().as_u16();
        // 401 (token rejected) / 404 (API-key account) → no data, not an error.
        if status == 401 || status == 404 {
            return None;
        }
        if status != 200 {
            tracing::warn!(status, "Anthropic usage API returned non-200");
            return None;
        }

        let data: serde_json::Value = resp.json().ok()?;
        Some(SubscriptionSnapshot::from_api_response(&data, &resolved))
    }
}

/// Spawn the background poll loop. Polls at the tracker's interval; each poll
/// runs on a blocking thread so the transcript scan + HTTP call never stall the
/// async runtime. The task ends when `shutdown` is cancelled/dropped.
pub fn spawn_poll_loop(
    tracker: Arc<SubscriptionTracker>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let interval = Duration::from_secs(tracker.poll_interval_s());
    tokio::spawn(async move {
        loop {
            let t = tracker.clone();
            if let Err(err) = tokio::task::spawn_blocking(move || t.poll_once()).await {
                tracing::warn!(%err, "subscription tracker poll task join error");
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        tracker.persist_state();
    })
}
