//! CTX-7: re-cache watchdog — response-side `usage` observer.
//!
//! The drift detector (PR-E6) sees a cache bust *coming* (request
//! bytes changed); this module sees it *happen* (the billed `usage`
//! numbers on the response). Together they answer both "did we lose
//! usage?" and "why?".
//!
//! # Detection rule
//!
//! For consecutive turns of the same conversation, a healthy prompt
//! cache satisfies:
//!
//! ```text
//!   cache_read(turn N) ≈ cache_read(turn N-1) + cache_creation(turn N-1)
//! ```
//!
//! (the prefix cached last turn — previous reads plus the newly
//! written tail — is read back in full this turn). When
//! `cache_read` instead *drops* below that expectation while
//! `cache_creation` spikes, Anthropic re-wrote a prefix it should
//! have read: a **re-cache event**, i.e. real billed tokens wasted.
//!
//! False-positive suppression: Anthropic's prompt cache TTL is
//! 5 minutes. A gap longer than that between turns legitimately
//! expires the cache and the full re-write is expected — classified
//! [`TurnClass::TtlExpiry`], logged at DEBUG, never warned or
//! counted as a re-cache.
//!
//! # Correlation flow
//!
//! - Request side (`proxy.rs` compression gate): [`UsageObserver::begin_request`]
//!   records `(request_id → conversation key, drift dims)` where the
//!   drift dims come from the PR-E6 detector — so a re-cache event
//!   can say *which* axis (system / tools / early_messages) drifted.
//! - Response side (`run_sse_state_machine`, Anthropic arm): on a
//!   cleanly completed stream, [`UsageObserver::complete`] looks up
//!   the pending entry, classifies against the conversation's
//!   previous turn, and emits log + metrics + snapshot state.
//!
//! Conversations are keyed by [`conversation_key`] — hash of
//! (session key, `system`, first message) — NOT by the auth-derived
//! session key alone, because one client (e.g. Claude Code plus its
//! subagents) runs many conversations concurrently and comparing
//! usage across different conversations would be pure noise.
//!
//! Everything here is a pure observer: no request or response byte
//! is ever mutated, and all bookkeeping happens off the client byte
//! path (the SSE state-machine task).

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lru::LruCache;
use serde::Serialize;

/// Anthropic prompt-cache TTL. A gap between turns longer than this
/// makes a full cache re-write legitimate (TtlExpiry, not a bug).
/// The default ephemeral TTL is 5 minutes; the optional 1h tier
/// would only make us *more* conservative, never produce a false
/// warning, so we key off the short tier.
pub const ANTHROPIC_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Token slack for the healthy-turn comparison. `cache_read` can
/// legitimately undershoot the expectation by a few tokens
/// (breakpoint rounding); anything inside the slack is Healthy.
pub const RECACHE_SLACK_TOKENS: u64 = 64;

/// Bounded capacities. Same rationale as the drift detector's LRU:
/// a flood of unique keys must not grow memory unboundedly.
const PENDING_CAPACITY: usize = 512;
const CONVERSATION_CAPACITY: usize = 512;
/// Rolling window for the fleet-wide hit-rate shown in the
/// statusline (`/cache-health`).
const RECENT_SAMPLE_CAPACITY: usize = 50;

/// Stable per-conversation key — SHA-256 over the auth-derived
/// session key plus `system` and the first message.
///
/// CTX-2: the single implementation now lives in
/// [`crate::ctx::identity::conversation_key`]; this thin re-export keeps the
/// CTX-7 call sites stable so the watchdog and the passive-capture classifier
/// can never diverge on how a conversation is keyed.
pub use crate::ctx::identity::conversation_key;

/// The usage counters of one completed turn, as billed by Anthropic.
#[derive(Debug, Clone, Copy)]
pub struct TurnRecord {
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub at: SystemTime,
}

/// Classification of one turn against its predecessor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnClass {
    /// No previous turn recorded for this conversation.
    FirstTurn,
    /// `cache_read` covers the previous prefix (within slack).
    Healthy,
    /// Prefix re-written, but the inter-turn gap exceeded the cache
    /// TTL — expected behaviour, not a defect.
    TtlExpiry,
    /// Prefix re-written inside the TTL window: billed tokens were
    /// wasted re-caching content that should have been a cache read.
    Recache {
        /// Previous-prefix tokens that were re-written instead of
        /// read: `min(expected_read - actual_read, cache_creation)`.
        wasted_tokens: u64,
    },
}

