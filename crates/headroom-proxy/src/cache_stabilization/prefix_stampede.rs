//! Hold same-prefix followers until their leader's cache write can be read.
//!
//! Anthropic makes a cache entry readable only once the response that wrote
//! it has begun. Requests that share a cacheable head and start inside the
//! leader's time to first byte all miss, and each one writes the same head
//! again at write price. Claude Code produces exactly that shape: a fan-out of
//! subagents of one type carries one system prompt and one tool roster, and
//! every one of them is sent in the same instant. Parallel sessions on one
//! repository do the same with the main-agent prompt.
//!
//! The gate keys on the forwarded head — model, `system`, `tools` — after
//! every rewrite has run, so two requests key alike exactly when the provider
//! would cache them alike. The first request under a cold key is the leader
//! and goes at once. Followers park until the leader's response headers are
//! in, or until `wait_cap` runs out, whichever is first. A leader that fails
//! before its first byte releases its followers, and the first of them to
//! re-enter becomes the leader. A key stays warm for `warm_ttl` after any
//! first byte, so an ordinary next turn never waits.
//!
//! Waiting is the only cost: a follower under a cold key starts at most
//! `wait_cap` later than it would have. It never changes what is sent.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

/// One tracked head.
struct Entry {
    leader_started: Instant,
    /// Set when a response under this key first began. `None` while the
    /// leader is still waiting on its own first byte.
    warm_at: Option<Instant>,
    notify: Arc<Notify>,
}

struct Inner {
    entries: Mutex<LruCache<String, Entry>>,
    wait_cap: Duration,
    warm_ttl: Duration,
}

/// Process-wide gate; clone freely.
#[derive(Clone)]
pub struct PrefixStampedeGate {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PrefixStampedeGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixStampedeGate")
            .field("wait_cap", &self.inner.wait_cap)
            .field("warm_ttl", &self.inner.warm_ttl)
            .finish()
    }
}

/// How the gate let a request through.
#[derive(Debug)]
pub enum Admission {
    /// First under a cold key: send now, then call [`LeaderToken::first_byte`]
    /// when the response headers arrive.
    Leader(LeaderToken),
    /// The key was warm; nothing to wait for.
    Warm,
    /// Parked behind a leader, then released.
    Follower {
        waited: Duration,
        release: FollowerRelease,
    },
}

/// Why a follower stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowerRelease {
    /// The leader's response began; the head is readable now.
    LeaderWarm,
    /// `wait_cap` ran out first. The follower goes as it would have without
    /// the gate.
    Timeout,
    /// The leader had been in flight past `wait_cap` before this follower
    /// arrived; treated as stuck.
    StaleLeader,
}

impl FollowerRelease {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeaderWarm => "leader_warm",
            Self::Timeout => "timeout",
            Self::StaleLeader => "stale_leader",
        }
    }
}

/// Handed to the leader; marks the head warm on first byte. Dropping it
/// without calling `first_byte` means the leader failed, and releases the
/// followers so one of them can lead.
pub struct LeaderToken {
    inner: Arc<Inner>,
    key: String,
    done: bool,
}

impl std::fmt::Debug for LeaderToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderToken")
            .field("key", &self.key)
            .field("done", &self.done)
            .finish()
    }
}

impl LeaderToken {
    /// The leader's response headers are in: the head is readable from here.
    pub fn first_byte(mut self) {
        self.done = true;
        let mut guard = match self.inner.entries.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(entry) = guard.get_mut(&self.key) {
            entry.warm_at = Some(Instant::now());
            entry.notify.notify_waiters();
        }
    }
}

impl Drop for LeaderToken {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Ok(mut guard) = self.inner.entries.lock()
            && let Some(entry) = guard.pop(&self.key)
        {
            entry.notify.notify_waiters();
        }
    }
}

enum Step {
    Admit(Admission),
    Wait {
        notify: Arc<Notify>,
        budget: Duration,
    },
}

