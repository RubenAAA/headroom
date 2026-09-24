//! Shared per-upstream-egress rate-limit gate + Zen in-flight cap for routed sends.
//!
//! `send_with_retry` retries each turn on its own: when Zen answers 429,
//! every parallel turn sleeps its private backoff and re-collides —
//! 2026-09-14 saw 40 parallel Zen 429s inside one hour, plus a 7-wide
//! subagent burst that truncated every turn in the same millisecond. The
//! gate shares one hold per (upstream host, egress): the first 429 parks that
//! egress until now+backoff, and turns arriving behind it wait instead of
//! firing into a known-limited egress. Separate egresses have separate holds
//! and in-flight counters. The in-flight cap bounds the *first*
//! wave (concurrent POST starts), which no backoff can stagger because
//! nothing has failed yet.
//!
//! Both mechanisms are fail-open and bounded: gate waits cap at
//! `retry_max_delay_ms`, cap waits cap there too and then proceed without a
//! slot, holds expire on their own. Request bytes are untouched, so
//! prefix/cache keys never move. State is process-global like
//! `turn_hooks::HOOKS` — the limit being shared is the entire point — with
//! the core logic parameterized for parallel-safe unit tests.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, LazyLock, Mutex,
};
use std::time::{Duration, Instant};

/// How long a capped-out turn sleeps between cap rechecks.
const CAP_POLL_MS: u64 = 100;