/// Pure classification of the current turn's usage against the
/// previous turn. Unit-testable without any observer state.
pub fn classify_turn(
    prev: &TurnRecord,
    now: SystemTime,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> TurnClass {
    let expected_read = prev
        .cache_read_input_tokens
        .saturating_add(prev.cache_creation_input_tokens);
    if cache_read_input_tokens.saturating_add(RECACHE_SLACK_TOKENS) >= expected_read {
        return TurnClass::Healthy;
    }
    let shortfall = expected_read - cache_read_input_tokens;
    if cache_creation_input_tokens <= RECACHE_SLACK_TOKENS {
        // The read dropped but nothing significant was re-written —
        // e.g. a much shorter branched conversation reusing the same
        // conversation key, or a degenerate retry. Nothing was
        // billed for re-caching, so there is nothing to warn about.
        return TurnClass::Healthy;
    }
    let gap = now
        .duration_since(prev.at)
        .unwrap_or(Duration::ZERO);
    if gap > ANTHROPIC_CACHE_TTL {
        return TurnClass::TtlExpiry;
    }
    TurnClass::Recache {
        wasted_tokens: shortfall.min(cache_creation_input_tokens),
    }
}

/// Request-side context parked until the response's usage arrives.
#[derive(Debug, Clone)]
struct PendingRequest {
    conversation_key: String,
    /// PR-E6 drift dimensions observed on this request, when any —
    /// the "why" attached to a re-cache event.
    drift_dims: Option<String>,
}

/// One re-cache event, kept for `/cache-health` (most recent only)
/// and the WARN log.
#[derive(Debug, Clone, Serialize)]
pub struct RecacheEvent {
    /// Unix seconds — snapshot consumers compute the age themselves.
    pub at_unix: u64,
    pub conversation_key: String,
    /// PR-E6 drift axes ("system" / "tools" / "early_messages",
    /// comma-joined) when the drift detector saw the cause; `None`
    /// means the bytes looked stable to the detector (likely a
    /// message edit / branch below the early-message window).
    pub drift_dims: Option<String>,
    pub wasted_tokens: u64,
    pub expected_cache_read: u64,
    pub actual_cache_read: u64,
}

/// JSON body served by `GET /cache-health`. Designed to be cheap to
/// render (statusline polls it every few seconds): everything comes
/// from one in-memory snapshot, no I/O on the read path.
#[derive(Debug, Clone, Serialize)]
pub struct CacheHealthSnapshot {
    /// Mean cache-hit rate over the last [`RECENT_SAMPLE_CAPACITY`]
    /// completed Anthropic sessions; `null` until the first sample.
    pub recent_hit_rate: Option<f64>,
    pub samples: usize,
    pub recache_events_total: u64,
    pub recache_wasted_tokens_total: u64,
    pub ttl_expiries_total: u64,
    pub last_event: Option<RecacheEvent>,
    /// Convenience for statusline scripts: seconds since
    /// `last_event`, `null` when no event has occurred.
    pub last_event_age_seconds: Option<u64>,
}

struct Inner {
    pending: LruCache<String, PendingRequest>,
    conversations: LruCache<String, TurnRecord>,
    recent_hit_rates: VecDeque<f64>,
    recache_events_total: u64,
    recache_wasted_tokens_total: u64,
    ttl_expiries_total: u64,
    last_event: Option<RecacheEvent>,
}

/// Shared observer, one per proxy process (lives on `AppState`).
pub struct UsageObserver {
    inner: Mutex<Inner>,
}

impl Default for UsageObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageObserver {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                pending: LruCache::new(
                    NonZeroUsize::new(PENDING_CAPACITY).expect("capacity is non-zero"),
                ),
                conversations: LruCache::new(
                    NonZeroUsize::new(CONVERSATION_CAPACITY).expect("capacity is non-zero"),
                ),
                recent_hit_rates: VecDeque::with_capacity(RECENT_SAMPLE_CAPACITY),
                recache_events_total: 0,
                recache_wasted_tokens_total: 0,
                ttl_expiries_total: 0,
                last_event: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!(
                    event = "usage_observer_mutex_poisoned",
                    "usage observer mutex was poisoned by a panicking task; recovering"
                );
                poisoned.into_inner()
            }
        }
    }

    /// Request side: park the conversation key + drift dims under the
    /// request id so [`complete`](Self::complete) can correlate.
    pub fn begin_request(
        &self,
        request_id: &str,
        conversation_key: String,
        drift_dims: Option<String>,
    ) {
        self.lock().pending.put(
            request_id.to_string(),
            PendingRequest {
                conversation_key,
                drift_dims,
            },
        );
    }

    /// Response side: classify this turn's billed usage against the
    /// conversation's previous turn. Call ONLY for cleanly completed
    /// streams (`message_stop`) — half-finished usage would classify
    /// garbage.
    pub fn complete(
        &self,
        request_id: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) {
        let now = SystemTime::now();
        let mut inner = self.lock();

        // Fleet-wide rolling hit rate (statusline ambient signal).
        let denom = input_tokens
            .saturating_add(cache_read_input_tokens)
            .saturating_add(cache_creation_input_tokens);
        if denom > 0 {
            if inner.recent_hit_rates.len() == RECENT_SAMPLE_CAPACITY {
                inner.recent_hit_rates.pop_front();
            }
            inner
                .recent_hit_rates
                .push_back(cache_read_input_tokens as f64 / denom as f64);
        }

        let Some(pending) = inner.pending.pop(request_id) else {
            // Request never went through the compression gate
            // (compression off, non-JSON, …) — no conversation
            // identity, so no per-turn classification. The rolling
            // rate above still counted it.
            return;
        };

        let class = match inner.conversations.get(&pending.conversation_key) {
            None => TurnClass::FirstTurn,
            Some(prev) => classify_turn(
                prev,
                now,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            ),
        };
        let expected_cache_read = inner
            .conversations
            .get(&pending.conversation_key)
            .map(|p| {
                p.cache_read_input_tokens
                    .saturating_add(p.cache_creation_input_tokens)
            })
            .unwrap_or(0);
        inner.conversations.put(
            pending.conversation_key.clone(),
            TurnRecord {
                cache_read_input_tokens,
                cache_creation_input_tokens,
                at: now,
            },
        );

        match class {
            TurnClass::FirstTurn | TurnClass::Healthy => {}
            TurnClass::TtlExpiry => {
                inner.ttl_expiries_total += 1;
                tracing::debug!(
                    event = "cache_recache_ttl_expiry",
                    request_id = %request_id,
                    conversation_key = %pending.conversation_key,
                    cache_creation_input_tokens = cache_creation_input_tokens,
                    "prefix re-written after cache TTL expiry (idle > 5 min); expected, not a defect"
                );
            }
            TurnClass::Recache { wasted_tokens } => {
                inner.recache_events_total += 1;
                inner.recache_wasted_tokens_total += wasted_tokens;
                let event = RecacheEvent {
                    at_unix: now
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or(Duration::ZERO)
                        .as_secs(),
                    conversation_key: pending.conversation_key.clone(),
                    drift_dims: pending.drift_dims.clone(),
                    wasted_tokens,
                    expected_cache_read,
                    actual_cache_read: cache_read_input_tokens,
                };
                tracing::warn!(
                    event = "cache_recache_observed",
                    request_id = %request_id,
                    conversation_key = %event.conversation_key,
                    drift_dims = event.drift_dims.as_deref().unwrap_or(""),
                    wasted_tokens = wasted_tokens,
                    expected_cache_read = expected_cache_read,
                    actual_cache_read = cache_read_input_tokens,
                    cache_creation_input_tokens = cache_creation_input_tokens,
                    "prompt cache re-written inside the TTL window: billed tokens wasted re-caching"
                );
                crate::observability::observe_recache_event(
                    event.drift_dims.as_deref(),
                    wasted_tokens,
                );
                inner.last_event = Some(event);
            }
        }
    }

    /// One cheap in-memory snapshot for `GET /cache-health`.
    pub fn snapshot(&self) -> CacheHealthSnapshot {
        let inner = self.lock();
        let recent_hit_rate = if inner.recent_hit_rates.is_empty() {
            None
        } else {
            Some(inner.recent_hit_rates.iter().sum::<f64>() / inner.recent_hit_rates.len() as f64)
        };
        let last_event_age_seconds = inner.last_event.as_ref().map(|e| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs()
                .saturating_sub(e.at_unix)
        });
        CacheHealthSnapshot {
            recent_hit_rate,
            samples: inner.recent_hit_rates.len(),
            recache_events_total: inner.recache_events_total,
            recache_wasted_tokens_total: inner.recache_wasted_tokens_total,
            ttl_expiries_total: inner.ttl_expiries_total,
            last_event: inner.last_event.clone(),
            last_event_age_seconds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prev(read: u64, creation: u64, age: Duration) -> TurnRecord {
        TurnRecord {
            cache_read_input_tokens: read,
            cache_creation_input_tokens: creation,
            at: SystemTime::now() - age,
        }
    }

    #[test]
    fn healthy_turn_reads_previous_prefix() {
        // prev cached 10_000 + wrote 2_000 → expect 12_000 read.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 12_000, 500);
        assert_eq!(c, TurnClass::Healthy);
    }

    #[test]
    fn healthy_within_slack() {
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 12_000 - RECACHE_SLACK_TOKENS, 500);
        assert_eq!(c, TurnClass::Healthy);
    }

    #[test]
    fn recache_inside_ttl_is_flagged_with_wasted_tokens() {
        // Expected read 12_000, got 0, re-wrote 12_500 → 12_000 wasted.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 0, 12_500);
        assert_eq!(
            c,
            TurnClass::Recache {
                wasted_tokens: 12_000
            }
        );
    }

    #[test]
    fn wasted_tokens_capped_at_cache_creation() {
        // Shortfall 12_000 but only 3_000 re-written (partial prefix
        // reuse via an earlier breakpoint) → waste is the re-write.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 0, 3_000);
        assert_eq!(
            c,
            TurnClass::Recache {
                wasted_tokens: 3_000
            }
        );
    }

    #[test]
    fn ttl_expiry_suppressed() {
        let p = prev(10_000, 2_000, ANTHROPIC_CACHE_TTL + Duration::from_secs(10));
        let c = classify_turn(&p, SystemTime::now(), 0, 12_500);
        assert_eq!(c, TurnClass::TtlExpiry);
    }

    #[test]
    fn read_drop_without_rewrite_is_healthy() {
        // Branched/shorter conversation: read dropped but nothing
        // significant was re-billed → nothing to warn about.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 4_000, 10);
        assert_eq!(c, TurnClass::Healthy);
    }

    #[test]
    fn observer_end_to_end_flags_recache_and_snapshots() {
        let obs = UsageObserver::new();
        // Turn 1.
        obs.begin_request("req-1", "conv-a".into(), None);
        obs.complete("req-1", 300, 0, 10_000);
        // Turn 2: healthy.
        obs.begin_request("req-2", "conv-a".into(), None);
        obs.complete("req-2", 200, 10_000, 800);
        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 0);
        // Turn 3: recache, drift detector blamed tools.
        obs.begin_request("req-3", "conv-a".into(), Some("tools".into()));
        obs.complete("req-3", 200, 0, 11_000);
        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 1);
        let ev = snap.last_event.expect("recache event recorded");
        assert_eq!(ev.drift_dims.as_deref(), Some("tools"));
        assert_eq!(ev.expected_cache_read, 10_800);
        assert_eq!(ev.wasted_tokens, 10_800);
        assert_eq!(snap.recache_wasted_tokens_total, 10_800);
        assert!(snap.recent_hit_rate.is_some());
        assert_eq!(snap.samples, 3);
    }

    #[test]
    fn concurrent_conversations_do_not_cross_talk() {
        // Two conversations from the same client (main session +
        // subagent) interleave; neither must flag the other.
        let obs = UsageObserver::new();
        obs.begin_request("req-a1", "conv-a".into(), None);
        obs.complete("req-a1", 300, 0, 50_000);
        obs.begin_request("req-b1", "conv-b".into(), None);
        obs.complete("req-b1", 300, 0, 2_000);
        obs.begin_request("req-a2", "conv-a".into(), None);
        obs.complete("req-a2", 200, 50_000, 900);
        obs.begin_request("req-b2", "conv-b".into(), None);
        obs.complete("req-b2", 200, 2_000, 400);
        assert_eq!(obs.snapshot().recache_events_total, 0);
    }

    #[test]
    fn unknown_request_only_updates_rolling_rate() {
        let obs = UsageObserver::new();
        obs.complete("never-began", 100, 900, 0);
        let snap = obs.snapshot();
        assert_eq!(snap.samples, 1);
        assert_eq!(snap.recache_events_total, 0);
    }

    #[test]
    fn conversation_key_stable_and_discriminating() {
        let body_a = serde_json::json!({
            "system": "you are helpful",
            "messages": [{"role": "user", "content": "first"}, {"role": "user", "content": "second"}]
        });
        let mut body_a2 = body_a.clone();
        body_a2["messages"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"role": "user", "content": "third"}));
        // Same conversation grown by a turn → same key.
        assert_eq!(
            conversation_key(&body_a, "sess"),
            conversation_key(&body_a2, "sess")
        );
        // Different first message → different key.
        let body_b = serde_json::json!({
            "system": "you are helpful",
            "messages": [{"role": "user", "content": "other convo"}]
        });
        assert_ne!(
            conversation_key(&body_a, "sess"),
            conversation_key(&body_b, "sess")
        );
        // Different client → different key.
        assert_ne!(
            conversation_key(&body_a, "sess"),
            conversation_key(&body_a, "sess2")
        );
    }
}