impl PrefixStampedeGate {
    pub fn new(capacity: usize, wait_cap: Duration, warm_ttl: Duration) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(LruCache::new(capacity)),
                wait_cap,
                warm_ttl,
            }),
        }
    }

    /// Digest of the cacheable head of a forwarded Anthropic Messages body,
    /// or `None` when nothing in the head carries a `cache_control` marker
    /// (then there is no shared write to protect).
    pub fn head_key(body: &Value) -> Option<String> {
        let system = body.get("system");
        let tools = body.get("tools");
        if !has_cache_control(system) && !has_cache_control(tools) {
            return None;
        }
        let mut hasher = Sha256::new();
        if let Some(model) = body.get("model").and_then(Value::as_str) {
            hasher.update(model.as_bytes());
        }
        hasher.update([0u8]);
        if let Some(system) = system {
            let _ = serde_json::to_writer(DigestSink(&mut hasher), system);
        }
        hasher.update([0u8]);
        if let Some(tools) = tools {
            let _ = serde_json::to_writer(DigestSink(&mut hasher), tools);
        }
        let digest = hasher.finalize();
        Some(hex::encode(&digest[..16]))
    }

    /// Admit a request under `key`. Leaders return at once; followers park
    /// until the leader's first byte or `wait_cap`.
    pub async fn admit(&self, key: &str) -> Admission {
        let started = Instant::now();
        let mut parked = false;
        loop {
            match self.step(key, started, parked) {
                Step::Admit(admission) => return admission,
                Step::Wait { notify, budget } => {
                    let notified = notify.notified();
                    tokio::pin!(notified);
                    // Arm, then look again: a wake that landed between the
                    // check above and this line is not lost.
                    notified.as_mut().enable();
                    parked = true;
                    if let Step::Admit(admission) = self.step(key, started, parked) {
                        return admission;
                    }
                    if tokio::time::timeout(budget, notified).await.is_err() {
                        return Admission::Follower {
                            waited: started.elapsed(),
                            release: FollowerRelease::Timeout,
                        };
                    }
                }
            }
        }
    }

    fn step(&self, key: &str, started: Instant, parked: bool) -> Step {
        let mut guard = match self.inner.entries.lock() {
            Ok(g) => g,
            // A poisoned gate must never hold a request.
            Err(_) => return Step::Admit(Admission::Warm),
        };
        let now = Instant::now();
        let waited = started.elapsed();
        match guard.get(key) {
            Some(entry) => {
                if let Some(warm_at) = entry.warm_at {
                    if now.duration_since(warm_at) < self.inner.warm_ttl {
                        return Step::Admit(if !parked {
                            Admission::Warm
                        } else {
                            Admission::Follower {
                                waited,
                                release: FollowerRelease::LeaderWarm,
                            }
                        });
                    }
                    // Warm entry gone stale: the next sender leads again.
                    guard.pop(key);
                    return Step::Admit(self.lead(&mut guard, key, now));
                }
                let leader_age = now.duration_since(entry.leader_started);
                if leader_age >= self.inner.wait_cap {
                    return Step::Admit(Admission::Follower {
                        waited,
                        release: FollowerRelease::StaleLeader,
                    });
                }
                let budget = self.inner.wait_cap - leader_age;
                Step::Wait {
                    notify: Arc::clone(&entry.notify),
                    budget,
                }
            }
            None => Step::Admit(self.lead(&mut guard, key, now)),
        }
    }

    fn lead(&self, guard: &mut LruCache<String, Entry>, key: &str, now: Instant) -> Admission {
        guard.put(
            key.to_string(),
            Entry {
                leader_started: now,
                warm_at: None,
                notify: Arc::new(Notify::new()),
            },
        );
        Admission::Leader(LeaderToken {
            inner: Arc::clone(&self.inner),
            key: key.to_string(),
            done: false,
        })
    }

    /// A request went out under a warm or timed-out key and its response has
    /// begun: refresh the key so the provider's TTL and ours stay aligned.
    pub fn touch(&self, key: &str) {
        if let Ok(mut guard) = self.inner.entries.lock() {
            let now = Instant::now();
            match guard.get_mut(key) {
                Some(entry) => {
                    entry.warm_at = Some(now);
                    entry.notify.notify_waiters();
                }
                None => {
                    guard.put(
                        key.to_string(),
                        Entry {
                            leader_started: now,
                            warm_at: Some(now),
                            notify: Arc::new(Notify::new()),
                        },
                    );
                }
            }
        }
    }
}