static HOLDS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static ZEN_INFLIGHT: LazyLock<Mutex<HashMap<String, Arc<AtomicUsize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Instant>> {
    HOLDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lowercased `host` (no port) of an upstream URL, for gate keys.
pub(crate) fn upstream_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// Milliseconds left on `holds[host]`, pruning as it goes. Pure against the
/// passed map so tests need no globals.
fn remaining_ms(holds: &mut HashMap<String, Instant>, host: &str) -> u64 {
    match holds.get(host) {
        Some(until) => {
            let now = Instant::now();
            if *until > now {
                until.duration_since(now).as_millis().min(u64::MAX as u128) as u64
            } else {
                holds.remove(host);
                0
            }
        }
        None => 0,
    }
}

/// Extend `holds[host]` to `now + delay_ms`, never shortening an existing
/// hold: concurrent 429s each push the shared deadline forward.
fn hold_for(holds: &mut HashMap<String, Instant>, host: &str, delay_ms: u64) {
    let until = Instant::now() + Duration::from_millis(delay_ms);
    holds
        .entry(host.to_string())
        .and_modify(|e| {
            if until > *e {
                *e = until;
            }
        })
        .or_insert(until);
}

/// How long this turn should wait before sending to an upstream-egress key
/// (0 = go).
pub(crate) fn gate_wait_ms(egress_key: &str) -> u64 {
    remaining_ms(&mut lock(), egress_key)
}

/// Park an upstream-egress key for `delay_ms` after a 429/5xx (extends, never shortens).
pub(crate) fn gate_hold(egress_key: &str, delay_ms: u64) {
    hold_for(&mut lock(), egress_key, delay_ms);
}

/// Test/reset helper: drops every hold.
#[allow(dead_code)]
pub(crate) fn clear_gate() {
    lock().clear();
}

/// RAII slot for the Zen in-flight cap. `Drop` releases the counter the slot
/// was acquired against (carried as an `Arc`, so the release always lands on
/// the right counter — a slot from an injected test counter must not touch
/// the global one). A slot that proceeded over the cap or with the cap
/// disabled carries no counter and releases nothing.
pub(crate) struct ZenSlot {
    counter: Option<Arc<AtomicUsize>>,
}

impl ZenSlot {
    #[allow(dead_code)]
    fn is_held(&self) -> bool {
        self.counter.is_some()
    }
}

impl Drop for ZenSlot {
    fn drop(&mut self) {
        if let Some(counter) = &self.counter {
            counter.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Wait for a Zen send slot, fail-open.
///
/// Returns a held slot when one frees within `wait_cap_ms`; otherwise logs
/// and returns an unheld slot so the turn proceeds rather than stalling.
/// `max == 0` disables the cap. `counter` is injectable so tests run
/// parallel-safe against a local counter; production passes `&ZEN_INFLIGHT`.
pub(crate) async fn acquire_zen_slot(
    counter: &Arc<AtomicUsize>,
    max: usize,
    wait_cap_ms: u64,
    request_id: &str,
) -> ZenSlot {
    if max == 0 {
        return ZenSlot { counter: None };
    }
    let mut waited: u64 = 0;
    loop {
        let cur = counter.load(Ordering::SeqCst);
        if cur < max {
            if counter
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return ZenSlot {
                    counter: Some(Arc::clone(counter)),
                };
            }
            continue;
        }
        if waited >= wait_cap_ms {
            tracing::warn!(
                event = "zen_concurrency_cap_exceeded",
                request_id = %request_id,
                inflight = cur,
                max,
                waited_ms = waited,
                "Zen send slots full; proceeding without one rather than stalling the turn"
            );
            return ZenSlot { counter: None };
        }
        tokio::time::sleep(Duration::from_millis(CAP_POLL_MS)).await;
        waited += CAP_POLL_MS;
    }
}

/// Production entry point: one shared in-flight counter per upstream-egress key.
pub(crate) async fn acquire_global_zen_slot(
    egress_key: &str,
    max: usize,
    wait_cap_ms: u64,
    request_id: &str,
) -> ZenSlot {
    let counter = {
        let mut counters = ZEN_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            counters
                .entry(egress_key.to_string())
                .or_insert_with(|| Arc::new(AtomicUsize::new(0))),
        )
    };
    acquire_zen_slot(&counter, max, wait_cap_ms, request_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing_lowercases_and_strips_port() {
        assert_eq!(
            upstream_host("https://opencode.ai/zen/v1/responses"),
            Some("opencode.ai".to_string())
        );
        assert_eq!(
            upstream_host("https://OpenCode.AI:443/x"),
            Some("opencode.ai".to_string())
        );
        assert_eq!(upstream_host("not a url"), None);
    }

    #[test]
    fn hold_then_wait_then_expiry() {
        let mut holds = HashMap::new();
        assert_eq!(remaining_ms(&mut holds, "h"), 0);
        hold_for(&mut holds, "h", 60_000);
        let left = remaining_ms(&mut holds, "h");
        assert!(left > 55_000 && left <= 60_000, "got {left}");
        // A shorter hold never pulls the deadline back in.
        hold_for(&mut holds, "h", 1_000);
        assert!(remaining_ms(&mut holds, "h") > 55_000);
    }

    #[test]
    fn expired_hold_prunes_and_reads_zero() {
        let mut holds = HashMap::new();
        holds.insert("h".to_string(), Instant::now() - Duration::from_secs(1));
        assert_eq!(remaining_ms(&mut holds, "h"), 0);
        assert!(!holds.contains_key("h"));
    }
    #[tokio::test]
    async fn slot_immediately_when_free() {
        let counter = Arc::new(AtomicUsize::new(0));
        let slot = acquire_zen_slot(&counter, 4, 1_000, "t").await;
        assert!(slot.is_held());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(slot);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn slot_fail_open_without_holding() {
        let counter = Arc::new(AtomicUsize::new(4));
        let slot = acquire_zen_slot(&counter, 4, 0, "t").await;
        assert!(!slot.is_held());
        drop(slot);
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn slot_waits_for_release() {
        let counter = Arc::new(AtomicUsize::new(1));
        let congestion = Arc::clone(&counter);
        let waiter =
            tokio::spawn(
                async move { acquire_zen_slot(&congestion, 1, 5_000, "t").await.is_held() },
            );
        // Let the waiter block on the full cap, then free a slot.
        tokio::time::sleep(Duration::from_millis(250)).await;
        counter.fetch_sub(1, Ordering::SeqCst);
        assert!(waiter.await.unwrap(), "waiter should acquire once freed");
    }

    #[tokio::test]
    async fn separate_egresses_have_independent_holds_and_slots() {
        let suffix = uuid::Uuid::new_v4();
        let egress_a = format!("test-upstream#{suffix}-a");
        let egress_b = format!("test-upstream#{suffix}-b");

        gate_hold(&egress_a, 5_000);
        assert!(gate_wait_ms(&egress_a) > 0);
        assert_eq!(gate_wait_ms(&egress_b), 0);

        let slot_a = acquire_global_zen_slot(&egress_a, 1, 0, "egress-a-first").await;
        assert!(slot_a.is_held());
        let over_a = acquire_global_zen_slot(&egress_a, 1, 0, "egress-a-over-cap").await;
        assert!(!over_a.is_held(), "same egress observes its own cap");
        let slot_b = acquire_global_zen_slot(&egress_b, 1, 0, "egress-b-first").await;
        assert!(slot_b.is_held(), "another egress has an independent slot");
    }

    #[test]
    fn zero_max_disables_without_touching_counter() {
        let counter = Arc::new(AtomicUsize::new(99));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let slot = acquire_zen_slot(&counter, 0, 1_000, "t").await;
            assert!(!slot.is_held());
        });
        assert_eq!(counter.load(Ordering::SeqCst), 99);
    }
}
