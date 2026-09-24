//! usage_observer::keys — split from usage_observer.rs (pure move, no logic change).
use super::*;
use sha2::Digest;
pub(super) struct DigestSink<'a>(pub(super) &'a mut sha2::Sha256);

impl std::io::Write for DigestSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Bounded capacities. Same rationale as the drift detector's LRU:
/// a flood of unique keys must not grow memory unboundedly.
pub(super) const PENDING_CAPACITY: usize = 512;

/// How long a pending turn can count as "still in flight" for another turn of
/// its conversation.
///
/// `complete` pops a pending entry only when the stream reached
/// `message_stop`. A 429, a stream that dropped, or a client that hung up
/// leaves the entry behind until the LRU evicts it, and every later turn of
/// that conversation then looks concurrent with a turn that ended long ago.
/// Over 2026-09-01/02, 133 of 218 `concurrent_turn_in_flight` events had no
/// other request of the session in flight at all. A real turn cannot stream
/// longer than this, so an older entry is a leftover, not a race.
pub(super) const IN_FLIGHT_HORIZON: Duration = Duration::from_secs(15 * 60);
/// How long after a clean completion a same-key turn still counts as a commit
/// race suspect (see `PendingRequest::commit_race_suspect`).
///
/// `complete` pops the pending entry the moment the stream ends, but the
/// provider commits the write later: `MissedNewestWrite` landings peak at
/// inter-turn gaps of 3-5s and fall away past 10s. Five seconds keeps scripted
/// back-to-back turns (fan-out, retries) while ordinary interactive turns,
/// which arrive slower, stay out.
///
/// Measured 2026-09-21 over 558 witnessed recache events (Anthropic models,
/// 2026-09-17..21). Among `MissedNewestWrite` landings the suspect rate is
/// 100% at every inter-turn gap below 10s, 76% at 10-60s and 20% past 60s,
/// against a 94% base rate over all witnessed events. Five seconds already
/// covers the whole race band — the two clocks differ, since this window ages
/// a *sibling completion* while the gap above is time since the previous turn.
pub(super) const COMMIT_LATENCY_WINDOW: Duration = Duration::from_secs(5);
/// How long after a clean completion a *sibling* key of the same session
/// still counts as recently completed (see
/// `PendingRequest::sibling_completed_recently`). Fan-out arrivals cluster in
/// seconds-to-a-minute; beyond that a sibling completion is context, not a
/// suspect.
pub(super) const SIBLING_COMPLETION_WINDOW: Duration = Duration::from_secs(60);
/// Bounded recent-completion log for the two windows above. Same rationale as
/// the drift detector's LRU: a flood of unique keys must not grow memory
/// unboundedly. Completions arrive far less often than requests, so 1024
/// entries cover hours of traffic; pruning is by window on read, by cap on
/// write.
pub(super) const RECENT_COMPLETION_CAPACITY: usize = 1024;
pub(super) const CONVERSATION_CAPACITY: usize = 512;

/// Anthropic's published multipliers against the base input rate. They are the
/// same for every model on the price list, which is why the stock comparison
/// can be run in input-equivalent tokens and never has to look up a price.
pub(super) const CACHE_READ_MULTIPLIER: f64 = 0.1;
pub(super) const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;
pub(super) const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;
/// Rolling window for the fleet-wide hit-rate shown in the
/// statusline (`/cache-health`).
pub(super) const RECENT_SAMPLE_CAPACITY: usize = 50;
/// Message-0 hashes of recent first turns, keyed for the fan-out check in
/// [`first_turn_reason`]. Parallel subagents launch within seconds of each
/// other, so a small window and a small table are enough.
pub(super) const FIRST_TURN_OPENER_CAPACITY: usize = 256;
pub(super) const IDENTICAL_PROMPT_FANOUT_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Watchdog conversation key — SHA-256 over the lane key (session key +
/// system digest, see `stream_lane_key`) plus the FIRST MESSAGE ONLY.
///
/// The lane is what lets same-opener subagent streams stop sharing usage
/// baselines. Price, stated plainly: a mid-conversation system rewrite now
/// mints a fresh key, so the busted turn reads as a first turn of a new
/// lineage rather than a recache of the old one. That is the correct
/// accounting — new bytes must be written once — and the backstop still
/// holds: a fresh key arriving WITH history trips the first-turn
/// contradiction path (`arrived_with_history`), which is what distinguishes
/// a genuine new lineage from a retry that abandoned its own.
///
/// This differs from [`crate::ctx::identity::conversation_key`] (which also
/// hashes `system`) only in that the lane, not the raw system, is folded in.
/// The CTX capture/injection stores keep the system-inclusive key; only the
/// watchdog needs bust-surviving identity, and the lane preserves exactly
/// the failures that matter (same-lane rewrites) while retiring the ones
/// that were always two streams (cross-lane alternation).
pub fn conversation_key(parsed: &serde_json::Value, session_key: &str) -> String {
    use sha2::{Digest, Sha256};
    // Stream the first message straight into the digest: the old
    // `first.to_string()` built a full serialized copy of msg0 just
    // to feed it here. `to_writer` emits identical bytes.
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    if let Some(first) = parsed.get("messages").and_then(|m| m.get(0)) {
        let _ = serde_json::to_writer(DigestSink(&mut hasher), first);
    }
    hex16(hasher.finalize().as_slice())
}