fn has_cache_control(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Array(items)) => items.iter().any(|item| item.get("cache_control").is_some()),
        Some(Value::Object(map)) => map.contains_key("cache_control"),
        _ => false,
    }
}

struct DigestSink<'a>(&'a mut Sha256);

impl std::io::Write for DigestSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gate(wait_cap_ms: u64) -> PrefixStampedeGate {
        PrefixStampedeGate::new(
            8,
            Duration::from_millis(wait_cap_ms),
            Duration::from_secs(300),
        )
    }

    fn body(model: &str, system: &str) -> Value {
        json!({
            "model": model,
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "Bash"}],
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    #[test]
    fn head_key_ignores_messages_and_needs_a_marker() {
        let a = body("m", "s");
        let mut b = a.clone();
        b["messages"] = json!([{"role": "user", "content": "other"}]);
        assert_eq!(
            PrefixStampedeGate::head_key(&a),
            PrefixStampedeGate::head_key(&b)
        );
        assert_ne!(
            PrefixStampedeGate::head_key(&a),
            PrefixStampedeGate::head_key(&body("m", "t"))
        );
        assert_ne!(
            PrefixStampedeGate::head_key(&a),
            PrefixStampedeGate::head_key(&body("n", "s"))
        );
        let unmarked = json!({"model": "m", "system": "plain", "messages": []});
        assert!(PrefixStampedeGate::head_key(&unmarked).is_none());
    }

    #[tokio::test]
    async fn first_is_leader_and_next_is_warm_after_first_byte() {
        let gate = gate(1000);
        let Admission::Leader(token) = gate.admit("k").await else {
            panic!("first admit must lead");
        };
        token.first_byte();
        assert!(matches!(gate.admit("k").await, Admission::Warm));
    }

    #[tokio::test]
    async fn follower_waits_for_leader_first_byte() {
        let gate = gate(5000);
        let Admission::Leader(token) = gate.admit("k").await else {
            panic!("lead");
        };
        let g2 = gate.clone();
        let follower = tokio::spawn(async move { g2.admit("k").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!follower.is_finished());
        token.first_byte();
        match follower.await.expect("join") {
            Admission::Follower { release, waited } => {
                assert_eq!(release, FollowerRelease::LeaderWarm);
                assert!(waited >= Duration::from_millis(40));
            }
            other => panic!("expected follower, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn follower_times_out_at_wait_cap() {
        let gate = gate(60);
        let Admission::Leader(_token) = gate.admit("k").await else {
            panic!("lead");
        };
        match gate.admit("k").await {
            Admission::Follower { release, .. } => {
                assert_eq!(release, FollowerRelease::Timeout)
            }
            other => panic!("expected timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_leader_releases_followers_and_one_leads() {
        let gate = gate(5000);
        let Admission::Leader(token) = gate.admit("k").await else {
            panic!("lead");
        };
        let g2 = gate.clone();
        let follower = tokio::spawn(async move { g2.admit("k").await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(token);
        assert!(matches!(
            follower.await.expect("join"),
            Admission::Leader(_)
        ));
    }

    #[tokio::test]
    async fn stale_leader_does_not_hold_late_arrivals() {
        let gate = gate(30);
        let Admission::Leader(_token) = gate.admit("k").await else {
            panic!("lead");
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        match gate.admit("k").await {
            Admission::Follower { release, waited } => {
                assert_eq!(release, FollowerRelease::StaleLeader);
                assert!(waited < Duration::from_millis(20));
            }
            other => panic!("expected stale leader, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn warm_key_expires_after_ttl() {
        let gate =
            PrefixStampedeGate::new(8, Duration::from_millis(1000), Duration::from_millis(20));
        let Admission::Leader(token) = gate.admit("k").await else {
            panic!("lead");
        };
        token.first_byte();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(matches!(gate.admit("k").await, Admission::Leader(_)));
    }

    #[tokio::test]
    async fn different_keys_do_not_wait_on_each_other() {
        let gate = gate(5000);
        let Admission::Leader(_a) = gate.admit("a").await else {
            panic!("lead a");
        };
        assert!(matches!(gate.admit("b").await, Admission::Leader(_)));
    }
}
