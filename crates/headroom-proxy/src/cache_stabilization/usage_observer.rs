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
//! Conversations are keyed by [`conversation_key`] — hash of a stream-lane
//! key (session key + system digest) plus the first message — NOT by the
//! auth-derived session key alone, because one client (e.g. Claude Code plus
//! its subagents) runs many lineages concurrently and comparing usage across
//! different lineages would be pure noise. Same-system streams sharing a lane
//! separate inside the replay tracker's alternates; the lane keeps their
//! baselines from annihilating each other on every switch.
//!
//! Everything here is a pure observer: no request or response byte
//! is ever mutated, and all bookkeeping happens off the client byte
//! path (the SSE state-machine task).

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lru::LruCache;
use serde::Serialize;

use crate::observability::proxy_counters::record_cache_miss_attribution;

use super::prefix_replay::ReplaySkip;

/// Provider label for the cache-miss attribution metric. This observer only
/// ever sees Anthropic usage counters (see the module docs), so the label is
/// constant rather than threaded through every call site.
const MISS_ATTRIBUTION_PROVIDER: &str = "anthropic";

/// Anthropic's default ephemeral prompt-cache TTL, and the classifier's
/// threshold when nothing pins a longer one. A gap between turns longer than
/// the effective TTL makes a full re-write legitimate (TtlExpiry, not a bug).
///
/// This used to be the threshold unconditionally, on the reasoning that the
/// optional 1h tier "would only make us *more* conservative, never produce a
/// false warning". That has it backwards. Keying off the short tier does not
/// risk false warnings — it manufactures false *exonerations*: with
/// `--force-1h-cache-ttl` on, every bust in a 5-minute-to-1-hour gap was filed
/// as "expected, not a defect" and disappeared from the numbers. Measured on
/// 2026-08-17 that was 557,276 creation tokens in a day, ~3% of all creation,
/// while resumptions in those same gap bands showed 74-88% cache read share —
/// so the prefix was mostly alive and the creation needed a different
/// explanation. Pass the TTL actually pinned; see [`ANTHROPIC_CACHE_TTL_1H`].
pub const ANTHROPIC_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// The extended cache tier `--force-1h-cache-ttl` pins. Since 2025-08-13 it
/// needs no beta header.
pub const ANTHROPIC_CACHE_TTL_1H: Duration = Duration::from_secs(60 * 60);

/// Token slack for the healthy-turn comparison. `cache_read` can
/// legitimately undershoot the expectation by a few tokens
/// (breakpoint rounding); anything inside the slack is Healthy.
pub const RECACHE_SLACK_TOKENS: u64 = 64;

/// Below this, an unearned write is breakpoint rounding and not worth a line.
/// Chosen against 2026-09-07: a floor of 1,024 logs 188 turns of the day and
/// still names 96% of the unearned tokens, where a floor at the slack would
/// log 331 turns to catch the last 4%.
pub const UNEARNED_WRITE_FLOOR_TOKENS: u64 = 1_024;

/// Split a healthy turn's cache write into the part that bought new cached
/// footprint and the part that re-wrote footprint the conversation already
/// had.
///
/// A turn that reads its whole expected prefix is `Healthy` however much it
/// writes, because the classifier only ever asked whether the *read* fell
/// short. That left 65% of one day's written tokens in a bucket with no name
/// — mostly the breakpoint advancing over genuinely new content, which is the
/// mechanism working and money well spent, but not only that. Growth in the
/// cached footprint is what a write is supposed to buy; anything written
/// beyond it went over ground already covered.
///
/// Deliberately conservative: `growth` is the *whole* footprint increase, so
/// a write is called earned whenever it plausibly paid for one. This
/// undercounts rather than accuses.
pub fn split_cache_write(
    previous_footprint: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> (u64, u64) {
    let footprint = cache_read_input_tokens.saturating_add(cache_creation_input_tokens);
    let growth = footprint.saturating_sub(previous_footprint);
    let earned = cache_creation_input_tokens.min(growth);
    (earned, cache_creation_input_tokens - earned)
}

/// Bounded capacities. Same rationale as the drift detector's LRU:
/// a flood of unique keys must not grow memory unboundedly.
const PENDING_CAPACITY: usize = 512;

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
const IN_FLIGHT_HORIZON: Duration = Duration::from_secs(15 * 60);
const CONVERSATION_CAPACITY: usize = 512;

/// Anthropic's published multipliers against the base input rate. They are the
/// same for every model on the price list, which is why the stock comparison
/// can be run in input-equivalent tokens and never has to look up a price.
const CACHE_READ_MULTIPLIER: f64 = 0.1;
const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;
/// Rolling window for the fleet-wide hit-rate shown in the
/// statusline (`/cache-health`).
const RECENT_SAMPLE_CAPACITY: usize = 50;
/// Message-0 hashes of recent first turns, keyed for the fan-out check in
/// [`first_turn_reason`]. Parallel subagents launch within seconds of each
/// other, so a small window and a small table are enough.
const FIRST_TURN_OPENER_CAPACITY: usize = 256;
const IDENTICAL_PROMPT_FANOUT_WINDOW: Duration = Duration::from_secs(10 * 60);

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
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    if let Some(first) = parsed.get("messages").and_then(|m| m.get(0)) {
        let _ = serde_json::to_writer(DigestSink(&mut hasher), first);
    }
    hex16(hasher.finalize().as_slice())
}

/// Item 11's deciding test: what the cacheable part of this request actually
/// contained, split at the boundary the evidence points to.
///
/// A recache event says a prefix was re-written. It cannot say *why*, and the
/// two candidate causes need opposite fixes:
///
/// - **Real thrash** — two concurrent streams on one conversation genuinely
///   send different bytes past the tools block, so each one's prefix misses.
///   Real money, roughly 90K tokens per turn on the observed conversation.
/// - **Artefact** — [`conversation_key`] is too coarse and merged two separate
///   conversations, so ordinary alternation only *looks* like drift.
///
/// Logging these two hashes next to the key decides it. For two alternating
/// turns under one key:
///
/// - same `head`, **different** `stable` → the streams diverge after the tools
///   block. Real thrash; the waste is real spend.
/// - same `head`, **same** `stable` → identical cacheable bytes, so the key
///   merged two streams upstream treats separately (or upstream evicted).
///   The waste is an accounting artefact and item 3's totals shrink.
///
/// The split is at system+tools because that is where the observed floor sits:
/// `actual_cache_read` pinned at exactly 13,907 across twelve turns while the
/// conversation grew from 55 to 121 messages means that stream matched the
/// leading block and nothing after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixFingerprint {
    /// `model` + `system` + `tools` — the block that does cache.
    pub head: String,
    /// The first [`FINGERPRINT_FIXED_DEPTH`] messages.
    ///
    /// Fixed depth on purpose. The obvious design — hash every message except
    /// the live tail — is useless here: that region grows by one message per
    /// turn, so two turns of one conversation never agree and the field can
    /// only ever report "different". A fixed depth is comparable between any
    /// two turns of any length, which is the whole job.
    ///
    /// Depth is measured from the opener because that is where a merged key
    /// hides. `conversation_key` is `(model, first message)`, so two subagents
    /// merged by it share message 0 by construction; if they are genuinely
    /// different work they diverge within the next few turns.
    ///
    /// Empty when the conversation has not yet reached the depth — below it the
    /// hash would move purely because the conversation grew, which is the very
    /// thing the fixed depth exists to prevent. An empty value means "not
    /// comparable yet", never "no difference".
    pub body: String,
    /// Every message except the live tail. Only comparable between turns whose
    /// `stable_msgs` agree — which the alternating pairs in item 11 mostly do.
    pub stable: String,
    /// Depth `stable` covered, so a reader can tell whether two `stable`
    /// values were even measured over the same span.
    pub stable_msgs: usize,
}

/// Hash the cacheable regions of a parsed Anthropic body.
///
/// Deliberately samples rather than serialising. A full re-serialise of a
/// 1.4 MB body would cost more than the whole optimisation stage it sits in
/// (`opt_ms` median is 11ms), and this is a diagnostic. Per text fragment it
/// feeds the hasher the exact byte length plus the leading
/// [`FINGERPRINT_SAMPLE_BYTES`], which two different conversations collide on
/// only if every fragment shares both — not a case worth engineering against
/// for a field whose job is to tell two live streams apart.
pub fn prefix_fingerprint(parsed: &serde_json::Value) -> PrefixFingerprint {
    prefix_fingerprint_with_model(parsed, None)
}

/// As [`prefix_fingerprint`], but with `identity_model` standing in for the
/// body's own `model`.
///
/// Same reason as `derive_session_key_with_model`: a turn the cost-aware
/// router sent to another upstream is the same conversation, and the
/// fingerprint has to match the one the previous turn left behind.
pub fn prefix_fingerprint_with_model(
    parsed: &serde_json::Value,
    identity_model: Option<&str>,
) -> PrefixFingerprint {
    use sha2::{Digest, Sha256};

    let mut head = Sha256::new();
    if let Some(model) = identity_model.or_else(|| parsed.get("model").and_then(|v| v.as_str())) {
        head.update(model.as_bytes());
    }
    for key in ["system", "tools"] {
        head.update([0xff]);
        if let Some(v) = parsed.get(key) {
            sample_value(v, &mut head);
        }
    }

    let mut body = Sha256::new();
    let mut stable = Sha256::new();
    let mut stable_msgs = 0usize;
    let mut body_comparable = false;
    if let Some(msgs) = parsed.get("messages").and_then(|v| v.as_array()) {
        // Only meaningful once the conversation is longer than the depth.
        // Below that, `take(depth)` returns a different number of messages on
        // every turn, so the hash would change purely because the
        // conversation grew — the exact failure mode the fixed depth exists to
        // avoid. Report nothing rather than something incomparable.
        if msgs.len() > FINGERPRINT_FIXED_DEPTH {
            body_comparable = true;
            for m in msgs.iter().take(FINGERPRINT_FIXED_DEPTH) {
                body.update([0xff]);
                sample_value(m, &mut body);
            }
        }
        // Drop the live tail: it differs between turns by design.
        let end = msgs.len().saturating_sub(1);
        for m in &msgs[..end] {
            stable.update([0xff]);
            sample_value(m, &mut stable);
            stable_msgs += 1;
        }
    }

    PrefixFingerprint {
        head: hex16(head.finalize().as_slice()),
        body: if body_comparable {
            hex16(body.finalize().as_slice())
        } else {
            String::new()
        },
        stable: hex16(stable.finalize().as_slice()),
        stable_msgs,
    }
}

/// Leading bytes taken from each text fragment. Enough that two different
/// messages differ, small enough that the walk stays off the latency budget.
const FINGERPRINT_SAMPLE_BYTES: usize = 64;

/// Messages covered by [`PrefixFingerprint::body`]. Deep enough that two
/// different lines of work have diverged, shallow enough to stay comparable on
/// a short conversation.
const FINGERPRINT_FIXED_DEPTH: usize = 8;

/// Walk a value feeding the hasher structure plus bounded text samples. Never
/// allocates a serialised copy — string fragments are hashed in place.
fn sample_value(v: &serde_json::Value, hasher: &mut impl sha2::Digest) {
    match v {
        serde_json::Value::String(s) => {
            hasher.update((s.len() as u64).to_le_bytes());
            let n = s.len().min(FINGERPRINT_SAMPLE_BYTES);
            hasher.update(&s.as_bytes()[..n]);
        }
        serde_json::Value::Array(items) => {
            hasher.update((items.len() as u64).to_le_bytes());
            for item in items {
                sample_value(item, hasher);
            }
        }
        serde_json::Value::Object(map) => {
            hasher.update((map.len() as u64).to_le_bytes());
            // serde_json preserves insertion order by default; hash the keys
            // too so a reordered object is not mistaken for the same one.
            for (k, val) in map {
                hasher.update(k.as_bytes());
                sample_value(val, hasher);
            }
        }
        serde_json::Value::Number(n) => hasher.update(n.to_string().as_bytes()),
        serde_json::Value::Bool(b) => hasher.update([*b as u8]),
        serde_json::Value::Null => hasher.update([0u8]),
    }
}

fn hex16(digest: &[u8]) -> String {
    // `hex` is already a workspace dep; identical lowercase output to
    // the per-byte `format!` loop at ~6x.
    hex::encode(&digest[..8])
}

/// The usage counters of one completed turn, as billed by Anthropic.
#[derive(Debug, Clone, Copy)]
pub struct TurnRecord {
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub at: SystemTime,
    /// Serialized body size actually sent upstream, retained so a later miss
    /// can prove that the replayed request shrank or grew.
    pub forwarded_request_bytes: Option<u64>,
    /// Messages this turn carried, less the live tail — the stream
    /// discriminator (see [`match_stream`]). `None` when the request reached
    /// the observer without a fingerprint, which falls back to single-stream
    /// behaviour.
    pub msgs: Option<usize>,
    /// This turn was itself a `prefix_content_diverged` bust.
    ///
    /// Carried forward one turn so the turn *after* a divergence can be named.
    /// Measured 2026-08-13: after a 210k-token divergence the next turn read
    /// only the system-and-tools floor and rewrote another 212k, landing in the
    /// residual bucket as though it had no cause. It had one — the turn before.
    /// (A second divergence the same minute recovered immediately, so this is
    /// an aftershock that happens, not one that always happens.)
    pub diverged: bool,
    /// `cache_read + cache_creation` of the turn this one continued, when it
    /// continued one. Carried one turn so a miss can be placed against *two*
    /// earlier boundaries, not one — see [`CacheLanding`].
    pub previous_boundary: Option<u64>,
    /// This turn's [`PrefixFingerprint::head`] — model, system and tools as the
    /// client sent them — so the next turn of the same stream can tell whether
    /// the cacheable head moved under it.
    ///
    /// Kept here because the drift detector cannot answer that question: it
    /// compares consecutive *requests* of a session, so a request that never
    /// completes still consumes the change, and the turn that is billed for it
    /// reads empty dims. Comparing against the previous turn that completed
    /// cannot be spent by a request that never arrives.
    ///
    /// The eight digest bytes as a `u64` rather than their hex, so the record
    /// stays `Copy`. `None` when the request reached the observer without a
    /// fingerprint, which compares as "not known", never as "unchanged".
    pub head: Option<u64>,
    /// The stock arm's cached prefix after this turn (`stock_read +
    /// stock_write`), carried per stream the way the rest of this record is.
    ///
    /// One `u64` per conversation used to live in a side map, so two
    /// same-lane streams sharing a key — a main loop and its subagent fork —
    /// priced against each other's prefix: a large stream following a small
    /// one read the small footprint back, rebuilt the difference on the stock
    /// arm at 1.25x, and reported up to ~38 points of saving on byte-identical
    /// traffic. A new lineage starts at 0, which is also what the rebuild
    /// below assumes.
    pub stock_footprint: u64,
}

/// Streams tracked per conversation key before the oldest is dropped.
///
/// One key really does carry several: measured live 2026-08-09, three of five
/// multi-event keys had a message count that ran *backwards* between turns
/// (17→16→35→28), which no single growing conversation can do.
const MAX_STREAMS_PER_CONVERSATION: usize = 8;

/// Pick which tracked stream a turn continues, by the one invariant a
/// conversation cannot break: it only ever grows.
///
/// `conversation_key` is `(session key, first message)`, so anything that
/// forks from a shared opener — a subagent inheriting its parent's context,
/// two clients resuming one transcript — lands on one key. Comparing a turn
/// of stream A against the last turn of stream B then reports a cache bust
/// that never happened, which is items 5 and 11 in the observation doc.
///
/// Matching on the message count and nothing else is deliberate. The obvious
/// refinement — also require the early-message fingerprint to agree — would
/// make the watchdog blind to the failure it exists to catch: an *edit* inside
/// the cached prefix changes those bytes while the count stays put, and that
/// turn would be filed as a brand-new stream instead of the bust it is. The
/// same reasoning already keeps `system` out of [`conversation_key`].
fn match_stream(streams: &[TurnRecord], msgs: Option<usize>) -> Option<usize> {
    let Some(msgs) = msgs else {
        // Nothing to discriminate on — behave as before and compare against
        // the most recent turn.
        return streams
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| r.at)
            .map(|(i, _)| i);
    };
    // The closest stream this turn could be a continuation of: the longest one
    // that is not already longer than this turn. A turn shorter than every
    // tracked stream continues none of them, and starts its own.
    streams
        .iter()
        .enumerate()
        .filter(|(_, r)| r.msgs.is_none_or(|m| m <= msgs))
        .max_by_key(|(_, r)| (r.msgs.unwrap_or(0), r.at))
        .map(|(i, _)| i)
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
    input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_ttl: Duration,
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
        //
        // Unless the prompt was billed as fresh input instead, which is the
        // same money under another name and was invisible here until
        // 2026-09-08: this arm only ever looked at what was *written*. Six
        // turns on 09-07 read nothing of an expected prefix, wrote nothing,
        // and paid full input price for the lot (one read 0 against an
        // expected 14,080 while billing 14,226 fresh). Cap the charge at what
        // was actually billed fresh, the way the written case caps at what
        // was written.
        if input_tokens <= RECACHE_SLACK_TOKENS {
            return TurnClass::Healthy;
        }
        return TurnClass::Recache {
            wasted_tokens: shortfall.min(input_tokens),
        };
    }
    let gap = now.duration_since(prev.at).unwrap_or(Duration::ZERO);
    if gap > cache_ttl {
        return TurnClass::TtlExpiry;
    }
    TurnClass::Recache {
        wasted_tokens: shortfall.min(cache_creation_input_tokens),
    }
}

/// Provenance for the histories compared by prefix replay.
///
/// Keep this explicit: a final-message difference is a branch/tail build only
/// when it came from the client histories entering the proxy. A comparison of
/// transformed/forwarded messages must never receive that attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayComparisonOrigin {
    InboundOriginalHistories,
}

/// Structured evidence from a prefix-replay decline.
///
/// The replay stage has the typed reason plus both message slices. Parking all
/// of the evidence here avoids collapsing `PrefixContentDiverged` to a string
/// before the response-side usage counters can distinguish a replaced live
/// tail from a deeper edit inside the cached prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaySkipEvidence {
    reason: ReplaySkip,
    comparison_origin: ReplayComparisonOrigin,
    prior_message_count: Option<usize>,
    current_message_count: usize,
}

/// Evidence that a stored prefix was selected and actually serialized onto
/// the upstream request. This deliberately stops at the proxy/provider
/// boundary: it proves what the proxy sent, without guessing why the provider
/// subsequently failed to read all of it from cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayAppliedEvidence {
    chain_id: u64,
    breakpoints_placed: usize,
    system_markers_dropped: usize,
}

impl ReplayAppliedEvidence {
    pub fn new(chain_id: u64, breakpoints_placed: usize, system_markers_dropped: usize) -> Self {
        Self {
            chain_id,
            breakpoints_placed,
            system_markers_dropped,
        }
    }
}

impl ReplaySkipEvidence {
    /// Message index where the prefix first differed, when that was the reason.
    ///
    /// The whole cost story turns on this number: an edit in the first quarter
    /// of the prefix destroys far more than one in the third, and the index is
    /// what separates "the client deleted mid-history" from "the opener churns
    /// every turn". It was computed and then dropped before reaching the log.
    pub fn first_diff_index(&self) -> Option<usize> {
        match self.reason {
            ReplaySkip::PrefixContentDiverged {
                first_diff_index, ..
            } => Some(first_diff_index),
            _ => None,
        }
    }

    /// Messages the stored turn carried, and this one carries. A count that
    /// holds steady or falls while the content changed is the deletion
    /// signature; one that grows is ordinary appending.
    pub fn message_counts(&self) -> (Option<usize>, usize) {
        (self.prior_message_count, self.current_message_count)
    }

    /// Evidence produced by comparing the prior and current inbound originals.
    pub fn from_inbound_original_histories(
        reason: ReplaySkip,
        prior: Option<&[serde_json::Value]>,
        current: &[serde_json::Value],
    ) -> Self {
        Self {
            reason,
            comparison_origin: ReplayComparisonOrigin::InboundOriginalHistories,
            prior_message_count: prior.map(|messages| messages.len()),
            current_message_count: current.len(),
        }
    }

    fn is_inbound_tail_replacement(self) -> bool {
        let Some(prior_count) = self.prior_message_count else {
            return false;
        };
        if prior_count == 0 || prior_count != self.current_message_count {
            return false;
        }
        matches!(
            (self.comparison_origin, self.reason),
            (
                ReplayComparisonOrigin::InboundOriginalHistories,
                ReplaySkip::PrefixContentDiverged { first_diff_index, .. }
            ) if first_diff_index == prior_count - 1
        )
    }
}

/// What the request side knows about a turn that may turn out to be the first
/// completed one under its conversation key. Computed once in the handler,
/// where the parsed body is, and parked here until the usage arrives — see
/// [`UsageObserver::note_first_turn_context`].
#[derive(Debug, Clone, Default)]
pub struct FirstTurnContext {
    /// Messages the client sent, before any compression.
    pub msgs: usize,
    /// Canonical hash of message 0, for the identical-prompt fan-out check.
    pub message_zero_hash: Option<String>,
    /// Message 0 carries Claude Code's compaction summary marker.
    pub compaction_restart: bool,
    pub model: Option<String>,
}

/// Outcome of the cross-session prefix adoption path: the replay store found a
/// donor tracker under another session key whose originals prefix-match this
/// request. Whether the donor's bytes reached the wire is read off the replay
/// evidence when the event is emitted.
#[derive(Debug, Clone)]
pub struct PrefixAdoption {
    pub donor_session_key_hash: String,
}

/// Compute [`FirstTurnContext`] from the client's parsed body, once, on the
/// request path.
pub fn first_turn_context(parsed: &serde_json::Value) -> FirstTurnContext {
    use sha2::{Digest, Sha256};
    let messages = parsed
        .get("messages")
        .and_then(|m| m.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let message_zero_hash = messages.first().map(|m| {
        let canonical = super::prefix_replay::canonicalize_for_prefix_compare(m);
        // Stream into the digest instead of materializing the
        // serialized copy (identical input bytes, no buffer).
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
        let mut hasher = Sha256::new();
        let _ = serde_json::to_writer(DigestSink(&mut hasher), &canonical);
        hex16(hasher.finalize().as_slice())
    });
    let compaction_restart = crate::ctx::identity::first_user_message_text(parsed)
        .is_some_and(|t| crate::ctx::identity::has_compaction_marker(&t));
    FirstTurnContext {
        msgs: messages.len(),
        message_zero_hash,
        compaction_restart,
        model: parsed
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }
}

/// Why the first completed turn under a conversation key wrote cache. Bounded
/// vocabulary; the metric label is built from it.
///
/// Precedence, when more than one applies: compaction restart, then session
/// key drift, then identical-prompt fan-out, then a fresh session, and
/// `arrived_with_history` when nothing else explains a turn that carried more
/// than an opener.
pub fn first_turn_reason(
    ctx: &FirstTurnContext,
    adoption: Option<&PrefixAdoption>,
    opener_seen_elsewhere: bool,
) -> &'static str {
    if ctx.compaction_restart {
        "compaction_restart"
    } else if adoption.is_some() {
        "session_key_drift"
    } else if ctx.msgs <= 2 && opener_seen_elsewhere {
        "identical_prompt_fanout"
    } else if ctx.msgs <= 2 {
        "fresh_session"
    } else {
        "arrived_with_history"
    }
}

/// Request-side context parked until the response's usage arrives.
#[derive(Debug, Clone)]
struct PendingRequest {
    conversation_key: String,
    /// See [`FirstTurnContext`]. `None` when the handler never filled it in.
    first_turn: Option<FirstTurnContext>,
    /// See [`PrefixAdoption`]. Set on the forward path, after `begin_request`.
    adoption: Option<PrefixAdoption>,
    /// Monotonic, so a wall clock that steps backwards cannot age an entry.
    began: Instant,
    /// Another turn of this same conversation was still in flight when this
    /// one began.
    ///
    /// Read from the pending map rather than from timestamps: this machine's
    /// wall clock steps backwards under load, and the map already knows the
    /// answer exactly.
    concurrent_with_in_flight: bool,
    /// The *drift detector's* session hash, so a recache event joins to the
    /// drift and volatile events on the same request. Only
    /// [`UsageObserver::begin_request`] fills this, and it derives the hash
    /// itself. `None` when the request never reached the drift gate.
    session_key_hash: Option<String>,
    /// Why prefix replay declined on this turn, when it did. Set after
    /// [`UsageObserver::begin_request`] because the replay decision happens
    /// later, on the forward path.
    replay_skip: Option<ReplaySkipEvidence>,
    /// A prior forwarded prefix was successfully put back on the wire.
    replay_applied: Option<ReplayAppliedEvidence>,
    /// `(tokens_before, tokens_after)` from this turn's compression, set on the
    /// forward path. Parked so the response side can price the saving against
    /// the billed usage that comes back — see [`UsageObserver::complete`].
    compression: Option<(u64, u64)>,
    /// Body size the client sent, before any transform. The denominator of the
    /// ground-truth ledger: work requested, measured before the proxy touches
    /// it and therefore not something the proxy can flatter.
    client_request_bytes: Option<u64>,
    /// Body size actually put on the wire.
    forwarded_request_bytes: Option<u64>,
    /// Which compression arm this turn ran under, so an on/off comparison is a
    /// query rather than an argument.
    compression_mode: Option<&'static str>,
    /// Item 11 decider, parked here because the recache event fires on the
    /// response side where the body is long gone.
    prefix: Option<PrefixFingerprint>,
    /// PR-E6 drift dimensions observed on this request, when any —
    /// the "why" attached to a re-cache event.
    drift_dims: Option<String>,
    /// The same dimensions measured on the body actually sent, after every
    /// proxy stage has run. Set by `note_outbound_drift`; `None` on a turn
    /// that never reached the forwarding path.
    outbound_drift_dims: Option<String>,
    /// `(input, cache_read, cache_write)` actually billed across every round of
    /// this request, when the proxy ran more than one. The usage passed to
    /// [`UsageObserver::complete`] is deliberately the *client baseline* — the
    /// footprint of the request the client sent — because that is what the next
    /// client turn can be compared against. A hidden CCR continuation round is
    /// still real money, so the ledger adds it back or it reports less than
    /// the bill, and the stock comparison's ours arm prices it the same way —
    /// while the stock arm stays on the baseline, since a stock client never
    /// runs continuation rounds. `None` on the common single-round path.
    billed_totals: Option<(u64, u64, u64)>,
    /// Completion tokens, where the path that billed them reports them.
    /// The ledger line is the richest per-turn record on disk for a turn that
    /// saved nothing, and turn cost cannot be rebuilt from it without this.
    billed_output: Option<u64>,
    /// The TTL shape the client asked for on this turn's own markers, for
    /// the stock arm: a stock client sends what we sent, so its prefix
    /// lives as long as the tier this turn bought. Set by
    /// [`UsageObserver::note_client_cache_ttl`]; unmarked when the path
    /// never noted one (the provider default, and the old flat
    /// assumption).
    client_ttl: super::cache_ttl::ClientTtl,
}

/// Where the provider's cache read landed against the two previous boundaries,
/// for a miss the proxy cannot explain from its own side.
///
/// Every `unexplained_after_replay` event over 2026-09-01..02 (563 events,
/// 547k tokens) had a byte-stable forwarded prefix; what differed was only
/// where `cache_read` stopped relative to what the two previous turns read
/// and wrote. Naming that position is what turns the residual bucket into
/// five provider-side behaviours that can each be counted.
///
/// With `prev_read`/`prev_boundary` the previous turn's read and
/// read+creation, and `prevprev_boundary` the boundary of the turn before it:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheLanding {
    /// `actual == prev_read`: the write the previous turn made was not found.
    /// Typical at gaps under 3s.
    MissedNewestWrite,
    /// `prev_read < actual < prev_boundary`: the read stops inside the
    /// previous write. On Fable a fixed 69–114 tokens.
    PartialOfPreviousWrite,
    /// `actual == prevprev_boundary` while `prev_read > prevprev_boundary`:
    /// the previous turn read past anything ever written, so the provider
    /// served a segment it never persisted, then lost it.
    FreeReadNotPersisted,
    /// `actual < prevprev_boundary` (or `actual < prev_read` when no earlier
    /// boundary is known): an older entry is gone, with the prefix stable.
    DroppedOlderEntry,
    /// Anything else below `prev_read`.
    BetweenEntries,
}

impl CacheLanding {
    fn classify(
        actual: u64,
        prev_read: u64,
        prev_boundary: u64,
        prevprev_boundary: Option<u64>,
    ) -> Self {
        if actual == prev_read {
            return Self::MissedNewestWrite;
        }
        if actual > prev_read && actual < prev_boundary {
            return Self::PartialOfPreviousWrite;
        }
        match prevprev_boundary {
            Some(pp) if actual == pp && prev_read > pp => Self::FreeReadNotPersisted,
            Some(pp) if actual < pp => Self::DroppedOlderEntry,
            Some(_) => Self::BetweenEntries,
            None if actual < prev_read => Self::DroppedOlderEntry,
            None => Self::BetweenEntries,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::MissedNewestWrite => "provider_missed_newest_write",
            Self::PartialOfPreviousWrite => "provider_partial_of_previous_write",
            Self::FreeReadNotPersisted => "provider_free_read_not_persisted",
            Self::DroppedOlderEntry => "provider_dropped_older_entry",
            Self::BetweenEntries => "provider_between_entries",
        }
    }
}

/// Return a cause only when the request supplied direct evidence for it.
///
/// Structural drift is evidence by definition. Replay declines are evidence
/// only when they describe a mismatch with a previously stored prefix;
/// `no_previous_turn` and future/unrecognised values do not establish why the
/// provider re-cached anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecacheAttribution<'a> {
    reason: Option<&'a str>,
    origin: Option<&'static str>,
    scope: Option<&'static str>,
    counts_as_waste: bool,
}

fn recache_attribution<'a>(
    drift_dims: Option<&'a str>,
    head_changed: bool,
    outbound_drift_dims: Option<&'a str>,
    replay_skip: Option<ReplaySkipEvidence>,
    replay_applied: Option<ReplayAppliedEvidence>,
    previous_turn_diverged: bool,
    concurrent_with_in_flight: bool,
) -> RecacheAttribution<'a> {
    if replay_skip.is_some_and(ReplaySkipEvidence::is_inbound_tail_replacement) {
        return RecacheAttribution {
            reason: Some("inbound_tail_replaced"),
            origin: Some("inbound"),
            scope: Some("final_message"),
            counts_as_waste: false,
        };
    }

    if let Some(dims) = drift_dims.filter(|dims| !dims.is_empty()) {
        // The dims come from the inbound hash, taken before the proxy touches
        // anything, so whatever moved was moved by the client. The dims name
        // which part of the hot zone it was.
        return RecacheAttribution {
            reason: Some(dims),
            origin: Some("client"),
            scope: Some("hot_zone"),
            counts_as_waste: true,
        };
    }

    // The cacheable head — model, system, tools — is not the one the previous
    // completed turn of this stream sent, so the provider keyed on something
    // else and everything behind it had to be written again. Client origin and
    // client scope: this is the same hot zone `drift_dims` names, read one
    // turn apart instead of one request apart.
    //
    // `drift_dims` alone cannot see it. The drift detector is edge-triggered on
    // a per-session LRU, so the first request carrying a change consumes the
    // edge whether or not it ever completes. On 2026-09-03 a stream died
    // mid-response, the client resent the turn with a system prompt 6 kB
    // shorter (16,125 B → 10,145 B), and the abandoned retry took the edge at
    // 15:56:36Z; the attempt that was billed nine seconds later saw empty dims,
    // wrote 213,309 tokens against 15,621 read — the day's largest write — and
    // fell through to `concurrent_turn_in_flight`, which was true and not the
    // cause. `prefix_head` on this event and `prefix_composition` on the
    // request say which of system or tools moved.
    if head_changed {
        return RecacheAttribution {
            reason: Some("prefix_head_changed"),
            origin: Some("client"),
            scope: Some("hot_zone"),
            counts_as_waste: true,
        };
    }

    // Exhaustive on the enum, not on `as_str()`. The string form let three
    // variants added after this filter — `SystemAdjacencyBroken` (09-02),
    // `InflatedWithoutConfirmedFloor` (09-07), `OptimizedShorterThanOriginals`
    // — join the unnamed bucket in silence, and a declined replay with no name
    // reads as a benign cache reset. A ninth variant now fails to compile here
    // until someone decides which side it belongs on.
    let reason = replay_skip.and_then(|evidence| match evidence.reason {
        // The client's own history no longer continues the prefix we stored:
        // it edited inside the prefix, or a second stream shares one session
        // key. Client evidence, ranked with a moved inbound hash below.
        ReplaySkip::PrefixContentDiverged { .. } => Some("prefix_content_diverged"),
        ReplaySkip::ForwardedCountMismatch => Some("forwarded_count_mismatch"),
        ReplaySkip::ShorterThanStoredPrefix => Some("shorter_than_stored_prefix"),
        ReplaySkip::OptimizedShorterThanPrefix => Some("optimized_shorter_than_prefix"),
        ReplaySkip::OptimizedShorterThanOriginals => Some("optimized_shorter_than_originals"),
        // These three say why the replay stood down, not why the cache moved,
        // so they are not ranked as causes. They are no longer *lost*: a turn
        // that burned tokens behind one of them now carries it as the reason
        // of an `Unexplained` event rather than falling through to `Expected`.
        ReplaySkip::NoPreviousTurn
        | ReplaySkip::InflatedWithoutConfirmedFloor
        | ReplaySkip::SystemAdjacencyBroken => None,
    });
    // Two of those four say the client's own history no longer continues the
    // prefix we stored for it: it edited inside the prefix, or it is a second
    // stream sharing one session key. That is client evidence, like a moved
    // inbound hash, and it is ranked with it.
    //
    // Ranked above the outbound hash because declining a replay is itself what
    // moves the forwarded hot zone: the overlay that had been restoring the
    // stored early messages stops, and the forwarded prefix snaps back to the
    // client's own bytes. Measured 2026-09-03: on 7 turns the client inserted a
    // `role:"system"` message at index 1, replay declined at
    // `first_diff_index=1`, and the outbound hash read `0:blocks 2->1` on the
    // very message the overlay had been replaying. Those turns were filed
    // `origin=proxy / forwarded_hot_zone` — the proxy charged for withdrawing
    // its own overlay in response to an edit only the client could make.
    //
    // The other two — `forwarded_count_mismatch` and
    // `optimized_shorter_than_prefix` — name our own pipeline dropping
    // messages, so they stay below the outbound hash, which speaks for the
    // proxy.
    if let Some(reason @ ("prefix_content_diverged" | "shorter_than_stored_prefix")) = reason {
        return RecacheAttribution {
            reason: Some(reason),
            origin: Some("client"),
            scope: Some("stored_prefix"),
            counts_as_waste: true,
        };
    }

    // The client's hot zone held still and ours did not, so the mutation was
    // ours. Checked after client drift, never before: when both moved, the
    // client's edit is the cause and the proxy only carried it forward.
    //
    // This is the one attribution the classifier could not make. Everything
    // the proxy does — tool injection, context injection, prefix replay, the
    // breakpoint move — runs after the inbound hash is taken, so a recache it
    // caused was indistinguishable from one nobody could explain.
    if let Some(dims) = outbound_drift_dims.filter(|dims| !dims.is_empty()) {
        return RecacheAttribution {
            reason: Some(dims),
            origin: Some("proxy"),
            scope: Some("forwarded_hot_zone"),
            counts_as_waste: true,
        };
    }

    // Residual, not a finding. Everything above named a cause from evidence;
    // reaching here means a replay went out and the read still came back short,
    // with nothing to say why.
    //
    // Deliberately NOT called a provider miss. `ReplayAppliedEvidence` proves
    // the overlay serialized stored bytes onto the request — it does not prove
    // those bytes match what the provider cached, because the equality behind
    // it is content-only and tolerates a `cache_control` marker moving between
    // messages (see `overlay_survives_moved_cache_control_marker`). Blaming the
    // provider here would assert something the evidence does not carry, and
    // that misreading is what sends people hunting a provider bug.
    // Another turn of this conversation was still running when this one
    // started, so the write it should have read may not have been committed
    // yet. Measured over the 2026-08-20/22 logs: 72% of overlapping turn-pairs
    // lose cache against a ~5% baseline, 377 pairs and 408,980 tokens, and 374
    // of them had a prefix replay applied — the splice was right, the timing
    // was not.
    //
    // Still counted as waste. The tokens were genuinely re-billed, and calling
    // it expected would retire 409k tokens into a bucket nobody looks at; if
    // the fan-out that causes it turns out to be avoidable, that is a saving,
    // not a fact of life. Checked after every structural cause, so a real edit
    // still wins the attribution.
    if reason.is_none() && concurrent_with_in_flight {
        return RecacheAttribution {
            reason: Some("concurrent_turn_in_flight"),
            origin: Some("client"),
            scope: Some("provider_cache_timing"),
            counts_as_waste: true,
        };
    }

    if reason.is_none() && replay_applied.is_some() {
        // The turn before this one diverged. That is a cause, and naming it
        // keeps a divergence's aftershock out of the residual bucket — the
        // whole point being that one client edit can bill twice.
        if previous_turn_diverged {
            return RecacheAttribution {
                reason: Some("aftershock_of_diverged_prefix"),
                origin: Some("previous_turn"),
                scope: Some("replayed_prefix"),
                counts_as_waste: true,
            };
        }
        return RecacheAttribution {
            reason: Some("unexplained_after_replay"),
            origin: Some("unknown"),
            scope: Some("replayed_prefix"),
            counts_as_waste: true,
        };
    }
    // Every reason left is a replay-skip: the body the client sent no longer
    // continues the prefix we stored for it, so the store is what went stale.
    RecacheAttribution {
        reason,
        origin: reason.map(|_| "client"),
        scope: reason.map(|_| "stored_prefix"),
        counts_as_waste: true,
    }
}

/// Severity classification of a re-cache event, derived from direct
/// attribution evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecacheEventKind {
    /// Direct structural-drift or replay-mismatch evidence attributes the
    /// cache bust — real waste, warn.
    Drift,
    /// The inbound history replaced only its final message. The provider must
    /// create that branch tail, but no reusable cached prefix was wasted.
    Branch,
    /// The proxy put the stored prefix back on the wire, but the provider did
    /// not read the expected cache footprint. This attributes the boundary,
    /// not an unproved provider-internal cause.
    Unexplained,
    /// No direct structural-drift or replay-mismatch evidence. This is an
    /// unattributed event, not evidence of a benign reset.
    Expected,
}

/// One re-cache event, kept for `/cache-health` (most recent only)
/// and the WARN log.
#[derive(Debug, Clone, Serialize)]
pub struct RecacheEvent {
    /// Unix seconds — snapshot consumers compute the age themselves.
    pub at_unix: u64,
    pub conversation_key: String,
    /// The drift detector's session hash, so `/cache-health` names the same
    /// session the drift and volatile log events do. `None` when the request
    /// never reached the drift gate.
    pub session_key_hash: Option<String>,
    /// PR-E6 drift axes ("system" / "tools" / "early_messages",
    /// comma-joined) when the drift detector saw structural change.
    pub drift_dims: Option<String>,
    /// Stable, explicit cause derived only from direct evidence. This is a
    /// structural drift dimension or a causal prefix-replay skip reason;
    /// `None` means the event is genuinely unattributed.
    pub attribution_reason: Option<String>,
    /// Provenance of the compared histories when it is known.
    pub origin: Option<String>,
    /// Structural extent of the change when it is known.
    pub scope: Option<String>,
    /// True only when a stored prefix was confirmed on the serialized wire
    /// body for this request.
    pub replayed_prefix: bool,
    pub replay_chain_id: Option<u64>,
    pub breakpoints_placed: Option<usize>,
    pub system_markers_dropped: Option<usize>,
    pub previous_forwarded_request_bytes: Option<u64>,
    pub forwarded_request_bytes: Option<u64>,
    /// `Drift` for charged prefix changes, `Branch` for a legitimate inbound
    /// tail build, and `Expected` when the rebuild is unattributed.
    pub event_kind: RecacheEventKind,
    pub wasted_tokens: u64,
    /// Tokens the provider created for this turn, whether waste or a legitimate
    /// branch-tail cache build.
    pub cache_creation_input_tokens: u64,
    pub expected_cache_read: u64,
    pub actual_cache_read: u64,
}

/// JSON body served by `GET /cache-health`. Designed to be cheap to
/// render (statusline polls it every few seconds): everything comes
/// from one in-memory snapshot, no I/O on the read path.
#[derive(Debug, Clone, Serialize)]
pub struct CacheHealthSnapshot {
    /// Mean cache-hit rate over the last [`RECENT_SAMPLE_CAPACITY`]
    /// completed Anthropic requests across every session handled by this proxy
    /// process; `null` until the first sample. This is an ambient fleet signal,
    /// not the rate for the session that happens to render the statusline.
    /// Turns whose provider reported no cache-usage data at all leave no
    /// capable sample and are excluded from the mean, so a provider with no
    /// cache telemetry cannot drag the fleet rate toward zero. `samples`
    /// counts the capable turns the mean is over.
    pub recent_hit_rate: Option<f64>,
    pub samples: usize,
    pub recache_events_total: u64,
    pub recache_wasted_tokens_total: u64,
    pub ttl_expiries_total: u64,
    /// First completed turns under a conversation key that wrote cache, and
    /// the tokens they wrote. Not waste — a cold start has nothing to read —
    /// but 2,729,094 tokens went through here on 2026-09-07 with no counter
    /// of any kind behind them, so the one category nobody could size was
    /// also the largest. Countable now; still uncharged.
    /// Healthy-turn cache writes split by whether they bought new cached
    /// footprint. `earned` is normal operation — the breakpoint advancing over
    /// content the conversation had not cached before — and is reported so the
    /// total reconciles, not because anything is wrong with it. `unearned` is
    /// the part that re-cached ground already covered — the savings-candidate
    /// number. The two sum to every cache-write token observed, which is what
    /// makes `productive_write_pct` below a share and not an estimate.
    pub earned_cache_write_tokens_total: u64,
    pub unearned_cache_write_tokens_total: u64,
    pub unearned_write_turns_total: u64,
    /// Requests that entered the pipeline and were pushed out of the pending
    /// cache before anything completed them.
    ///
    /// Every one is tokens the proxy forwarded and the books never saw. Some
    /// are legitimate — an upstream 429 bills nothing — so this is a seam to
    /// look at rather than a fault on its own. Zero is the only value that
    /// needs no explanation.
    pub abandoned_requests_total: u64,
    /// Turns shed by the conversation-concurrency cap before anything was
    /// forwarded, so unlike `abandoned_requests_total` these cost nothing and
    /// miss nothing: the client retries them against a committed prefix.
    /// Zero until the cap is configured and a fan-out trips it.
    pub concurrency_sheds_total: u64,
    /// Turns where the client's hot zone (model, system, tools) changed.
    ///
    /// Every one is a turn a stock client would have been at risk of
    /// re-caching from the system block down. `head` is hashed over the body
    /// the client sent, not the held view the drift detector sees, which is
    /// what makes this a statement about the client rather than about us.
    pub hot_zone_changes_total: u64,
    /// The subset that re-cached anyway: stabilisation did not absorb them.
    pub hot_zone_recaches_total: u64,
    /// The subset that read its cache regardless — absorbed.
    pub stabilization_absorbed_total: u64,
    /// Footprint those turns kept, summed. Each is the previous turn's
    /// observed read plus write, so it is what the provider billed last turn
    /// and not a guess at what a rebuild would cost.
    pub stabilization_absorbed_tokens_total: u64,
    /// Share of the client's hot-zone changes that stabilisation absorbed.
    ///
    /// The honest headline for "what is stabilisation worth": of the changes
    /// that would have cost a stock client its cache, this many did not cost
    /// this one. 100.0 with no hot-zone changes yet, because nothing has been
    /// missed. Read it beside `hot_zone_changes_total` -- a rate over three
    /// turns means nothing.
    pub stabilization_absorb_pct: f64,

    /// How this proxy compares with a plain Claude Code client -- no
    /// compression, no offload, no holds -- on the same traffic.
    ///
    /// Both arms are counted in input-equivalent tokens: fresh input at 1x,
    /// cache reads at 0.1x, 5-minute writes at 1.25x, 1-hour writes at 2.0x.
    /// Those multipliers are identical across the price list, so mixed routing
    /// cannot skew the ratio and no price table has to be current for it to
    /// hold. Ours is billed; stock is modelled, and
    /// `predicted_read_error_pct` says how much to trust the model.
    pub ours_effective_tokens: u64,
    pub stock_effective_tokens: u64,
    /// Turns where both arms could be priced. A turn with no billed usage is
    /// in neither.
    pub stock_turns_compared: u64,
    /// `(1 - ours/stock) * 100`. Positive means we cost less than stock would
    /// have. It can exceed nothing in particular: it is bounded above by 100
    /// (free) and unbounded below, so a negative reading is a real regression
    /// and not a scaling artefact. `0.0` until a turn has been compared.
    pub vs_stock_saving_pct: f64,

    /// The same comparison over the last [`RECENT_SAMPLE_CAPACITY`] compared
    /// turns instead of all of them, and `None` until the first one.
    ///
    /// Prefer this to the lifetime figure when the question is "how is the
    /// proxy doing". An ordinary turn -- nothing compressed away, hot zone
    /// unmoved -- prices almost identically on both arms, so it pulls the
    /// lifetime ratio toward the marginal rate no matter what came before. The
    /// lifetime figure therefore decays toward the recent one in any long
    /// session, which reads as a slide even when nothing has got worse.
    pub vs_stock_saving_pct_recent: Option<f64>,
    /// How many turns are in that window, so a reader can weigh it.
    pub vs_stock_turns_recent: usize,
    /// The stock arm's one modelled rule -- "next turn reads back as much of
    /// the last prompt as still fits" -- scored every turn against our own
    /// observed reads, where the answer is billed rather than assumed. Read
    /// as: the counterfactual is good to about this much. Absolute error over
    /// observed reads, so it does not cancel.
    pub predicted_read_error_pct: f64,
    /// `earned / (earned + unearned)`, as a percentage, over every cache-write
    /// token seen since the process started.
    ///
    /// Writes only. Cache *reads* outnumber writes about fifty to one, so
    /// folding them in would pin this near 100% and it would never move — and a
    /// number that never moves does not earn a statusline slot. Writes are the
    /// tokens the proxy had a choice about, so they are the ones to watch.
    ///
    /// `100.0` before anything has been written, so a fresh process does not
    /// open by reporting total waste.
    pub productive_write_pct: f64,
    pub first_turn_writes_total: u64,
    pub first_turn_write_tokens_total: u64,
    /// The subset whose stated reason contradicts what the turn did: a
    /// `fresh_session` that read cache, or an `arrived_with_history` that read
    /// none. Neither is a cold start — both are a live conversation rebuilding
    /// itself under a new key — and both were filed as ordinary first turns.
    pub first_turn_contradictions_total: u64,
    pub last_event: Option<RecacheEvent>,
    /// Convenience for statusline scripts: seconds since
    /// `last_event`, `null` when no event has occurred.
    pub last_event_age_seconds: Option<u64>,
    /// Billed usage over the same window as `recent_hit_rate`. The read/write
    /// split is what the hit rate averages; these are the totals behind it, so
    /// a caller can price the window instead of only ranking it.
    pub recent_cache_read_tokens: u64,
    pub recent_cache_write_tokens: u64,
    pub recent_forwarded_bytes: u64,
    /// Billed fresh-equivalents per KB actually put on the wire — reads at 0.1x,
    /// writes at 1.25x. Tokens, not dollars. `null` until a turn with a known
    /// forwarded size lands.
    pub recent_cost_per_forwarded_kb: Option<f64>,
}

/// One turn's billed usage, kept only long enough to average. The statusline
/// needs the read/write split and the cost of a forwarded KB in the same window
/// the hit rate already covers, and the log is the wrong place to ask — it would
/// mean re-parsing megabytes on every render.
struct CostSample {
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    forwarded_bytes: u64,
    billed_fresh_equivalents: f64,
}

/// One turn's contribution to the fleet-wide hit-rate window.
struct RecentHitRateSample {
    rate: f64,
    /// False when the provider turn reported no cache-usage data at all (no
    /// cache fields in the usage block): "no signal", not a cache miss.
    cache_capable: bool,
}

struct Inner {
    pending: LruCache<String, PendingRequest>,
    /// Several streams can share one key — see [`match_stream`].
    conversations: LruCache<String, Vec<TurnRecord>>,
    /// Conversations `conversations` has evicted, so a turn that comes back
    /// after eviction is not silently taken for a first turn.
    ///
    /// Without this the two are the same observation: `get` returns `None`
    /// either way, an empty stream list goes in, `match_stream` finds nothing,
    /// and the turn is booked `FirstTurn` — whose whole point is that its
    /// cache write is not waste. A real recache then leaves the totals
    /// untouched and no line in the log, because `cache_stream_unmatched`
    /// only fires when the stream list is not empty. The classification stays
    /// as it was, since the earlier turn really is gone and inventing waste
    /// would be worse; what changes is that the undercount can be seen.
    forgotten: LruCache<String, ()>,
    /// Message-0 hash of each recent first turn → `(seen, conversation_key)`.
    first_turn_openers: LruCache<String, (Instant, String)>,
    recent_hit_rates: VecDeque<RecentHitRateSample>,
    recent_cost_samples: VecDeque<CostSample>,
    recache_events_total: u64,
    recache_wasted_tokens_total: u64,
    ttl_expiries_total: u64,
    earned_cache_write_tokens_total: u64,
    unearned_cache_write_tokens_total: u64,
    unearned_write_turns_total: u64,
    abandoned_requests_total: u64,
    concurrency_sheds_total: u64,
    hot_zone_changes_total: u64,
    hot_zone_recaches_total: u64,
    stabilization_absorbed_total: u64,
    stabilization_absorbed_tokens_total: u64,
    /// The stock arm's cached prefix lives on [`TurnRecord::stock_footprint`],
    /// one per tracked stream: same-lane subagent streams sharing a
    /// conversation key must price against their own prefix, not each other's.
    /// Input-equivalent tokens billed to us, and modelled for a plain client.
    ours_effective_tokens: f64,
    stock_effective_tokens: f64,
    stock_turns_compared: u64,
    /// The same two arms per turn, last [`RECENT_SAMPLE_CAPACITY`] only.
    ///
    /// The cumulative ratio answers "since this process started", which is the
    /// wrong question for a statusline: an ordinary turn contributes almost
    /// identically to both arms, so every one of them drags the lifetime figure
    /// toward the marginal rate and averages away whatever happened early. A
    /// window says what the proxy is doing *now*, which is what someone reading
    /// a statusline is asking.
    recent_vs_stock: VecDeque<(f64, f64)>,
    /// Self-check for the stock arm's one modelled rule, run against our own
    /// observed reads.
    predicted_read_tokens: u64,
    observed_read_tokens: u64,
    predicted_read_abs_error: u64,
    first_turn_writes_total: u64,
    first_turn_write_tokens_total: u64,
    first_turn_contradictions_total: u64,
    /// Turns booked `FirstTurn` only because their conversation had been
    /// evicted. The floor under any waste figure this observer reports.
    forgotten_conversations_total: u64,
    last_event: Option<RecacheEvent>,
}

/// Shared observer, one per proxy process (lives on `AppState`).
pub struct UsageObserver {
    /// TTL the forwarded body actually pins, used to tell a legitimate cache
    /// expiry apart from a bust. Defaults to the 5-minute tier; set it to match
    /// `--force-1h-cache-ttl` or every bust in a 5m..1h gap is filed as
    /// "expected" and vanishes from the numbers.
    cache_ttl: Duration,
    inner: Mutex<Inner>,
}

impl Default for UsageObserver {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`UsageObserver::complete`] decided about a turn, handed back so the
/// caller can persist it. The observer's own counters live in memory and reset
/// on restart; these are the ones worth keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionClass {
    /// Idle gap exceeded the cache TTL. Benign — nothing was wasted that
    /// staying warm could have saved.
    TtlExpiry,
    /// Bytes inside the cached prefix changed and the provider re-created it.
    /// This is the one that means we (or the client) moved something.
    PrefixChange { wasted_tokens: u64 },
    /// A stored prefix reached the wire but the provider did not reuse its
    /// expected cache footprint. The detailed boundary cause lives in the
    /// recache event; the durable three-bucket schema retains it as unknown.
    UnexplainedAfterReplay { wasted_tokens: u64 },
    /// A re-cache with no direct causal evidence.
    Unknown,
}

impl CompletionClass {
    /// `(reason, wasted_tokens)` in the vocabulary the durable metrics use.
    /// Only a structural bust reports waste: a TTL expiry cost nothing that
    /// staying warm could have saved, and an unattributed re-cache is counted
    /// but not charged.
    pub fn as_record(self) -> (&'static str, i64) {
        match self {
            CompletionClass::TtlExpiry => ("ttl_expiry", 0),
            CompletionClass::PrefixChange { wasted_tokens } => {
                ("prefix_change", wasted_tokens.min(i64::MAX as u64) as i64)
            }
            CompletionClass::UnexplainedAfterReplay { wasted_tokens } => {
                ("unknown", wasted_tokens.min(i64::MAX as u64) as i64)
            }
            CompletionClass::Unknown => ("unknown", 0),
        }
    }
}

impl UsageObserver {
    /// Pin the TTL the classifier assumes. Must match what the forwarded body
    /// carries, not what Anthropic defaults to.
    #[must_use]
    pub fn with_cache_ttl(mut self, cache_ttl: Duration) -> Self {
        self.cache_ttl = cache_ttl;
        self
    }

    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                pending: LruCache::new(
                    NonZeroUsize::new(PENDING_CAPACITY).expect("capacity is non-zero"),
                ),
                conversations: LruCache::new(
                    NonZeroUsize::new(CONVERSATION_CAPACITY).expect("capacity is non-zero"),
                ),
                forgotten: LruCache::new(
                    NonZeroUsize::new(CONVERSATION_CAPACITY).expect("capacity is non-zero"),
                ),
                first_turn_openers: LruCache::new(
                    NonZeroUsize::new(FIRST_TURN_OPENER_CAPACITY).expect("capacity is non-zero"),
                ),
                recent_hit_rates: VecDeque::with_capacity(RECENT_SAMPLE_CAPACITY),
                recent_cost_samples: VecDeque::with_capacity(RECENT_SAMPLE_CAPACITY),
                recache_events_total: 0,
                earned_cache_write_tokens_total: 0,
                unearned_cache_write_tokens_total: 0,
                unearned_write_turns_total: 0,
                abandoned_requests_total: 0,
                concurrency_sheds_total: 0,
                hot_zone_changes_total: 0,
                hot_zone_recaches_total: 0,
                stabilization_absorbed_total: 0,
                stabilization_absorbed_tokens_total: 0,
                ours_effective_tokens: 0.0,
                stock_effective_tokens: 0.0,
                stock_turns_compared: 0,
                recent_vs_stock: VecDeque::with_capacity(RECENT_SAMPLE_CAPACITY),
                predicted_read_tokens: 0,
                observed_read_tokens: 0,
                predicted_read_abs_error: 0,
                first_turn_writes_total: 0,
                first_turn_write_tokens_total: 0,
                first_turn_contradictions_total: 0,
                recache_wasted_tokens_total: 0,
                ttl_expiries_total: 0,
                forgotten_conversations_total: 0,
                last_event: None,
            }),
            cache_ttl: ANTHROPIC_CACHE_TTL,
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
    ///
    /// Takes the raw `session_key` and hashes it here rather than accepting a
    /// pre-computed hash. Callers used to pass the digest, and one of them
    /// passed `hash(conversation_key)` — a different value that joined to
    /// nothing. There is now no way to hand this the wrong hash.
    pub fn begin_request(
        &self,
        request_id: &str,
        conversation_key: String,
        session_key: Option<&str>,
        drift_dims: Option<String>,
        prefix: Option<PrefixFingerprint>,
    ) {
        let mut inner = self.lock();
        // Anything else already running under this key means the provider may
        // not have committed that turn's cache write yet.
        let now = Instant::now();
        let mut concurrent_with_in_flight = false;
        let mut abandoned = Vec::new();
        for (id, p) in inner.pending.iter() {
            let age = now.duration_since(p.began);
            if age >= IN_FLIGHT_HORIZON {
                // Past the horizon nothing is going to complete it. Sweeping
                // here rather than waiting for the LRU to push it out is what
                // makes the count prompt: at 512 slots against a day of eight
                // thousand turns, eviction alone would report zero for hours
                // after the leak started.
                abandoned.push(id.clone());
            } else if p.conversation_key == conversation_key {
                concurrent_with_in_flight = true;
            }
        }
        for id in abandoned {
            inner.pending.pop(&id);
            inner.abandoned_requests_total += 1;
        }
        // `push` reports what fell off the end; `put` does not, and the
        // eviction is the signal. A pending entry only leaves this cache two
        // ways: `complete` takes it, or it is pushed out unfinished. The
        // second is a request the proxy forwarded and never booked — the seam
        // between what was sent and what the books know about. On 2026-09-07
        // that was 132 of 8,209 turns, and finding it took a log-mining script
        // because nothing counted it.
        let evicted = inner.pending.push(
            request_id.to_string(),
            PendingRequest {
                began: now,
                concurrent_with_in_flight,
                conversation_key,
                first_turn: None,
                adoption: None,
                session_key_hash: session_key.map(super::drift_detector::session_key_log_prefix),
                drift_dims,
                outbound_drift_dims: None,
                replay_skip: None,
                replay_applied: None,
                compression: None,
                client_request_bytes: None,
                forwarded_request_bytes: None,
                compression_mode: None,
                prefix,
                billed_totals: None,
                billed_output: None,
                client_ttl: super::cache_ttl::ClientTtl::Unmarked,
            },
        );
        if let Some((evicted_id, _)) = evicted {
            if evicted_id != request_id {
                inner.abandoned_requests_total += 1;
            }
        }
    }

    /// Shed this turn when its conversation already has more than `cap` turns
    /// in flight, returning the in-flight count (this turn included) so the
    /// caller can say what it saw. `cap == 0` disables the check and always
    /// returns `None`.
    ///
    /// Overlapping turns of one conversation race the provider's cache commit:
    /// measured 2026-09-09, 43 overlapping turns on one fan-out burned 27.8k
    /// tokens re-writing prefixes their siblings had not finished committing
    /// (14% of everything that conversation wrote). Shedding paces the fan-out
    /// with the client's own retry instead of paying the race on every turn.
    /// Ordinary interactive overlap (one or two in flight) never reaches a cap
    /// worth setting, so only storms trip it.
    ///
    /// A shed turn is popped, not completed: nothing was forwarded, so unlike
    /// an abandoned request there is no seam — it must not count there, and a
    /// later turn of the conversation must not read it as concurrent. Counted
    /// in `concurrency_sheds_total` instead. Atomic under one lock with the
    /// count, so two turns arriving together cannot both pass on each other's
    /// stale view; the residual race (both count, one sheds) errs toward one
    /// extra client backoff, never toward an uncounted overlap.
    pub fn shed_if_over_conversation_cap(
        &self,
        request_id: &str,
        conversation_key: &str,
        cap: usize,
    ) -> Option<usize> {
        if cap == 0 {
            return None;
        }
        let mut inner = self.lock();
        let now = Instant::now();
        let in_flight = inner
            .pending
            .iter()
            .filter(|(_, p)| {
                p.conversation_key == conversation_key
                    && now.duration_since(p.began) < IN_FLIGHT_HORIZON
            })
            .count();
        if in_flight <= cap {
            return None;
        }
        // Count only a real shed: if this id never parked (compression-off,
        // non-JSON, eviction race), popping books a phantom.
        if inner.pending.pop(request_id).is_none() {
            return Some(in_flight);
        }
        inner.concurrency_sheds_total += 1;
        Some(in_flight)
    }

    /// Record what the provider billed across every round of this request.
    ///
    /// Call this only when the proxy issued hidden continuation rounds, and
    /// before [`UsageObserver::complete`]. Classification still runs on the
    /// client baseline `complete` is given; only the cost ledger uses these
    /// totals, so the ledger and the pricing counterfactual agree on one
    /// request's billed usage.
    /// Test hook: whether `request_id` began while another turn of its
    /// conversation was still in flight.
    #[cfg(test)]
    fn pending_is_concurrent(&self, request_id: &str) -> Option<bool> {
        self.lock()
            .pending
            .peek(request_id)
            .map(|p| p.concurrent_with_in_flight)
    }

    /// Test hook: pretend `request_id` began `by` earlier than it did.
    #[cfg(test)]
    fn age_pending(&self, request_id: &str, by: Duration) {
        if let Some(p) = self.lock().pending.peek_mut(request_id) {
            p.began -= by;
        }
    }

    /// Test hook: pretend this conversation's last turn was `by` earlier, so
    /// the next one lands after an idle gap.
    #[cfg(test)]
    fn age_conversation(&self, conversation_key: &str, by: Duration) {
        if let Some(turns) = self.lock().conversations.peek_mut(conversation_key) {
            for turn in turns.iter_mut() {
                turn.at -= by;
            }
        }
    }

    pub fn note_billed_totals(
        &self,
        request_id: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.billed_totals = Some((
                input_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            ));
        }
    }

    /// Record the turn's completion count.
    ///
    /// Apart from `note_billed_totals` because that one only fires for
    /// multi-round CCR turns, and output belongs on every turn: the ledger
    /// line is the only per-turn record on disk for a turn that saved nothing,
    /// and turn cost cannot be rebuilt from it without the output side.
    pub fn note_output_tokens(&self, request_id: &str, output_tokens: u64) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.billed_output = Some(output_tokens);
        }
    }

    /// Record the wire sizes and the arm this turn ran under.
    ///
    /// Deliberately taken from the bytes themselves rather than from any
    /// component's opinion of what it achieved.
    pub fn note_wire_bytes(
        &self,
        request_id: &str,
        client_bytes: u64,
        forwarded_bytes: u64,
        compression_mode: &'static str,
    ) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.client_request_bytes = Some(client_bytes);
            pending.forwarded_request_bytes = Some(forwarded_bytes);
            pending.compression_mode = Some(compression_mode);
        }
    }

    /// Record the TTL shape the client asked for on its own markers, read
    /// before any rewrite. Prices the stock arm at the tier this turn
    /// actually bought: main-loop traffic arrives on `1h`, subagent traffic
    /// on the 5-minute default, and a single flat assumption fits neither.
    /// Also the record the subagent TTL pin reads back at the pin site.
    pub fn note_client_cache_ttl(&self, request_id: &str, ttl: super::cache_ttl::ClientTtl) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.client_ttl = ttl;
        }
    }

    /// What the client asked for on a turn already past the gate, if it got
    /// that far. `None` falls back to pinning: a turn the observer never
    /// saw is not a turn to change TTL behaviour on.
    pub fn client_ttl_for(&self, request_id: &str) -> Option<super::cache_ttl::ClientTtl> {
        let inner = self.lock();
        inner.pending.peek(request_id).map(|p| p.client_ttl)
    }

    /// Record what this turn's compression removed, so the response side can
    /// price it.
    ///
    /// Answering "is the proxy worth running" needs the saving and the billed
    /// usage in the same place. They are produced on opposite sides of the
    /// request and were only ever joinable by correlating two log events on
    /// `request_id` after the fact — which is why the question stayed open as
    /// long as it did. Parking the pair here lets [`UsageObserver::complete`]
    /// emit one line that already contains the answer.
    pub fn note_compression(&self, request_id: &str, tokens_before: u64, tokens_after: u64) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.compression = Some((tokens_before, tokens_after));
        }
    }

    /// Record that prefix replay declined on this turn.
    ///
    /// Some replay declines are the missing cause for a whole class of
    /// re-cache events. The
    /// `drift_dims` that classify an event cover `system`, `tools` and the
    /// first three messages only, so a prefix that diverges deeper is invisible
    /// to them and the event falls through to [`RecacheEventKind::Expected`] —
    /// "no cause found", which the analysis then writes off as a session reset
    /// and excludes from waste. Measured over the 2026-08-08/09 logs, **98% of
    /// the tokens in that supposedly-benign bucket are turns where replay was
    /// declined**: 8.39M of 8.52M. Only mismatch reasons are causal evidence;
    /// `no_previous_turn` and unrecognised values are retained for diagnostics
    /// but do not attribute a re-cache.
    pub fn note_replay_skip(&self, request_id: &str, evidence: ReplaySkipEvidence) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.replay_skip = Some(evidence);
        }
    }

    /// Record a prefix replay only after the rewritten body serialized
    /// successfully, so this evidence describes bytes that reached upstream.
    /// Park what the handler knows about this turn's opener, so a first turn
    /// that writes cache can say why — see `first_turn_write_observed`.
    pub fn note_first_turn_context(&self, request_id: &str, ctx: FirstTurnContext) {
        let mut inner = self.lock();
        if let Some(p) = inner.pending.get_mut(request_id) {
            p.first_turn = Some(ctx);
        }
    }

    /// Record that the cross-session adoption path found a donor for this
    /// request, whether or not its prefix was used.
    pub fn note_prefix_adoption(&self, request_id: &str, adoption: PrefixAdoption) {
        let mut inner = self.lock();
        if let Some(p) = inner.pending.get_mut(request_id) {
            p.adoption = Some(adoption);
        }
    }

    pub fn note_replay_applied(&self, request_id: &str, evidence: ReplayAppliedEvidence) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.replay_applied = Some(evidence);
        }
    }

    /// Record drift measured on the body the proxy is about to send.
    ///
    /// `begin_request` carries the drift the *client* caused, measured before
    /// the proxy touches anything. That is the only drift the detector could
    /// ever see, so every recache the proxy inflicted on itself landed in the
    /// residual bucket — then `unexplained_after_replay`, 85% of events, now
    /// the `provider_*` reasons of [`CacheLanding`] — with the classifier
    /// structurally unable to say whether it was to blame.
    ///
    /// Hashing the outbound body closes that: the same hot zone, the same
    /// comparison, one turn later in the pipeline. Inbound quiet plus outbound
    /// drift means the mutation was ours.
    pub fn note_outbound_drift(&self, request_id: &str, dims: Option<String>) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.outbound_drift_dims = dims;
        }
    }

    /// Response side: classify this turn's billed usage against the
    /// conversation's previous turn. Call ONLY for cleanly completed
    /// streams (`message_stop`) — half-finished usage would classify
    /// garbage.
    /// Returns what this turn was classified as, so a caller that can reach
    /// durable storage can persist it. The observer deliberately holds no
    /// reference to the savings tracker — it is a pure in-process watchdog,
    /// and its counters reset on restart — so the caller does the writing.
    ///
    /// `cache_write_ttl_split` is the `(5m, 1h)` breakdown of
    /// `cache_creation_input_tokens`, when the provider published one. It rides
    /// in the signature rather than in a `note_*` call because it is part of
    /// the same billed `usage` block as the three counters beside it, and
    /// pricing already reads it: a 5-minute write costs 1.25x input, a 1-hour
    /// write 2.0x. `None` means the caller's provider has no such field, which
    /// is not the same as a turn that wrote nothing at the 1-hour tier.
    pub fn complete(
        &self,
        request_id: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_write_ttl_split: Option<(u64, u64)>,
    ) -> Option<CompletionClass> {
        self.complete_with_cache_capability(
            request_id,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_write_ttl_split,
            true,
        )
    }

    /// Same as `complete`, for providers that may not report cache usage.
    ///
    /// Pass `cache_capable: false` when the turn's usage block carried no
    /// cache fields at all. The sample still joins the window (so capacity
    /// accounting is unchanged) but `snapshot` leaves it out of the
    /// `recent_hit_rate` mean. A capable turn that simply read nothing from
    /// cache still counts as a genuine 0%.
    pub fn complete_with_cache_capability(
        &self,
        request_id: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_write_ttl_split: Option<(u64, u64)>,
        cache_capable: bool,
    ) -> Option<CompletionClass> {
        let now = SystemTime::now();
        let now_instant = Instant::now();
        let cache_ttl = self.cache_ttl;
        let mut inner = self.lock();

        // Fleet-wide rolling hit rate (statusline ambient signal).
        let denom = input_tokens
            .saturating_add(cache_read_input_tokens)
            .saturating_add(cache_creation_input_tokens);
        if denom > 0 {
            if inner.recent_hit_rates.len() == RECENT_SAMPLE_CAPACITY {
                inner.recent_hit_rates.pop_front();
            }
            inner.recent_hit_rates.push_back(RecentHitRateSample {
                rate: cache_read_input_tokens as f64 / denom as f64,
                cache_capable,
            });
        }

        let Some(pending) = inner.pending.pop(request_id) else {
            // Request never went through the compression gate
            // (compression off, non-JSON, …) — no conversation
            // identity, so no per-turn classification. The rolling
            // rate above still counted it.
            return None;
        };

        // Price this turn's saving against the usage actually billed for it.
        //
        // A token removed from the request is worth what it *would have cost*,
        // and on a cached workload that is not one number. Tokens inside the
        // cached prefix bill at the cache-read rate; tokens past it bill at the
        // cache-write or fresh-input rate, which is over 12x more. Reporting a
        // saving without saying which it was overstates it by that factor —
        // item 10, and the reason the headline figure read 10x high.
        //
        // The live zone is the request's tail, so when its forwarded tokens fit
        // inside the newly-written plus uncached region, the whole compressed
        // span sits past the cache boundary. The test is deliberately generous
        // to the proxy: blocks inside the cached region that do not overflow
        // that budget are counted as outside, so `freed_past_cache_boundary`
        // is an upper bound on the valuable share, never an overstatement of
        // the cheap one.
        if let Some((tokens_before, tokens_after)) = pending.compression {
            let freed = tokens_before.saturating_sub(tokens_after);
            if freed > 0 {
                let fresh_region = cache_creation_input_tokens.saturating_add(input_tokens);
                let past_boundary = tokens_after <= fresh_region;
                tracing::info!(
                    event = "savings_placement",
                    request_id = %request_id,
                    conversation_key = %pending.conversation_key,
                    tokens_freed = freed,
                    live_zone_forwarded_tokens = tokens_after,
                    cache_read_input_tokens = cache_read_input_tokens,
                    cache_creation_input_tokens = cache_creation_input_tokens,
                    input_tokens = input_tokens,
                    // true  → the freed tokens would have been billed at the
                    //         cache-write / fresh-input rate (the valuable case)
                    // false → they sat in the cached prefix and would have been
                    //         billed at the cache-read rate, worth ~1/12th
                    freed_past_cache_boundary = past_boundary,
                    "compression saving priced against the usage billed for this turn"
                );
            }
        }

        // ── Ground-truth ledger ───────────────────────────────────────────
        //
        // Every savings number this proxy reports is produced by the component
        // doing the saving: the compressor states how many tokens it removed,
        // and the placement test that prices them was written to be generous.
        // Self-reported metrics are exactly the ones to distrust, so this line
        // is deliberately built from figures the proxy cannot influence — the
        // `usage` block Anthropic returns, which is the bill.
        //
        // `billed_fresh_equivalents` restates that bill in one comparable unit,
        // weighting each class by its published price relative to fresh input:
        // cache reads cost a tenth, cache writes a quarter more. Divided by the
        // bytes the client asked us to send, it gives cost per unit of work
        // requested — a ratio that falls only if the proxy genuinely helps, and
        // that no amount of favourable accounting on our side can move.
        //
        // It is NOT a savings figure. It is the denominator-free number to
        // compare between a run with compression on and one with it off; see
        // `docs/measurement.md`. Reading it alone proves nothing.
        {
            // Bill every round, not just the client's. When the proxy answered
            // a retrieval itself, the rounds it added were billed too, and the
            // baseline above deliberately excludes them.
            let (billed_input, billed_cache_read, billed_cache_write) =
                pending.billed_totals.unwrap_or((
                    input_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                ));
            let billed_fresh_equivalents = billed_input as f64
                + (billed_cache_read as f64 * 0.1)
                + (billed_cache_write as f64 * 1.25);
            // Same window as the hit rate above, and the same reason: the
            // statusline needs it per render and cannot afford to re-read the
            // log. Kept here rather than beside the hit rate because the
            // forwarded size lives on `pending`, which only exists past the
            // gate — a turn that never reached compression has no size to
            // divide by and would price as free.
            if inner.recent_cost_samples.len() == RECENT_SAMPLE_CAPACITY {
                inner.recent_cost_samples.pop_front();
            }
            inner.recent_cost_samples.push_back(CostSample {
                cache_read_tokens: billed_cache_read,
                cache_write_tokens: billed_cache_write,
                forwarded_bytes: pending.forwarded_request_bytes.unwrap_or(0),
                billed_fresh_equivalents,
            });
            tracing::info!(
                event = "turn_cost_ledger",
                request_id = %request_id,
                conversation_key = %pending.conversation_key,
                // Anthropic's own numbers, summed over every round the proxy
                // ran and otherwise unmodified.
                input_tokens = billed_input,
                cache_read_input_tokens = billed_cache_read,
                cache_creation_input_tokens = billed_cache_write,
                // Which TTL the provider actually billed the write at. The
                // proxy asks for the 1-hour tier on the prefix, but asking is
                // not granting, and the flat creation count above cannot tell
                // the two apart — a 1-hour write costs 2.0x input against the
                // 5-minute tier's 1.25x, so a silently downgraded request is a
                // price change the ledger would otherwise miss. `-1` where the
                // provider publishes no breakdown, so "absent" stays distinct
                // from "wrote nothing at that tier".
                cache_write_5m_tokens = cache_write_ttl_split.map_or(-1_i64, |(m5, _)| m5 as i64),
                cache_write_1h_tokens = cache_write_ttl_split.map_or(-1_i64, |(_, h1)| h1 as i64),
                // `-1` where the path that booked this turn never reported an
                // output count, same convention as the TTL split above.
                output_tokens = pending.billed_output.map_or(-1_i64, |o| o as i64),
                billed_fresh_equivalents = billed_fresh_equivalents,
                // What the client handed us, before anything we did.
                client_request_bytes = pending.client_request_bytes.unwrap_or(0),
                forwarded_request_bytes = pending.forwarded_request_bytes.unwrap_or(0),
                // The arm this turn ran under, so on/off runs are separable.
                compression_mode = pending.compression_mode.unwrap_or("unknown"),
                "billed usage against the work the client asked for"
            );
        }

        // Classify against the stream this turn continues, not against
        // whatever turn happened to arrive last under the same key.
        let turn_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs);
        // `head` is the hex of eight digest bytes (see `hex16`), so it reads
        // back as the `u64` the record holds.
        let turn_head = pending
            .prefix
            .as_ref()
            .and_then(|p| u64::from_str_radix(&p.head, 16).ok());
        let (
            class,
            expected_cache_read,
            idle_gap,
            previous_forwarded_request_bytes,
            previous_turn_diverged,
            previous_cache_read,
            previous_previous_boundary,
            head_changed,
            matched_stream_msgs,
            streams_tracked,
            matched_stream_idx,
            matched_stock_prior,
        ) = {
            if inner.conversations.get(&pending.conversation_key).is_none() {
                if inner.forgotten.pop(&pending.conversation_key).is_some() {
                    inner.forgotten_conversations_total += 1;
                    tracing::warn!(
                        event = "cache_conversation_forgotten",
                        conversation_key = %pending.conversation_key,
                        capacity = CONVERSATION_CAPACITY,
                        forgotten_total = inner.forgotten_conversations_total,
                        "conversation evicted before its next turn; booked as a first turn, \
                         so any cache write it just paid for goes uncounted"
                    );
                }
                // `push` reports what fell off the end; `put` does not, and the
                // eviction is the whole signal.
                if let Some((evicted, _)) = inner
                    .conversations
                    .push(pending.conversation_key.clone(), Vec::new())
                {
                    if evicted != pending.conversation_key {
                        inner.forgotten.put(evicted, ());
                    }
                }
            }
            let streams = inner
                .conversations
                .get_mut(&pending.conversation_key)
                .expect("just inserted");
            let matched = match_stream(streams, turn_msgs);
            // A turn shorter than every tracked stream matches nothing and is
            // filed `FirstTurn`, which reports no waste however much the
            // provider re-wrote. That is right for a subagent forking off a
            // shared opener — it had no prefix to reuse — and wrong for
            // anything that shortened a conversation it meant to continue.
            //
            // The two are indistinguishable from here, so this does not guess:
            // it makes the case countable. Silence was the problem; a turn that
            // re-wrote a large prefix and reported nothing looked identical to
            // a turn that cost nothing.
            if matched.is_none() && !streams.is_empty() {
                tracing::info!(
                    event = "cache_stream_unmatched",
                    request_id = %request_id,
                    conversation_key = %pending.conversation_key,
                    turn_msgs = turn_msgs.unwrap_or(0),
                    longest_tracked = streams.iter().filter_map(|r| r.msgs).max().unwrap_or(0),
                    streams_tracked = streams.len(),
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    "turn was shorter than every tracked stream; booked as a \
                     first turn, so its cache write is not counted as waste"
                );
            }
            // Which stream this turn was paired against, carried out so the
            // booking event can name it. Without this a recache says only that
            // the numbers did not add up, never which prefix the arithmetic
            // was done against — and with up to 8 streams per key, that is the
            // difference between a finding and an argument.
            let matched_stream_msgs = matched.and_then(|i| streams[i].msgs);
            let streams_tracked = streams.len();
            // The stock arm's prior is this stream's own footprint, not the
            // conversation's last write: sibling streams sharing a key must
            // not price against each other. A new lineage starts at 0.
            let matched_stock_prior = matched.map(|i| streams[i].stock_footprint).unwrap_or(0);
            let outcome = match matched {
                None => (
                    TurnClass::FirstTurn,
                    0,
                    Duration::ZERO,
                    None,
                    false,
                    0,
                    None,
                    false,
                ),
                Some(i) => {
                    let prev = streams[i];
                    (
                        classify_turn(
                            &prev,
                            now,
                            input_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                            cache_ttl,
                        ),
                        prev.cache_read_input_tokens
                            .saturating_add(prev.cache_creation_input_tokens),
                        // How long this stream sat idle. On a TTL expiry it is
                        // the whole story: a five-minute-plus gap means the
                        // provider's cache died on its own.
                        now.duration_since(prev.at).unwrap_or(Duration::ZERO),
                        prev.forwarded_request_bytes,
                        prev.diverged,
                        prev.cache_read_input_tokens,
                        prev.previous_boundary,
                        // Both sides known and different. An unknown head on
                        // either side is not comparable, and reporting a change
                        // from it would blame the client for a missing
                        // measurement.
                        matches!((prev.head, turn_head), (Some(p), Some(c)) if p != c),
                    )
                }
            };
            let record = TurnRecord {
                cache_read_input_tokens,
                cache_creation_input_tokens,
                at: now,
                forwarded_request_bytes: pending.forwarded_request_bytes,
                msgs: turn_msgs,
                // Read straight off the skip evidence rather than off the
                // attribution below, which is computed after this record is
                // stored. Same source either way: `recache_attribution` derives
                // `prefix_content_diverged` from this very field.
                diverged: pending
                    .replay_skip
                    .as_ref()
                    .is_some_and(|e| e.reason.as_str() == "prefix_content_diverged"),
                previous_boundary: matched.map(|i| {
                    streams[i]
                        .cache_read_input_tokens
                        .saturating_add(streams[i].cache_creation_input_tokens)
                }),
                head: turn_head,
                // Patched below once the stock arm prices this turn; 0 until
                // then so a turn that never reaches the stock arm (empty
                // prompt) leaves a rebuild, never a phantom hit.
                stock_footprint: 0,
            };
            // Index of the record just stored, carried out so the stock arm
            // can file this turn's footprint on the stream it priced.
            let matched_stream_idx = match matched {
                Some(i) => {
                    streams[i] = record;
                    i
                }
                None => {
                    if streams.len() >= MAX_STREAMS_PER_CONVERSATION {
                        if let Some(oldest) = streams
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, r)| r.at)
                            .map(|(i, _)| i)
                        {
                            streams.remove(oldest);
                        }
                    }
                    streams.push(record);
                    streams.len() - 1
                }
            };
            let (class, expected, gap, bytes, diverged, prev_read, prevprev_boundary, head_moved) =
                outcome;
            (
                class,
                expected,
                gap,
                bytes,
                diverged,
                prev_read,
                prevprev_boundary,
                head_moved,
                matched_stream_msgs,
                streams_tracked,
                matched_stream_idx,
                matched_stock_prior,
            )
        };

        // A healthy turn is the one class that reports nothing, and it is by
        // far the largest: 5,747 turns and 10,935,835 written tokens on
        // 2026-09-07, against 129 recache events. Most of that is the
        // breakpoint advancing over new content and is money well spent — but
        // it was indistinguishable from the rest, so split it and count both
        // sides. Only the unearned half is a savings candidate.
        // ---- what cache stabilisation is worth, measured rather than modelled
        //
        // The prefix `head` is hashed over the model, system and tools of the
        // body the *client* sent: `forward_http` takes the drift hash on the
        // held view but puts the client's own `system` back before this
        // observer ever sees it. So `head_changed` says the client's hot zone
        // moved -- the thing that re-caches a conversation from the system
        // block down, and the thing the holds exist to absorb.
        //
        // What happened next is observed, not assumed. A hot-zone change that
        // still read its cache is one the stabilisation absorbed; one that
        // re-cached is one it did not. Counting both gives the absorb rate and
        // the tokens, from real turns, with no counterfactual arm and no model
        // of the provider's cache.
        if head_changed {
            inner.hot_zone_changes_total += 1;
            match class {
                TurnClass::Healthy => {
                    inner.stabilization_absorbed_total += 1;
                    // Worth the footprint that would have been rebuilt, which
                    // is the previous turn's observed read plus write. Not an
                    // estimate of it -- the number Anthropic billed last turn.
                    inner.stabilization_absorbed_tokens_total += expected_cache_read;
                }
                TurnClass::Recache { .. } => {
                    inner.hot_zone_recaches_total += 1;
                }
                // A first turn has no cache to lose, and a TTL expiry would
                // have re-cached under any client. Neither says anything about
                // stabilisation, so neither is counted either way.
                TurnClass::FirstTurn | TurnClass::TtlExpiry => {}
            }
        }

        // --- the stock arm -------------------------------------------------
        //
        // What the same turn would have cost a plain Claude Code client: no
        // compression, no offload, no holds. It runs beside the real request
        // rather than instead of it, so there is no A/B split and no session
        // is ever served the worse arm.
        //
        // Three of the four inputs are measured, not modelled:
        //
        //   * our prompt is the billed `input + read + write` -- exact;
        //   * the size the client sent and the size we forwarded are the wire
        //     bytes from `note_wire_bytes` -- exact;
        //   * the verdict the provider handed down on our prefix is `class`.
        //
        // The one model is the stock client's cache behaviour, and it is the
        // simple one: Claude Code puts a breakpoint at the tail, so its whole
        // prompt is cacheable and the next turn reads back as much of it as
        // still fits. The tier and the horizon come from the turn's own
        // markers, read before any rewrite: hour-marked traffic prices and
        // expires like an hour entry, five-minute traffic like a five-minute
        // one. `predicted_read_error_pct` below measures that same rule
        // against our own observed reads every turn, which is what bounds how
        // far to trust this arm.
        let ours_prompt = input_tokens + cache_read_input_tokens + cache_creation_input_tokens;
        if ours_prompt > 0 {
            // Bytes to tokens by proportion. Both numbers are the same kind of
            // JSON measured at the same place, so the ratio carries over even
            // though neither side is a token count.
            let stock_prompt = match (
                pending.client_request_bytes,
                pending.forwarded_request_bytes,
            ) {
                (Some(sent), Some(fwd)) if fwd > 0 && sent > fwd => {
                    ((ours_prompt as f64) * (sent as f64) / (fwd as f64)).round() as u64
                }
                // Nothing was removed, or we cannot prove anything was: the
                // stock client would have sent what we sent.
                _ => ours_prompt,
            };

            // This stream's own prior footprint: sibling streams sharing one
            // conversation key (a main loop and its subagent fork) must not
            // price against each other. A new lineage starts at 0, which reads
            // as a full rebuild below — the same assumption the classifier
            // makes when it books the turn `FirstTurn`, on the grounds that a
            // fork had no prefix to reuse.
            let prior = matched_stock_prior;
            // Anything that busted our prefix would have busted theirs -- an
            // idle gap is idle for both, and a body edit is the client's own.
            // A hot-zone change is the case where the arms part: our holds may
            // absorb it, and without them it is a rebuild every time.
            //
            // The gap test is the second place they part. The horizon is the
            // tier this turn's own markers bought, read before any rewrite:
            // a gap we survived on an hour marker ends a five-minute
            // client's prefix (and it rebuilds), but an hour-marked stock
            // client survives it alongside us. Charge the cost, credit the
            // benefit, both at the tier the client actually asked for.
            let stock_tier = pending.client_ttl.stock_tier();
            let stock_horizon = stock_tier.horizon();
            let stock_kept =
                matches!(class, TurnClass::Healthy) && !head_changed && idle_gap <= stock_horizon;
            // The tail after the last breakpoint is billed as fresh input on
            // both arms. Which message the breakpoint lands on is the client's
            // shape, not ours, so handing the stock arm a cheaper tail than we
            // got would be inventing a difference the transforms did not make.
            let stock_cacheable = stock_prompt.saturating_sub(input_tokens);
            let stock_read = if stock_kept {
                prior.min(stock_cacheable)
            } else {
                0
            };
            let stock_write = stock_cacheable.saturating_sub(stock_read);
            // File the footprint on the stream just stored, so the next turn
            // of *this* stream reads its own prefix back. Keyed by position,
            // not by key: the index was taken from the same `Vec` above and
            // nothing between here and there touches it.
            if let Some(streams) = inner.conversations.peek_mut(&pending.conversation_key) {
                if let Some(rec) = streams.get_mut(matched_stream_idx) {
                    rec.stock_footprint = stock_read + stock_write;
                }
            }

            // Priced in input-equivalents rather than dollars: Anthropic's
            // multipliers (read 0.1x, 5-minute write 1.25x, 1-hour write 2.0x)
            // are the same for every model, so the ratio of the two arms holds
            // whatever was routed where, and no price table has to be right
            // for the comparison to be.
            let (w5, w1h) = match cache_write_ttl_split {
                Some((five, hour)) => (five, hour),
                None => (cache_creation_input_tokens, 0),
            };
            let mut ours_effective = input_tokens as f64
                + cache_read_input_tokens as f64 * CACHE_READ_MULTIPLIER
                + w5 as f64 * CACHE_WRITE_5M_MULTIPLIER
                + w1h as f64 * CACHE_WRITE_1H_MULTIPLIER;
            // Hidden continuation rounds were billed but are not in the client
            // baseline above, and the stock client never runs them — so the
            // stock arm must not include them, but ours must, or the
            // comparison reports less than the bill. The extra write prices at
            // the 5-minute rate, matching the ground-truth ledger below, which
            // prices every billed write the same way; the true cost can only
            // be higher (up to the 1-hour rate), never lower.
            let ccr_hidden_effective = match pending.billed_totals {
                Some((billed_input, billed_read, billed_write)) => {
                    billed_input.saturating_sub(input_tokens) as f64
                        + billed_read.saturating_sub(cache_read_input_tokens) as f64
                            * CACHE_READ_MULTIPLIER
                        + billed_write.saturating_sub(cache_creation_input_tokens) as f64
                            * CACHE_WRITE_5M_MULTIPLIER
                }
                None => 0.0,
            };
            ours_effective += ccr_hidden_effective;
            // The stock client pays the tier its own markers bought: hour
            // writes at 2.0x, five-minute writes at 1.25x.
            let stock_effective = input_tokens as f64
                + stock_read as f64 * CACHE_READ_MULTIPLIER
                + stock_write as f64 * stock_tier.write_multiplier();
            inner.ours_effective_tokens += ours_effective;
            inner.stock_effective_tokens += stock_effective;
            inner.stock_turns_compared += 1;
            if inner.recent_vs_stock.len() == RECENT_SAMPLE_CAPACITY {
                inner.recent_vs_stock.pop_front();
            }
            inner
                .recent_vs_stock
                .push_back((ours_effective, stock_effective));

            // One line per compared turn, with both arms broken into the parts
            // that priced them. The aggregate can only say that the two arms
            // diverged; it cannot say on which turns or through which term, and
            // a ratio nobody can decompose is a ratio nobody should act on.
            //
            // Read `stock_kept` first. When it is true the modelled client is
            // credited with a perfect read of its whole prior footprint and a
            // write of only the growth since last turn -- the best case
            // available to it -- while `ours_*` are what the provider actually
            // billed. Those turns are where the comparison is least fair to us,
            // so a persistent loss confined to them is a modelling artifact,
            // and one that shows up with `stock_kept = false` is real.
            tracing::info!(
                event = "vs_stock_turn",
                request_id = %request_id,
                conversation_key = %pending.conversation_key,
                turn_class = ?class,
                head_changed,
                stock_kept,
                client_ttl = ?pending.client_ttl,
                ours_effective = ours_effective.round() as u64,
                stock_effective = stock_effective.round() as u64,
                ours_input = input_tokens,
                ours_read = cache_read_input_tokens,
                ours_write_5m = w5,
                ours_write_1h = w1h,
                ccr_hidden_effective = ccr_hidden_effective.round() as u64,
                stock_read,
                stock_write,
                "priced this turn against a stock client"
            );

            // The self-check, on the arm where the answer is observable: the
            // stock model's rule, applied to our own previous footprint,
            // against what the provider actually read back. Reported as a
            // share of the reads it was predicting, so it stays readable as
            // "the counterfactual is good to about this much".
            let predicted_ours_read = expected_cache_read.min(ours_prompt);
            inner.predicted_read_tokens += predicted_ours_read;
            inner.observed_read_tokens += cache_read_input_tokens;
            inner.predicted_read_abs_error += predicted_ours_read.abs_diff(cache_read_input_tokens);
        }

        // Every completed turn that wrote anything, not just the healthy ones.
        // Gating on `Healthy` made this dead arithmetic: healthy means
        // `read + RECACHE_SLACK_TOKENS >= previous footprint`, which forces
        // `unearned <= RECACHE_SLACK_TOKENS` — under the warning floor, always.
        // The turns actually re-writing ground they already held are the ones
        // the gate threw away. Recache turns are counted here *and* by the
        // recache detector; the two measure different things (this one, tokens
        // re-written; that one, prefix not read) and must not be added up.
        if cache_creation_input_tokens > 0 {
            let (earned, unearned) = split_cache_write(
                expected_cache_read,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            );
            inner.earned_cache_write_tokens_total += earned;
            inner.unearned_cache_write_tokens_total += unearned;
            if unearned > 0 {
                inner.unearned_write_turns_total += 1;
            }
            if unearned > UNEARNED_WRITE_FLOOR_TOKENS {
                tracing::warn!(
                    event = "unearned_cache_write_observed",
                    request_id = %request_id,
                    turn_class = match class {
                        TurnClass::FirstTurn => "first_turn",
                        TurnClass::Healthy => "healthy",
                        TurnClass::TtlExpiry => "ttl_expiry",
                        TurnClass::Recache { .. } => "recache",
                    },
                    conversation_key = %pending.conversation_key,
                    session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    previous_footprint = expected_cache_read,
                    earned_tokens = earned,
                    unearned_tokens = unearned,
                    "cache write re-covered footprint the conversation already held"
                );
            }
        }
        // First completed turn under this key. The recache classifier has
        // nothing to score it against, so without this its cache write —
        // 41% of all write tokens, live — went unattributed.
        if streams_tracked == 0 {
            let ctx = pending.first_turn.clone().unwrap_or_default();
            let opener_seen_elsewhere = match ctx.message_zero_hash.as_deref() {
                Some(hash) => {
                    let seen = inner.first_turn_openers.get(hash).is_some_and(|(at, key)| {
                        *key != pending.conversation_key
                            && now_instant.duration_since(*at) < IDENTICAL_PROMPT_FANOUT_WINDOW
                    });
                    inner.first_turn_openers.put(
                        hash.to_string(),
                        (now_instant, pending.conversation_key.clone()),
                    );
                    seen
                }
                None => false,
            };
            if cache_creation_input_tokens > RECACHE_SLACK_TOKENS {
                let reason =
                    first_turn_reason(&ctx, pending.adoption.as_ref(), opener_seen_elsewhere);
                // A cold start writing cache is normal and stays uncharged.
                // Two shapes are not cold starts and were filed as if they
                // were: a `fresh_session` that read cache is not fresh, and an
                // `arrived_with_history` that read none is a live conversation
                // whose key moved under it with no compaction to explain the
                // move. On 2026-09-07 those two accounted for 549K of the
                // 2.73M written here, and nothing counted either.
                let contradicts_itself = (reason == "fresh_session" && cache_read_input_tokens > 0)
                    || (reason == "arrived_with_history" && cache_read_input_tokens == 0);
                inner.first_turn_writes_total += 1;
                inner.first_turn_write_tokens_total += cache_creation_input_tokens;
                if contradicts_itself {
                    inner.first_turn_contradictions_total += 1;
                }
                tracing::info!(
                    event = "first_turn_write_observed",
                    contradicts_itself,
                    request_id = %request_id,
                    conversation_key = %pending.conversation_key,
                    session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                    msgs = ctx.msgs,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    model = ctx.model.as_deref().unwrap_or(""),
                    attribution_reason = reason,
                    adopted = pending
                        .adoption
                        .as_ref()
                        .map(|_| pending.replay_applied.is_some()),
                    donor_session_key_hash = pending
                        .adoption
                        .as_ref()
                        .map(|a| a.donor_session_key_hash.as_str())
                        .unwrap_or(""),
                    "first turn under its conversation key wrote cache"
                );
                crate::observability::observe_first_turn_write(reason, cache_creation_input_tokens);
            }
        }

        match class {
            TurnClass::FirstTurn | TurnClass::Healthy => None,
            TurnClass::TtlExpiry => {
                inner.ttl_expiries_total += 1;
                record_cache_miss_attribution(MISS_ATTRIBUTION_PROVIDER, "ttl_expiry");
                // Raised from `debug!` deliberately. At the proxy's `info`
                // level this event could never appear, so its count read zero
                // whether TTL expiries happened constantly or never — and it
                // was quoted as evidence that they were not happening. A TTL
                // expiry is the *legitimate* cache loss: Anthropic's prefix
                // cache lives 5 minutes, so coming back to a session after a
                // break costs a full re-cache that is nobody's defect. Telling
                // that apart from a real bust is the difference between waste
                // the proxy caused and waste it merely witnessed.
                tracing::info!(
                    event = "cache_recache_ttl_expiry",
                    request_id = %request_id,
                    conversation_key = %pending.conversation_key,
                    session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                    // Which tracked stream the arithmetic was done against.
                    // `-1` = matched nothing, so this was booked a first turn.
                    matched_stream_msgs = matched_stream_msgs.map_or(-1_i64, |m| m as i64),
                    turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
                    streams_tracked = streams_tracked,
                    cache_creation_input_tokens = cache_creation_input_tokens,
                    idle_seconds = idle_gap.as_secs(),
                    "prefix re-written after cache TTL expiry (idle > 5 min); expected, not a defect"
                );
                Some(CompletionClass::TtlExpiry)
            }
            TurnClass::Recache { wasted_tokens } => {
                inner.recache_events_total += 1;
                let attribution = recache_attribution(
                    pending.drift_dims.as_deref(),
                    head_changed,
                    pending.outbound_drift_dims.as_deref(),
                    pending.replay_skip,
                    pending.replay_applied,
                    previous_turn_diverged,
                    pending.concurrent_with_in_flight,
                );
                // The residual is never left as "unexplained": the proxy's side
                // was stable, so the only thing left to name is where the
                // provider's read landed.
                let landing = CacheLanding::classify(
                    cache_read_input_tokens,
                    previous_cache_read,
                    expected_cache_read,
                    previous_previous_boundary,
                );
                let unexplained = attribution.reason == Some("unexplained_after_replay");
                let attribution = if unexplained {
                    RecacheAttribution {
                        reason: Some(landing.as_str()),
                        ..attribution
                    }
                } else {
                    attribution
                };
                let charged_wasted_tokens = if attribution.counts_as_waste {
                    wasted_tokens
                } else {
                    0
                };
                inner.recache_wasted_tokens_total += charged_wasted_tokens;
                // Tokens we charged as waste and could not name used to fall
                // through to `Expected`, which logs at INFO and reads as a
                // benign session reset. On 2026-09-07 that hid 1,471,795
                // tokens across 96 events — one conversation rebuilding its
                // own cache — behind a green statusline. Nothing that cost
                // real tokens may log below WARN. When attribution found no
                // cause, say that in the reason and hand over what the replay
                // decline knew, which the ranking above deliberately drops.
                let uncaused_waste = charged_wasted_tokens > 0 && attribution.reason.is_none();
                let attribution = if uncaused_waste {
                    RecacheAttribution {
                        reason: Some(
                            pending
                                .replay_skip
                                .map(|e| e.reason.as_str())
                                .unwrap_or("no_cause_found"),
                        ),
                        ..attribution
                    }
                } else {
                    attribution
                };
                let event_kind = if attribution.reason == Some("inbound_tail_replaced") {
                    RecacheEventKind::Branch
                } else if unexplained || uncaused_waste {
                    RecacheEventKind::Unexplained
                } else if attribution.reason.is_some() {
                    RecacheEventKind::Drift
                } else {
                    RecacheEventKind::Expected
                };
                // Python buckets every miss on an expected-cached prefix as
                // ttl_expiry / prefix_change / unknown, and `unknown` is the
                // fall-through: we expected a read, the content looked stable,
                // we cannot name the cause. `Expected` is the same measurement
                // — the extra reading that these are usually session resets is
                // a judgement made after the fact, and it already rides on the
                // log level and `RecacheEvent.event_kind`. Suppressing it here
                // would break `total = ttl_expiry + prefix_change + unknown`
                // and make the two named buckets look like the whole story.
                if event_kind != RecacheEventKind::Branch {
                    record_cache_miss_attribution(
                        MISS_ATTRIBUTION_PROVIDER,
                        match event_kind {
                            RecacheEventKind::Drift => "prefix_change",
                            RecacheEventKind::Unexplained | RecacheEventKind::Expected => "unknown",
                            RecacheEventKind::Branch => unreachable!("guarded above"),
                        },
                    );
                }
                let event = RecacheEvent {
                    at_unix: now
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or(Duration::ZERO)
                        .as_secs(),
                    conversation_key: pending.conversation_key.clone(),
                    session_key_hash: pending.session_key_hash.clone(),
                    drift_dims: pending.drift_dims.clone(),
                    attribution_reason: attribution.reason.map(str::to_owned),
                    origin: attribution.origin.map(str::to_owned),
                    scope: attribution.scope.map(str::to_owned),
                    replayed_prefix: pending.replay_applied.is_some(),
                    replay_chain_id: pending.replay_applied.map(|e| e.chain_id),
                    breakpoints_placed: pending.replay_applied.map(|e| e.breakpoints_placed),
                    system_markers_dropped: pending
                        .replay_applied
                        .map(|e| e.system_markers_dropped),
                    previous_forwarded_request_bytes,
                    forwarded_request_bytes: pending.forwarded_request_bytes,
                    event_kind,
                    wasted_tokens: charged_wasted_tokens,
                    cache_creation_input_tokens,
                    expected_cache_read,
                    actual_cache_read: cache_read_input_tokens,
                };
                match event_kind {
                    RecacheEventKind::Drift => tracing::warn!(
                        event = "cache_recache_observed",
                        request_id = %request_id,
                        conversation_key = %event.conversation_key,
                        session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                        // Which tracked stream the arithmetic was done against.
                        // `-1` = matched nothing, so this was booked a first turn.
                        matched_stream_msgs = matched_stream_msgs.map_or(-1_i64, |m| m as i64),
                        turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
                        streams_tracked = streams_tracked,
                        drift_dims = event.drift_dims.as_deref().unwrap_or(""),
                        replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
                        // `-1` for "not a divergence". The index says how much
                        // of the prefix died: an edit near the opener costs far
                        // more than one near the tail, and the message counts
                        // separate a mid-history deletion (count steady or
                        // falling) from ordinary appending.
                        first_diff_index = pending
                            .replay_skip
                            .and_then(|e| e.first_diff_index())
                            .map_or(-1_i64, |i| i as i64),
                        prior_message_count = pending
                            .replay_skip
                            .and_then(|e| e.message_counts().0)
                            .map_or(-1_i64, |n| n as i64),
                        current_message_count = pending
                            .replay_skip
                            .map_or(-1_i64, |e| e.message_counts().1 as i64),
                        attribution_reason = event.attribution_reason.as_deref().unwrap_or(""),
                        origin = event.origin.as_deref().unwrap_or(""),
                        scope = event.scope.as_deref().unwrap_or(""),
                        event_kind = "drift",
                        wasted_tokens = charged_wasted_tokens,
                        prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
                        prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
                        prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
                        prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
                        expected_cache_read = expected_cache_read,
                        actual_cache_read = cache_read_input_tokens,
                        cache_creation_input_tokens = cache_creation_input_tokens,
                        "prompt cache re-written inside the TTL window: billed tokens wasted re-caching"
                    ),
                    RecacheEventKind::Branch => tracing::info!(
                        event = "cache_recache_observed",
                        request_id = %request_id,
                        conversation_key = %event.conversation_key,
                        session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                        // Which tracked stream the arithmetic was done against.
                        // `-1` = matched nothing, so this was booked a first turn.
                        matched_stream_msgs = matched_stream_msgs.map_or(-1_i64, |m| m as i64),
                        turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
                        streams_tracked = streams_tracked,
                        drift_dims = event.drift_dims.as_deref().unwrap_or(""),
                        replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
                        attribution_reason = "inbound_tail_replaced",
                        origin = "inbound",
                        scope = "final_message",
                        event_kind = "branch",
                        wasted_tokens = 0,
                        // Branch is the one kind that charges nothing: the
                        // tail really did change, so the rebuild was earned.
                        // But `is_inbound_tail_replacement` asks only for an
                        // equal message count and a difference at the last
                        // index, which a retry that re-rendered its final
                        // message matches just as well — and then the
                        // shortfall was real money written off. Report it
                        // uncharged so the bucket can be audited instead of
                        // reading as a flat zero.
                        uncharged_shortfall_tokens = wasted_tokens,
                        prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
                        prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
                        prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
                        prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
                        expected_cache_read = expected_cache_read,
                        actual_cache_read = cache_read_input_tokens,
                        cache_creation_input_tokens = cache_creation_input_tokens,
                        "prompt cache built for an inbound final-message replacement; branch creation, not waste"
                    ),
                    RecacheEventKind::Unexplained => tracing::warn!(
                        event = "cache_recache_observed",
                        request_id = %request_id,
                        conversation_key = %event.conversation_key,
                        session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                        // Which tracked stream the arithmetic was done against.
                        // `-1` = matched nothing, so this was booked a first turn.
                        matched_stream_msgs = matched_stream_msgs.map_or(-1_i64, |m| m as i64),
                        turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
                        streams_tracked = streams_tracked,
                        attribution_reason = landing.as_str(),
                        landing = landing.as_str(),
                        origin = "unknown",
                        scope = "replayed_prefix",
                        event_kind = "unexplained",
                        // The same structural evidence the drift arm prints.
                        // Until this was here, "unexplained" was unexplained by
                        // construction: attribution runs before these fields
                        // are read, so anything reaching this arm was recorded
                        // as causeless without ever being shown against the
                        // evidence — 2.45M of 3.76M wasted tokens over the
                        // 2026-08-09 logs, in a field set disjoint from the
                        // drift arm's. Printing them changes no classification;
                        // it lets a later query ask how many of these turns had
                        // a structural dimension that simply went unconsulted.
                        drift_dims = event.drift_dims.as_deref().unwrap_or(""),
                        replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
                        prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
                        prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
                        prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
                        prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
                        replayed_prefix = true,
                        replay_chain_id = event.replay_chain_id.unwrap_or(0),
                        breakpoints_placed = event.breakpoints_placed.unwrap_or(0),
                        system_markers_dropped = event.system_markers_dropped.unwrap_or(0),
                        previous_forwarded_request_bytes = event.previous_forwarded_request_bytes.unwrap_or(0),
                        forwarded_request_bytes = event.forwarded_request_bytes.unwrap_or(0),
                        wasted_tokens = charged_wasted_tokens,
                        expected_cache_read = expected_cache_read,
                        actual_cache_read = cache_read_input_tokens,
                        // The three boundaries the landing was read against,
                        // so the classification can be audited off the line.
                        // `-1` = no turn before the previous one.
                        previous_cache_read = previous_cache_read,
                        previous_boundary = expected_cache_read,
                        previous_previous_boundary = previous_previous_boundary.map_or(-1_i64, |b| b as i64),
                        cache_creation_input_tokens = cache_creation_input_tokens,
                        "provider did not reuse the expected cache footprint after a confirmed prefix replay"
                    ),
                    RecacheEventKind::Expected => tracing::info!(
                        event = "cache_recache_observed",
                        request_id = %request_id,
                        conversation_key = %event.conversation_key,
                        session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                        // Which tracked stream the arithmetic was done against.
                        // `-1` = matched nothing, so this was booked a first turn.
                        matched_stream_msgs = matched_stream_msgs.map_or(-1_i64, |m| m as i64),
                        turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
                        streams_tracked = streams_tracked,
                        drift_dims = "",
                        replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
                        attribution_reason = "",
                        origin = "",
                        scope = "",
                        event_kind = "expected",
                        wasted_tokens = charged_wasted_tokens,
                        prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
                        prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
                        prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
                        prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
                        expected_cache_read = expected_cache_read,
                        actual_cache_read = cache_read_input_tokens,
                        cache_creation_input_tokens = cache_creation_input_tokens,
                        "prompt cache re-written inside the TTL window with no causal evidence: cause unattributed"
                    ),
                }
                crate::observability::observe_recache_event(
                    event.attribution_reason.as_deref(),
                    attribution.counts_as_waste.then_some(wasted_tokens),
                );
                inner.last_event = Some(event);
                Some(match event_kind {
                    // A structural bust: bytes inside the cached prefix moved,
                    // and `wasted_tokens` is what that cost.
                    RecacheEventKind::Drift => CompletionClass::PrefixChange { wasted_tokens },
                    RecacheEventKind::Unexplained => {
                        CompletionClass::UnexplainedAfterReplay { wasted_tokens }
                    }
                    // The event remains visible in cache health, but it is not
                    // a miss and therefore has no durable miss classification.
                    RecacheEventKind::Branch => return None,
                    // A re-cache with no direct causal evidence. Counted, but
                    // not charged as attributed waste.
                    RecacheEventKind::Expected => CompletionClass::Unknown,
                })
            }
        }
    }

    /// One cheap in-memory snapshot for `GET /cache-health`.
    pub fn snapshot(&self) -> CacheHealthSnapshot {
        let inner = self.lock();
        let mut capable_sum = 0.0;
        let mut capable_samples = 0usize;
        for sample in inner.recent_hit_rates.iter().filter(|s| s.cache_capable) {
            capable_sum += sample.rate;
            capable_samples += 1;
        }
        let recent_hit_rate = if capable_samples == 0 {
            None
        } else {
            Some(capable_sum / capable_samples as f64)
        };
        let last_event_age_seconds = inner.last_event.as_ref().map(|e| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs()
                .saturating_sub(e.at_unix)
        });
        let recent_cache_read_tokens = inner
            .recent_cost_samples
            .iter()
            .map(|s| s.cache_read_tokens)
            .sum();
        let recent_cache_write_tokens = inner
            .recent_cost_samples
            .iter()
            .map(|s| s.cache_write_tokens)
            .sum();
        let recent_forwarded_bytes: u64 = inner
            .recent_cost_samples
            .iter()
            .map(|s| s.forwarded_bytes)
            .sum();
        let recent_cost_per_forwarded_kb = if recent_forwarded_bytes == 0 {
            None
        } else {
            let billed: f64 = inner
                .recent_cost_samples
                .iter()
                .map(|s| s.billed_fresh_equivalents)
                .sum();
            Some(billed / (recent_forwarded_bytes as f64 / 1024.0))
        };
        CacheHealthSnapshot {
            recent_hit_rate,
            samples: capable_samples,
            recache_events_total: inner.recache_events_total,
            recache_wasted_tokens_total: inner.recache_wasted_tokens_total,
            ttl_expiries_total: inner.ttl_expiries_total,
            earned_cache_write_tokens_total: inner.earned_cache_write_tokens_total,
            unearned_cache_write_tokens_total: inner.unearned_cache_write_tokens_total,
            unearned_write_turns_total: inner.unearned_write_turns_total,
            abandoned_requests_total: inner.abandoned_requests_total,
            concurrency_sheds_total: inner.concurrency_sheds_total,
            hot_zone_changes_total: inner.hot_zone_changes_total,
            hot_zone_recaches_total: inner.hot_zone_recaches_total,
            stabilization_absorbed_total: inner.stabilization_absorbed_total,
            stabilization_absorbed_tokens_total: inner.stabilization_absorbed_tokens_total,
            ours_effective_tokens: inner.ours_effective_tokens.round() as u64,
            stock_effective_tokens: inner.stock_effective_tokens.round() as u64,
            stock_turns_compared: inner.stock_turns_compared,
            vs_stock_saving_pct: {
                // Nothing compared yet reads as "no difference", not as a win.
                if inner.stock_effective_tokens <= 0.0 {
                    0.0
                } else {
                    (1.0 - inner.ours_effective_tokens / inner.stock_effective_tokens) * 100.0
                }
            },
            vs_stock_saving_pct_recent: {
                let stock: f64 = inner.recent_vs_stock.iter().map(|(_, s)| s).sum();
                if stock <= 0.0 {
                    None
                } else {
                    let ours: f64 = inner.recent_vs_stock.iter().map(|(o, _)| o).sum();
                    Some((1.0 - ours / stock) * 100.0)
                }
            },
            vs_stock_turns_recent: inner.recent_vs_stock.len(),
            predicted_read_error_pct: {
                // Denominator is the observed reads the rule was predicting.
                // Before anything has been read back there is no error to
                // report, and 0.0 says exactly that.
                if inner.observed_read_tokens == 0 {
                    0.0
                } else {
                    inner.predicted_read_abs_error as f64 * 100.0
                        / inner.observed_read_tokens as f64
                }
            },
            stabilization_absorb_pct: {
                // Denominator is absorbed + re-cached, not every hot-zone
                // change: first turns and TTL expiries are counted in neither
                // and would drag the rate toward a number about idling.
                let judged = inner
                    .stabilization_absorbed_total
                    .saturating_add(inner.hot_zone_recaches_total);
                if judged == 0 {
                    100.0
                } else {
                    inner.stabilization_absorbed_total as f64 * 100.0 / judged as f64
                }
            },
            productive_write_pct: {
                let earned = inner.earned_cache_write_tokens_total;
                let written = earned.saturating_add(inner.unearned_cache_write_tokens_total);
                if written == 0 {
                    100.0
                } else {
                    earned as f64 * 100.0 / written as f64
                }
            },
            first_turn_writes_total: inner.first_turn_writes_total,
            first_turn_write_tokens_total: inner.first_turn_write_tokens_total,
            first_turn_contradictions_total: inner.first_turn_contradictions_total,
            last_event: inner.last_event.clone(),
            last_event_age_seconds,
            recent_cache_read_tokens,
            recent_cache_write_tokens,
            recent_forwarded_bytes,
            recent_cost_per_forwarded_kb,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::proxy_counters::cache_miss_attribution_for_test;

    /// Eviction and never-seen used to be the same observation, so a turn
    /// that came back after its conversation fell out of the map was booked
    /// a first turn and its cache write went uncounted, silently.
    #[test]
    fn a_conversation_evicted_before_its_next_turn_is_counted_as_forgotten() {
        let obs = UsageObserver::new();
        let fp = |msgs| PrefixFingerprint {
            head: "h".into(),
            body: "b".into(),
            stable: "s".into(),
            stable_msgs: msgs,
        };
        obs.begin_request("r0", "conv-evicted".into(), None, None, Some(fp(10)));
        obs.complete("r0", 100, 60_000, 20_000, None);

        // Push it off the end with a full capacity of other conversations.
        for i in 0..CONVERSATION_CAPACITY {
            let id = format!("r-filler-{i}");
            obs.begin_request(&id, format!("conv-{i}"), None, None, Some(fp(10)));
            obs.complete(&id, 100, 1_000, 1_000, None);
        }
        assert_eq!(
            obs.lock().forgotten_conversations_total,
            0,
            "nothing is forgotten until an evicted conversation comes back"
        );

        obs.begin_request("r1", "conv-evicted".into(), None, None, Some(fp(12)));
        obs.complete("r1", 100, 0, 80_000, None);
        assert_eq!(
            obs.lock().forgotten_conversations_total,
            1,
            "the turn is still booked a first turn, but the undercount is on record"
        );
    }

    // ── aftershock attribution ───────────────────────────────────────
    fn applied_evidence() -> ReplayAppliedEvidence {
        ReplayAppliedEvidence::new(1, 2, 0)
    }

    #[test]
    fn a_turn_after_a_divergence_names_the_previous_turn() {
        let a = recache_attribution(
            None,
            false,
            None,
            None,
            Some(applied_evidence()),
            true,
            false,
        );
        assert_eq!(a.reason, Some("aftershock_of_diverged_prefix"));
        assert_eq!(a.origin, Some("previous_turn"));
        assert!(a.counts_as_waste, "the rewrite is still real waste");
    }

    /// The residual marker never reaches an event: `complete` swaps it for a
    /// [`CacheLanding`] reason, keeping `origin` as it is.
    #[test]
    fn without_a_previous_divergence_the_residual_is_left_for_the_landing() {
        let a = recache_attribution(
            None,
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
            false,
        );
        assert_eq!(a.reason, Some("unexplained_after_replay"));
        assert_eq!(a.origin, Some("unknown"));
    }

    // ── landing classification ───────────────────────────────────────
    //
    // prev read 100 and wrote 50, so prev_boundary = 150; the turn before it
    // ended at 80.
    fn landing(actual: u64, prevprev: Option<u64>) -> &'static str {
        CacheLanding::classify(actual, 100, 150, prevprev).as_str()
    }

    #[test]
    fn a_read_equal_to_the_previous_read_missed_the_newest_write() {
        assert_eq!(landing(100, Some(80)), "provider_missed_newest_write");
        assert_eq!(landing(100, None), "provider_missed_newest_write");
    }

    #[test]
    fn a_read_inside_the_previous_write_is_partial() {
        assert_eq!(landing(101, Some(80)), "provider_partial_of_previous_write");
        assert_eq!(landing(149, None), "provider_partial_of_previous_write");
    }

    #[test]
    fn a_read_back_at_the_older_boundary_was_a_free_read_never_persisted() {
        assert_eq!(landing(80, Some(80)), "provider_free_read_not_persisted");
    }

    #[test]
    fn a_read_below_the_older_boundary_dropped_an_older_entry() {
        assert_eq!(landing(79, Some(80)), "provider_dropped_older_entry");
        // With no older boundary known, anything short of the previous read.
        assert_eq!(landing(99, None), "provider_dropped_older_entry");
    }

    #[test]
    fn a_read_between_the_two_boundaries_is_between_entries() {
        assert_eq!(landing(90, Some(80)), "provider_between_entries");
    }

    /// The older boundary rides on the `TurnRecord`, one turn behind, so the
    /// third turn of a stream can be placed against the first's boundary.
    #[test]
    fn the_older_boundary_is_carried_across_turns() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        let fp = |msgs| PrefixFingerprint {
            head: "h".into(),
            body: "b".into(),
            stable: "s".into(),
            stable_msgs: msgs,
        };
        // Turn 1: boundary 80_000.
        obs.begin_request("c1", "conv-carry".into(), None, None, Some(fp(10)));
        obs.complete("c1", 100, 60_000, 20_000, None);
        // Turn 2 read past that boundary without anyone writing there.
        obs.begin_request("c2", "conv-carry".into(), None, None, Some(fp(12)));
        obs.complete("c2", 100, 100_000, 5_000, None);
        // Turn 3 lands exactly on turn 1's boundary.
        obs.begin_request("c3", "conv-carry".into(), None, None, Some(fp(14)));
        obs.note_replay_applied("c3", ReplayAppliedEvidence::new(3, 2, 0));
        let class = obs.complete("c3", 100, 80_000, 25_000, None);
        assert_eq!(
            class,
            Some(CompletionClass::UnexplainedAfterReplay {
                wasted_tokens: 25_000
            })
        );
        let event = obs.snapshot().last_event.expect("recache recorded");
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("provider_free_read_not_persisted")
        );
        assert_eq!(event.origin.as_deref(), Some("unknown"));
        assert_eq!(event.event_kind, RecacheEventKind::Unexplained);
    }

    /// A turn that began while another turn of the same conversation was still
    /// running gets its own name, and still counts as waste.
    ///
    /// Measured over the 2026-08-20/22 logs: 72% of overlapping turn-pairs lose
    /// cache against a ~5% baseline — 377 pairs, 408,980 tokens — and 374 of
    /// them had a replay applied, so the splice was right and only the timing
    /// was wrong. Before this they landed in `unexplained_after_replay`, which
    /// is where a cause goes to be forgotten.
    #[test]
    fn a_turn_racing_its_own_conversation_is_named_but_still_billed() {
        let a = recache_attribution(
            None,
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
            true,
        );
        assert_eq!(a.reason, Some("concurrent_turn_in_flight"));
        assert_eq!(a.origin, Some("client"));
        assert!(
            a.counts_as_waste,
            "the tokens were re-billed; calling this expected would retire 409k \
             tokens into a bucket nobody reads"
        );
    }

    /// Concurrency is the explanation of last resort. A structural cause the
    /// evidence actually names must win, or a real edit hides behind a race.
    #[test]
    fn a_named_cause_outranks_concurrency() {
        let a = recache_attribution(
            Some("system"),
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
            true,
        );
        assert_eq!(a.reason, Some("system"));
    }

    /// A retry that never completed still consumed the drift detector's edge,
    /// so the attempt that was billed saw empty dims and only the race left to
    /// report. 2026-09-03 15:57:03Z: the client resent a turn with a system
    /// prompt 6 kB shorter and the retry wrote 213,309 tokens against 15,621
    /// read, filed `concurrent_turn_in_flight` — true, and not the cause.
    #[test]
    fn a_moved_cacheable_head_outranks_a_retry_still_in_flight() {
        let a = recache_attribution(
            None,
            true,
            None,
            None,
            Some(applied_evidence()),
            false,
            true,
        );
        assert_eq!(a.reason, Some("prefix_head_changed"));
        assert_eq!(a.origin, Some("client"));
        assert_eq!(a.scope, Some("hot_zone"));
        assert!(a.counts_as_waste, "the prefix was genuinely re-written");
    }

    /// Declining a replay is what moves the forwarded hot zone: the overlay
    /// stops restoring the stored early messages and the prefix snaps back to
    /// the client's bytes. On 2026-09-03 that read as `origin=proxy` on 7
    /// turns where the client had inserted a `role:"system"` message at index
    /// 1 and the proxy had declined replay exactly as it should.
    #[test]
    fn a_declined_replay_is_charged_to_the_client_that_diverged() {
        let prior = vec![
            serde_json::json!({"role": "user", "content": "a"}),
            serde_json::json!({"role": "assistant", "content": "b"}),
        ];
        let current = vec![
            serde_json::json!({"role": "user", "content": "a"}),
            serde_json::json!({"role": "system", "content": "reminder"}),
            serde_json::json!({"role": "assistant", "content": "b"}),
        ];
        let skip = ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 1,
                replayed_prefix_msgs: 0,
            },
            Some(&prior),
            &current,
        );
        let a = recache_attribution(
            None,
            false,
            Some("early_messages"),
            Some(skip),
            None,
            false,
            false,
        );
        assert_eq!(a.reason, Some("prefix_content_diverged"));
        assert_eq!(a.origin, Some("client"));
        assert_eq!(a.scope, Some("stored_prefix"));
    }

    #[test]
    fn a_quiet_inbound_with_a_moved_outbound_is_charged_to_the_proxy() {
        // The hole this closes: the inbound hash is taken before any proxy
        // stage runs, so a recache the proxy itself caused used to fall
        // through every branch and land in the residual.
        let a = recache_attribution(
            None,
            false,
            Some("tools,messages[0]"),
            None,
            Some(applied_evidence()),
            false,
            false,
        );
        assert_eq!(a.reason, Some("tools,messages[0]"));
        assert_eq!(a.origin, Some("proxy"));
        assert!(a.counts_as_waste, "our own rewrite is waste like any other");
    }

    #[test]
    fn client_drift_wins_when_both_hot_zones_moved() {
        // The proxy carries the client's edit forward, so the outbound hash
        // moves whenever the inbound one did. Blaming the proxy for that
        // would misattribute nearly every ordinary recache.
        let a = recache_attribution(
            Some("system"),
            false,
            Some("system,tools"),
            None,
            Some(applied_evidence()),
            false,
            false,
        );
        assert_eq!(a.reason, Some("system"));
        // Named, not blank: the inbound hash moved, and only the client can
        // move that.
        assert_eq!(a.origin, Some("client"));
        assert_eq!(a.scope, Some("hot_zone"));
    }

    #[test]
    fn a_cause_this_turn_produced_outranks_the_aftershock_flag() {
        // The carried flag must never mask evidence from the turn itself,
        // or a divergence following a divergence would be filed as its own
        // aftershock and the real cause would vanish.
        let prior = vec![serde_json::json!({"role": "user", "content": "a"})];
        let current = vec![
            serde_json::json!({"role": "user", "content": "b"}),
            serde_json::json!({"role": "assistant", "content": "c"}),
        ];
        let skip = ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0,
                replayed_prefix_msgs: 0,
            },
            Some(&prior),
            &current,
        );
        let a = recache_attribution(
            None,
            false,
            None,
            Some(skip),
            Some(applied_evidence()),
            true,
            false,
        );
        assert_eq!(a.reason, Some("prefix_content_diverged"));
    }

    /// The Prometheus registry is process-global, so tests that read a counter
    /// delta must not run concurrently with any other test that writes it.
    pub(super) fn miss_metric_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn miss_count(reason: &str) -> u64 {
        cache_miss_attribution_for_test(MISS_ATTRIBUTION_PROVIDER, reason)
    }

    fn prev(read: u64, creation: u64, age: Duration) -> TurnRecord {
        TurnRecord {
            cache_read_input_tokens: read,
            cache_creation_input_tokens: creation,
            at: SystemTime::now() - age,
            forwarded_request_bytes: None,
            msgs: None,
            diverged: false,
            previous_boundary: None,
            head: None,
            stock_footprint: read + creation,
        }
    }

    #[test]
    fn healthy_turn_reads_previous_prefix() {
        // prev cached 10_000 + wrote 2_000 → expect 12_000 read.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 0, 12_000, 500, ANTHROPIC_CACHE_TTL);
        assert_eq!(c, TurnClass::Healthy);
    }

    /// The healthy bucket is not one thing. A turn that appends new content
    /// and caches it spent well; a turn that re-writes footprint the
    /// conversation already had spent for nothing. Both read their prefix
    /// cleanly, so both used to report nothing at all.
    #[test]
    fn a_healthy_write_that_buys_new_footprint_is_earned() {
        // Footprint went 10,000 -> 22,000 and the turn wrote the 12,000
        // difference. Every token bought ground the conversation had not held.
        let (earned, unearned) = split_cache_write(10_000, 10_000, 12_000);
        assert_eq!((earned, unearned), (12_000, 0));
    }

    /// A forwarded request that nothing ever completes is the seam between
    /// what the proxy sent and what the books know about. Two ways out of the
    /// pending cache and only one of them is booking; this covers the other.
    #[test]
    fn a_request_nothing_completes_is_counted_as_abandoned() {
        let print = |n: usize| PrefixFingerprint {
            head: "head".into(),
            body: "body".into(),
            stable: format!("stable-{n}"),
            stable_msgs: n,
        };
        let obs = UsageObserver::new();
        obs.begin_request("stranded", "conv".into(), None, None, Some(print(10)));
        assert_eq!(
            obs.snapshot().abandoned_requests_total,
            0,
            "still in flight"
        );

        obs.age_pending("stranded", IN_FLIGHT_HORIZON);
        // The sweep runs on the next arrival, which is the only moment the
        // observer is awake.
        obs.begin_request("next", "conv".into(), None, None, Some(print(10)));
        assert_eq!(obs.snapshot().abandoned_requests_total, 1);

        obs.complete("next", 10, 0, 0, None);
        obs.begin_request("third", "conv".into(), None, None, Some(print(10)));
        assert_eq!(
            obs.snapshot().abandoned_requests_total,
            1,
            "a completed request is taken by `complete`, never swept"
        );
    }

    /// The concurrency cap sheds only past the cap, pops the shed turn so it
    /// neither flags later turns concurrent nor counts as abandoned, and
    /// leaves other conversations alone.
    #[test]
    fn the_cap_sheds_only_past_the_cap_and_pops_the_shed_turn() {
        let obs = UsageObserver::new();
        obs.begin_request("r1", "conv".into(), None, None, None);
        obs.begin_request("r2", "conv".into(), None, None, None);
        assert_eq!(
            obs.shed_if_over_conversation_cap("r2", "conv", 2),
            None,
            "at the cap, not past it"
        );
        obs.begin_request("other", "elsewhere".into(), None, None, None);
        assert_eq!(
            obs.shed_if_over_conversation_cap("other", "elsewhere", 2),
            None,
            "other conversations are not counted"
        );
        obs.begin_request("r3", "conv".into(), None, None, None);
        assert_eq!(
            obs.shed_if_over_conversation_cap("r3", "conv", 2),
            Some(3),
            "self plus two in flight exceeds a cap of two"
        );
        let s = obs.snapshot();
        assert_eq!(s.concurrency_sheds_total, 1);
        assert_eq!(
            s.abandoned_requests_total, 0,
            "a shed turn was never forwarded; it must not read as abandoned"
        );
        // The shed turn is gone: completing it is a no-op, and the survivors
        // complete normally without seeing it as concurrent baggage.
        assert_eq!(obs.complete("r3", 10, 0, 0, None), None);
        obs.complete("r1", 10, 0, 0, None);
        assert_eq!(obs.snapshot().concurrency_sheds_total, 1);
    }

    /// `cap == 0` is the off switch: whatever is in flight, nothing sheds.
    #[test]
    fn a_zero_cap_disables_the_check() {
        let obs = UsageObserver::new();
        for i in 0..5 {
            let id = format!("r{i}");
            obs.begin_request(&id, "conv".into(), None, None, None);
            assert_eq!(obs.shed_if_over_conversation_cap(&id, "conv", 0), None);
        }
        assert_eq!(obs.snapshot().concurrency_sheds_total, 0);
    }

    /// Stale entries past the horizon are not in flight, so a leftover from
    /// a dead turn cannot keep tripping the cap for the turns after it.
    #[test]
    fn stale_entries_do_not_count_toward_the_cap() {
        let obs = UsageObserver::new();
        obs.begin_request("old", "conv".into(), None, None, None);
        obs.age_pending("old", IN_FLIGHT_HORIZON);
        obs.begin_request("r1", "conv".into(), None, None, None);
        obs.begin_request("r2", "conv".into(), None, None, None);
        // "old" was swept as abandoned by r1's arrival; r1 and r2 are the
        // only live entries, exactly at a cap of two.
        assert_eq!(obs.shed_if_over_conversation_cap("r2", "conv", 2), None);
        assert_eq!(obs.snapshot().concurrency_sheds_total, 0);
    }

    /// The statusline number. Writes only, and the two buckets sum to every
    /// written token, so it is a share and not an estimate.
    #[test]
    fn productive_write_pct_is_the_earned_share_of_every_written_token() {
        let obs = UsageObserver::new();
        assert_eq!(
            obs.snapshot().productive_write_pct,
            100.0,
            "a process that has written nothing has wasted nothing"
        );

        {
            let mut inner = obs.lock();
            inner.earned_cache_write_tokens_total = 3_000;
            inner.unearned_cache_write_tokens_total = 1_000;
        }
        assert_eq!(obs.snapshot().productive_write_pct, 75.0);
    }

    /// The gate this split first shipped behind made it dead code, and only
    /// arithmetic showed it: `Healthy` means the read covered the previous
    /// footprint to within `RECACHE_SLACK_TOKENS`, which bounds `unearned` by
    /// that same slack — under the warning floor, on every turn that could
    /// reach the counter. The turns worth naming are the ones that gate
    /// excluded, so the split now runs on all of them. This holds the proof.
    #[test]
    fn a_healthy_turn_can_never_have_more_unearned_than_the_slack() {
        for prev in [0u64, 1_000, 50_000, 249_949, 1_000_000] {
            for creation in [1u64, 64, 5_000, 120_000] {
                // The healthiest and the worst-but-still-healthy read.
                for read in [prev, prev.saturating_sub(RECACHE_SLACK_TOKENS)] {
                    let (_, unearned) = split_cache_write(prev, read, creation);
                    assert!(
                        unearned <= RECACHE_SLACK_TOKENS,
                        "prev={prev} read={read} creation={creation} unearned={unearned}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_healthy_write_over_ground_already_held_is_unearned() {
        // The shape that repeated every 30 seconds on conv fb553646 on
        // 2026-09-07: footprint 249,949, read 186,655, wrote 63,838 — so the
        // footprint ended at 250,493 and the write bought 544 tokens of it.
        let (earned, unearned) = split_cache_write(249_949, 186_655, 63_838);
        assert_eq!(earned, 544);
        assert_eq!(unearned, 63_294);
    }

    #[test]
    fn a_shrinking_footprint_earns_nothing_and_never_underflows() {
        let (earned, unearned) = split_cache_write(500_000, 1_000, 9_000);
        assert_eq!((earned, unearned), (0, 9_000));
    }

    /// The counters must reconcile: every token a healthy turn wrote lands on
    /// exactly one side of the split. That is the whole point of the split —
    /// the bucket that could not be reconciled was the one hiding money.
    #[test]
    fn the_split_accounts_for_every_written_token() {
        for (prev, read, creation) in [
            (0u64, 0u64, 5_000u64),
            (10_000, 10_000, 12_000),
            (249_949, 186_655, 63_838),
            (500_000, 1_000, 9_000),
            (77, 4_096, 64),
        ] {
            let (earned, unearned) = split_cache_write(prev, read, creation);
            assert_eq!(earned + unearned, creation, "prev={prev} read={read}");
        }
    }

    /// First-turn cache writes are not waste, but they were not countable
    /// either: 2,729,094 tokens went through this path on 2026-09-07 with
    /// nothing in `/cache-health` behind them. The snapshot now carries them,
    /// and separates the ones whose stated reason contradicts what the turn
    /// did — here, a turn that arrived carrying history yet read no cache,
    /// which is a live conversation whose key moved, not a cold start.
    #[test]
    fn first_turn_writes_are_counted_and_contradictions_singled_out() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("f1", "conv-first-turn".into(), None, None, None);
        obs.note_first_turn_context(
            "f1",
            FirstTurnContext {
                msgs: 40,
                message_zero_hash: None,
                compaction_restart: false,
                model: None,
            },
        );
        obs.complete("f1", 100, 0, 50_000, None);
        let snap = obs.snapshot();
        assert_eq!(snap.first_turn_writes_total, 1);
        assert_eq!(snap.first_turn_write_tokens_total, 50_000);
        assert_eq!(
            snap.first_turn_contradictions_total, 1,
            "history but no read is not a cold start"
        );
    }

    /// A genuine cold start counts as a write and not as a contradiction, so
    /// the two numbers keep meaning different things.
    #[test]
    fn a_real_cold_start_is_counted_but_not_flagged() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("f1", "conv-cold-start".into(), None, None, None);
        obs.note_first_turn_context(
            "f1",
            FirstTurnContext {
                msgs: 1,
                message_zero_hash: None,
                compaction_restart: false,
                model: None,
            },
        );
        obs.complete("f1", 100, 0, 50_000, None);
        let snap = obs.snapshot();
        assert_eq!(snap.first_turn_writes_total, 1);
        assert_eq!(snap.first_turn_contradictions_total, 0);
    }

    /// A turn that read nothing of a prefix it should have read, wrote
    /// nothing, and paid full input price for the whole prompt. Until
    /// 2026-09-08 this arm looked only at what was written, so it returned
    /// `Healthy` and the money vanished. Six turns on 09-07 had this shape.
    #[test]
    fn a_shortfall_paid_as_fresh_input_is_not_healthy() {
        let p = prev(14_080, 0, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 14_226, 0, 0, ANTHROPIC_CACHE_TTL);
        assert_eq!(
            c,
            TurnClass::Recache {
                wasted_tokens: 14_080
            },
            "capped at the shortfall, not the whole fresh prompt"
        );
    }

    /// The other side of the same arm: nothing read, nothing written, and
    /// nothing billed fresh either. A shorter branch under the same key costs
    /// nothing and must stay quiet.
    #[test]
    fn a_shortfall_that_cost_nothing_stays_healthy() {
        let p = prev(14_080, 0, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 0, 0, 0, ANTHROPIC_CACHE_TTL);
        assert_eq!(c, TurnClass::Healthy);
    }

    #[test]
    fn healthy_within_slack() {
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(
            &p,
            SystemTime::now(),
            0,
            12_000 - RECACHE_SLACK_TOKENS,
            500,
            ANTHROPIC_CACHE_TTL,
        );
        assert_eq!(c, TurnClass::Healthy);
    }

    #[test]
    fn recache_inside_ttl_is_flagged_with_wasted_tokens() {
        // Expected read 12_000, got 0, re-wrote 12_500 → 12_000 wasted.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 0, 0, 12_500, ANTHROPIC_CACHE_TTL);
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
        let c = classify_turn(&p, SystemTime::now(), 0, 0, 3_000, ANTHROPIC_CACHE_TTL);
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
        let c = classify_turn(&p, SystemTime::now(), 0, 0, 12_500, ANTHROPIC_CACHE_TTL);
        assert_eq!(c, TurnClass::TtlExpiry);
    }

    #[test]
    fn a_bust_inside_the_pinned_hour_is_not_excused_as_expiry() {
        // The defect this parameter exists for. With `--force-1h-cache-ttl` the
        // forwarded body pins an hour, so a 20-minute gap cannot have expired —
        // but the classifier used to key off the 5-minute tier and filed it as
        // "expected, not a defect". Measured at 557,276 creation tokens in one
        // day, all of it invisible.
        let gap = Duration::from_secs(20 * 60);
        let p = prev(10_000, 2_000, gap);

        let excused = classify_turn(&p, SystemTime::now(), 0, 0, 12_500, ANTHROPIC_CACHE_TTL);
        assert_eq!(
            excused,
            TurnClass::TtlExpiry,
            "sanity: against the 5-minute tier this gap does read as an expiry"
        );

        let p = prev(10_000, 2_000, gap);
        let honest = classify_turn(&p, SystemTime::now(), 0, 0, 12_500, ANTHROPIC_CACHE_TTL_1H);
        assert_eq!(
            honest,
            TurnClass::Recache {
                wasted_tokens: 12_000
            },
            "against the hour we actually pin it is a bust and must be counted"
        );
    }

    #[test]
    fn a_gap_beyond_the_pinned_hour_is_still_an_expiry() {
        let p = prev(
            10_000,
            2_000,
            ANTHROPIC_CACHE_TTL_1H + Duration::from_secs(10),
        );
        let c = classify_turn(&p, SystemTime::now(), 0, 0, 12_500, ANTHROPIC_CACHE_TTL_1H);
        assert_eq!(c, TurnClass::TtlExpiry);
    }

    #[test]
    fn read_drop_without_rewrite_is_healthy() {
        // Branched/shorter conversation: read dropped but nothing
        // significant was re-billed → nothing to warn about.
        let p = prev(10_000, 2_000, Duration::from_secs(30));
        let c = classify_turn(&p, SystemTime::now(), 0, 4_000, 10, ANTHROPIC_CACHE_TTL);
        assert_eq!(c, TurnClass::Healthy);
    }

    #[test]
    fn observer_end_to_end_flags_recache_and_snapshots() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        // Turn 1.
        obs.begin_request("req-1", "conv-a".into(), None, None, None);
        obs.complete("req-1", 300, 0, 10_000, None);
        // Turn 2: healthy.
        obs.begin_request("req-2", "conv-a".into(), None, None, None);
        obs.complete("req-2", 200, 10_000, 800, None);
        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 0);
        // Turn 3: recache, drift detector blamed tools.
        obs.begin_request("req-3", "conv-a".into(), None, Some("tools".into()), None);
        obs.complete("req-3", 200, 0, 11_000, None);
        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 1);
        let ev = snap.last_event.expect("recache event recorded");
        assert_eq!(ev.drift_dims.as_deref(), Some("tools"));
        assert_eq!(ev.attribution_reason.as_deref(), Some("tools"));
        assert_eq!(ev.event_kind, RecacheEventKind::Drift);
        assert_eq!(ev.expected_cache_read, 10_800);
        assert_eq!(ev.wasted_tokens, 10_800);
        assert_eq!(snap.recache_wasted_tokens_total, 10_800);
        assert!(snap.recent_hit_rate.is_some());
        assert_eq!(snap.samples, 3);
    }

    /// A turn whose provider reported no cache-usage data is "no signal", not
    /// a miss: the mean covers capable turns only, while a capable genuine
    /// 0% still counts.
    #[test]
    fn recent_hit_rate_ignores_turns_without_cache_data() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("cap-1", "conv-a".into(), None, None, None);
        obs.complete_with_cache_capability("cap-1", 100, 900, 0, None, true);
        obs.begin_request("dark-1", "conv-b".into(), None, None, None);
        obs.complete_with_cache_capability("dark-1", 1_000, 0, 0, None, false);
        obs.begin_request("cap-2", "conv-c".into(), None, None, None);
        obs.complete_with_cache_capability("cap-2", 1_000, 0, 0, None, true);
        let snap = obs.snapshot();
        assert!((snap.recent_hit_rate.unwrap() - 0.45).abs() < 1e-9);
        assert_eq!(snap.samples, 2);
    }

    /// No capable sample yet reads exactly like no samples at all.
    #[test]
    fn recent_hit_rate_stays_null_without_capable_samples() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("dark-1", "conv-a".into(), None, None, None);
        obs.complete_with_cache_capability("dark-1", 1_000, 0, 0, None, false);
        let snap = obs.snapshot();
        assert_eq!(snap.recent_hit_rate, None);
        assert_eq!(snap.samples, 0);
    }

    /// These four fields were served by one binary, dropped from the tree, and
    /// nobody noticed until the statusline segment reading them went blank.
    /// Nothing else asserts they exist.
    #[test]
    fn snapshot_prices_the_recent_window() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("req-1", "conv-a".into(), None, None, None);
        obs.note_wire_bytes("req-1", 4096, 2048, "all_messages");
        obs.complete("req-1", 100, 10_000, 400, None);

        let snap = obs.snapshot();
        assert_eq!(snap.recent_cache_read_tokens, 10_000);
        assert_eq!(snap.recent_cache_write_tokens, 400);
        assert_eq!(snap.recent_forwarded_bytes, 2048);
        // 100 + 10_000 * 0.1 + 400 * 1.25 = 1600 over 2 KB.
        assert_eq!(snap.recent_cost_per_forwarded_kb, Some(800.0));

        // A turn that never reached the gate has no forwarded size, so it must
        // not price as free work.
        let obs = UsageObserver::new();
        obs.complete("never-began", 100, 10_000, 400, None);
        assert_eq!(obs.snapshot().recent_cost_per_forwarded_kb, None);
    }

    #[test]
    fn recache_without_drift_dims_is_not_drift_but_is_still_loud() {
        // Subagent close / `/clear`: cache busted upstream but the drift
        // detector saw stable bytes. Not Drift — nothing was attributed — yet
        // the rebuild was billed, so it may not sink to the INFO bucket
        // either. Unexplained is the honest middle: charged, and unnamed.
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("req-1", "conv-a".into(), None, None, None);
        obs.complete("req-1", 300, 0, 10_000, None);
        obs.begin_request("req-2", "conv-a".into(), None, None, None);
        obs.complete("req-2", 200, 0, 11_000, None);
        let ev = obs.snapshot().last_event.expect("event recorded");
        assert!(ev.wasted_tokens > 0);
        assert_eq!(ev.event_kind, RecacheEventKind::Unexplained);
        assert_eq!(ev.attribution_reason.as_deref(), Some("no_cause_found"));
    }

    #[test]
    fn recache_with_empty_string_drift_dims_is_not_drift_but_is_still_loud() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("req-1", "conv-a".into(), None, Some(String::new()), None);
        obs.complete("req-1", 300, 0, 10_000, None);
        obs.begin_request("req-2", "conv-a".into(), None, Some(String::new()), None);
        obs.complete("req-2", 200, 0, 11_000, None);
        let ev = obs.snapshot().last_event.expect("event recorded");
        assert_eq!(ev.event_kind, RecacheEventKind::Unexplained);
    }

    /// A turn that never reached `message_stop` (a 429, a dropped stream, a
    /// client that hung up) stays in the pending map. It must not make every
    /// later turn of the conversation look like a race.
    #[test]
    fn a_turn_that_never_completed_stops_counting_as_in_flight() {
        let obs = UsageObserver::new();
        obs.begin_request("req-1", "conv".into(), None, None, None);
        obs.begin_request("req-2", "conv".into(), None, None, None);
        assert_eq!(obs.pending_is_concurrent("req-2"), Some(true));

        obs.age_pending("req-1", IN_FLIGHT_HORIZON);
        obs.age_pending("req-2", IN_FLIGHT_HORIZON);
        obs.begin_request("req-3", "conv".into(), None, None, None);
        assert_eq!(
            obs.pending_is_concurrent("req-3"),
            Some(false),
            "a leftover older than the horizon is not a turn in flight"
        );
    }

    #[test]
    fn concurrent_conversations_do_not_cross_talk() {
        // Two conversations from the same client (main session +
        // subagent) interleave; neither must flag the other.
        let obs = UsageObserver::new();
        obs.begin_request("req-a1", "conv-a".into(), None, None, None);
        obs.complete("req-a1", 300, 0, 50_000, None);
        obs.begin_request("req-b1", "conv-b".into(), None, None, None);
        obs.complete("req-b1", 300, 0, 2_000, None);
        obs.begin_request("req-a2", "conv-a".into(), None, None, None);
        obs.complete("req-a2", 200, 50_000, 900, None);
        obs.begin_request("req-b2", "conv-b".into(), None, None, None);
        obs.complete("req-b2", 200, 2_000, 400, None);
        assert_eq!(obs.snapshot().recache_events_total, 0);
    }

    #[test]
    fn unknown_request_only_updates_rolling_rate() {
        let obs = UsageObserver::new();
        obs.complete("never-began", 100, 900, 0, None);
        let snap = obs.snapshot();
        assert_eq!(snap.samples, 1);
        assert_eq!(snap.recache_events_total, 0);
    }

    #[test]
    fn system_prompt_bust_is_classified_as_recache() {
        // The live scenario the watchdog originally missed: three turns of
        // one conversation, the third with a mutated system prompt. The
        // conversation key must survive the mutation so the collapsed
        // cache_read on turn 3 classifies as Recache, not FirstTurn.
        let body1 = serde_json::json!({
            "system": "stable system",
            "messages": [{"role":"user","content":"say ok"}]
        });
        let mut body3 = body1.clone();
        body3["system"] = serde_json::json!("MUTATED system");

        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        let k = conversation_key(&body1, "sess");
        // Turn 1: cold cache — all creation.
        obs.begin_request("r1", k.clone(), None, None, None);
        obs.complete("r1", 10, 0, 8400, None);
        // Turn 2: healthy — reads what turn 1 created.
        obs.begin_request("r2", conversation_key(&body1, "sess"), None, None, None);
        obs.complete("r2", 10, 8400, 0, None);
        // Turn 3: mutated system → cache busted upstream (read 0, big creation).
        obs.begin_request("r3", conversation_key(&body3, "sess"), None, None, None);
        obs.complete("r3", 10, 0, 8410, None);

        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 1, "bust must be classified");
        let ev = snap.last_event.expect("last_event populated");
        assert_eq!(ev.conversation_key, k);
        assert!(ev.wasted_tokens > 8000);
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
        // Mutated system prompt → SAME key: a system-prompt change is a
        // cache bust the watchdog must classify, so identity survives it.
        let mut body_a3 = body_a.clone();
        body_a3["system"] = serde_json::json!("MUTATED");
        assert_eq!(
            conversation_key(&body_a, "sess"),
            conversation_key(&body_a3, "sess")
        );
        // Different client → different key.
        assert_ne!(
            conversation_key(&body_a, "sess"),
            conversation_key(&body_a, "sess2")
        );
    }

    // ─── Cache-miss attribution metric ──────────────────────────────────

    /// A drift-attributed re-cache is a `prefix_change` miss, and only that.
    #[test]
    fn drift_recache_records_prefix_change() {
        let _guard = miss_metric_test_lock();
        let (b0, b1, b2) = (
            miss_count("prefix_change"),
            miss_count("unknown"),
            miss_count("ttl_expiry"),
        );

        let obs = UsageObserver::new();
        obs.begin_request(
            "m-d1",
            "conv-drift".into(),
            None,
            Some("tools".into()),
            None,
        );
        obs.complete("m-d1", 300, 0, 10_000, None);
        obs.begin_request(
            "m-d2",
            "conv-drift".into(),
            None,
            Some("tools".into()),
            None,
        );
        obs.complete("m-d2", 200, 0, 11_000, None);

        assert_eq!(miss_count("prefix_change"), b0 + 1);
        assert_eq!(miss_count("unknown"), b1);
        assert_eq!(miss_count("ttl_expiry"), b2);
    }

    /// No drift dims → the fall-through `unknown` bucket, so the buckets still
    /// sum to the total number of misses.
    #[test]
    fn driftless_recache_records_unknown() {
        let _guard = miss_metric_test_lock();
        let (b0, b1) = (miss_count("unknown"), miss_count("prefix_change"));

        let obs = UsageObserver::new();
        obs.begin_request("m-u1", "conv-unknown".into(), None, None, None);
        obs.complete("m-u1", 300, 0, 10_000, None);
        obs.begin_request("m-u2", "conv-unknown".into(), None, None, None);
        obs.complete("m-u2", 200, 0, 11_000, None);

        assert_eq!(miss_count("unknown"), b0 + 1);
        assert_eq!(miss_count("prefix_change"), b1);
    }

    /// `complete` has to hand the classification back, or the caller — which
    /// is the only thing that can reach durable storage — has nothing to
    /// persist and cache busts die with the process.
    #[test]
    fn complete_reports_a_structural_bust_to_the_caller() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        {
            let mut inner = obs.lock();
            inner.conversations.put(
                "conv-drift".to_string(),
                vec![TurnRecord {
                    cache_read_input_tokens: 10_000,
                    cache_creation_input_tokens: 2_000,
                    at: SystemTime::now(),
                    forwarded_request_bytes: None,
                    msgs: None,
                    diverged: false,
                    previous_boundary: None,
                    head: None,
                    stock_footprint: 12_000,
                }],
            );
        }
        // Drift dims present → the detector saw bytes move.
        obs.begin_request(
            "m-d1",
            "conv-drift".into(),
            None,
            Some("tools".to_string()),
            None,
        );
        let class = obs.complete("m-d1", 200, 0, 12_500, None);

        assert_eq!(
            class,
            Some(CompletionClass::PrefixChange {
                wasted_tokens: 12_000
            })
        );
        assert_eq!(class.unwrap().as_record(), ("prefix_change", 12_000));
    }

    /// A TTL expiry is reported too, but charges no waste — time passing is
    /// not something the proxy did.
    #[test]
    fn complete_reports_ttl_expiry_without_charging_waste() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        {
            let mut inner = obs.lock();
            inner.conversations.put(
                "conv-ttl2".to_string(),
                vec![TurnRecord {
                    cache_read_input_tokens: 10_000,
                    cache_creation_input_tokens: 2_000,
                    at: SystemTime::now() - (ANTHROPIC_CACHE_TTL + Duration::from_secs(10)),
                    forwarded_request_bytes: None,
                    msgs: None,
                    diverged: false,
                    previous_boundary: None,
                    head: None,
                    stock_footprint: 12_000,
                }],
            );
        }
        obs.begin_request("m-t9", "conv-ttl2".into(), None, None, None);
        let class = obs.complete("m-t9", 200, 0, 12_500, None);
        assert_eq!(class, Some(CompletionClass::TtlExpiry));
        assert_eq!(class.unwrap().as_record(), ("ttl_expiry", 0));
    }

    /// A healthy turn reports nothing, so the caller does no disk work on the
    /// common path.
    #[test]
    fn complete_reports_nothing_on_a_healthy_turn() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("m-h1", "conv-healthy".into(), None, None, None);
        assert_eq!(obs.complete("m-h1", 200, 10_000, 0, None), None);
    }

    /// An idle gap past the TTL is a real miss, bucketed `ttl_expiry`.
    #[test]
    fn ttl_expiry_records_ttl_expiry() {
        let _guard = miss_metric_test_lock();
        let (b0, b1) = (miss_count("ttl_expiry"), miss_count("prefix_change"));

        // Drive the pending/conversation state directly so the previous turn
        // can be dated older than the TTL without sleeping.
        let obs = UsageObserver::new();
        {
            let mut inner = obs.lock();
            inner.conversations.put(
                "conv-ttl".to_string(),
                vec![TurnRecord {
                    cache_read_input_tokens: 10_000,
                    cache_creation_input_tokens: 2_000,
                    at: SystemTime::now() - (ANTHROPIC_CACHE_TTL + Duration::from_secs(10)),
                    forwarded_request_bytes: None,
                    msgs: None,
                    diverged: false,
                    previous_boundary: None,
                    head: None,
                    stock_footprint: 12_000,
                }],
            );
        }
        obs.begin_request("m-t1", "conv-ttl".into(), None, None, None);
        obs.complete("m-t1", 200, 0, 12_500, None);

        assert_eq!(obs.snapshot().ttl_expiries_total, 1);
        assert_eq!(miss_count("ttl_expiry"), b0 + 1);
        assert_eq!(miss_count("prefix_change"), b1);
    }

    /// First turns and healthy turns are not misses and must not be counted.
    #[test]
    fn healthy_and_first_turns_record_nothing() {
        let _guard = miss_metric_test_lock();
        let before = [
            miss_count("ttl_expiry"),
            miss_count("prefix_change"),
            miss_count("unknown"),
        ];

        let obs = UsageObserver::new();
        // FirstTurn.
        obs.begin_request(
            "m-h1",
            "conv-healthy".into(),
            None,
            Some("tools".into()),
            None,
        );
        obs.complete("m-h1", 300, 0, 10_000, None);
        // Healthy: reads back everything turn 1 wrote.
        obs.begin_request(
            "m-h2",
            "conv-healthy".into(),
            None,
            Some("tools".into()),
            None,
        );
        obs.complete("m-h2", 200, 10_000, 800, None);

        assert_eq!(
            [
                miss_count("ttl_expiry"),
                miss_count("prefix_change"),
                miss_count("unknown"),
            ],
            before
        );
    }
}

#[cfg(test)]
mod prefix_fingerprint_tests {
    use super::*;
    use serde_json::json;

    fn body(system: &str, msgs: &[&str]) -> serde_json::Value {
        json!({
            "model": "claude-sonnet-5",
            "system": system,
            "tools": [{"name": "Read", "input_schema": {}}],
            "messages": msgs.iter().map(|m| json!({"role":"user","content":m}))
                .collect::<Vec<_>>(),
        })
    }

    /// The live tail must not participate, or every comparison says "different"
    /// and the field decides nothing. Two turns of one growing conversation
    /// share a stable region.
    #[test]
    fn appending_a_live_turn_leaves_the_fixed_depth_hash_alone() {
        // Longer than FINGERPRINT_FIXED_DEPTH, as any real conversation the
        // watchdog fires on will be (item 11's ran 55-121 messages).
        let base: Vec<String> = (0..12).map(|i| format!("msg {i}")).collect();
        let refs: Vec<&str> = base.iter().map(|s| s.as_str()).collect();
        let mut grown = refs.clone();
        grown.push("one more live turn");
        let turn_n = prefix_fingerprint(&body("sys", &refs));
        let turn_n1 = prefix_fingerprint(&body("sys", &grown));
        assert_eq!(turn_n.head, turn_n1.head);
        // The comparable field: one conversation, two turns, same value.
        assert_eq!(turn_n.body, turn_n1.body, "fixed-depth hash must be stable");
        // `stable` grows with the conversation, which is exactly why it cannot
        // be the comparator on its own — it is reported with its depth so a
        // reader knows when two values are even measured over the same span.
        assert_ne!(turn_n.stable, turn_n1.stable);
        assert_eq!(turn_n.stable_msgs, 11);
        assert_eq!(turn_n1.stable_msgs, 12);
    }

    /// Two conversations that share an opener — the exact shape
    /// `conversation_key` merges — must still be told apart.
    #[test]
    fn a_shared_opener_does_not_hide_different_work() {
        let mut a_msgs = vec!["same opener", "audit the cache"];
        let mut b_msgs = vec!["same opener", "rename a symbol"];
        for _ in 0..10 {
            a_msgs.push("filler");
            b_msgs.push("filler");
        }
        let a = prefix_fingerprint(&body("sys", &a_msgs));
        let b = prefix_fingerprint(&body("sys", &b_msgs));
        assert_eq!(a.head, b.head);
        assert_ne!(a.body, b.body, "merged key, different work: must diverge");
    }

    /// Reading 1 (real thrash): same leading block, different bodies past it.
    /// This is the case that means real money, so the hashes must diverge.
    #[test]
    fn same_head_different_bodies_diverge() {
        let a = prefix_fingerprint(&body("sys", &["a", "b", "tail"]));
        let b = prefix_fingerprint(&body("sys", &["a", "DIFFERENT", "tail"]));
        assert_eq!(a.head, b.head, "same system+tools");
        assert_ne!(a.stable, b.stable, "divergence past the tools block");
    }

    /// Reading 2 (artefact): byte-identical cacheable regions under one key.
    #[test]
    fn identical_requests_agree() {
        let a = prefix_fingerprint(&body("sys", &["a", "b", "tail"]));
        let b = prefix_fingerprint(&body("sys", &["a", "b", "tail"]));
        assert_eq!(a, b);
    }

    /// A changed system prompt is a head change, not a body change — item 3e's
    /// shape must land on the other side of the split.
    #[test]
    fn a_changed_system_prompt_moves_the_head_not_the_body() {
        let a = prefix_fingerprint(&body("sys one", &["a", "b", "tail"]));
        let b = prefix_fingerprint(&body("sys two", &["a", "b", "tail"]));
        assert_ne!(a.head, b.head);
        assert_eq!(a.stable, b.stable);
    }

    /// Divergence beyond the sampled window still has to register, or a long
    /// shared preamble would hide it.
    #[test]
    fn divergence_past_the_sample_window_still_registers() {
        let long = "x".repeat(FINGERPRINT_SAMPLE_BYTES * 4);
        let a = prefix_fingerprint(&body("sys", &[&format!("{long}AAA"), "tail"]));
        let b = prefix_fingerprint(&body("sys", &[&format!("{long}BBB"), "tail"]));
        // Same leading bytes and same length, so this is the collision the
        // sampling admits. Documented rather than asserted away: the field
        // tells live streams apart, it is not a content digest.
        assert_eq!(
            a.stable, b.stable,
            "known limit of sampling: equal len + equal prefix"
        );

        let c = prefix_fingerprint(&body("sys", &[&format!("{long}AAAA"), "tail"]));
        assert_ne!(a.stable, c.stable, "a length change must always register");
    }
}

/// End-to-end proof that the item 11 decider reaches the log line.
///
/// The hash tests above prove it discriminates; these prove it survives the
/// trip from the request side, through the parked entry, onto the event an
/// operator actually reads. A field that decides nothing because it never
/// arrives is the failure mode this whole document keeps running into.
#[cfg(test)]
mod prefix_on_recache_event_tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    #[derive(Default)]
    struct Captured {
        fields: Vec<String>,
    }

    struct CaptureFields(Arc<StdMutex<Captured>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureFields {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            struct V(String);
            impl tracing::field::Visit for V {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={:?} ", f.name(), v));
                }
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    self.0.push_str(&format!("{}={} ", f.name(), v));
                }
            }
            let mut v = V(String::new());
            event.record(&mut v);
            self.0.lock().unwrap().fields.push(v.0);
        }
    }

    /// Drive a real recache classification and assert the fingerprint is on the
    /// emitted event.
    #[test]
    fn a_recache_event_carries_the_prefix_fingerprint() {
        // These emit real recache events, which bump the process-global
        // cache-miss counter a sibling test reads as a delta. Share its lock.
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp = PrefixFingerprint {
            head: "aaaaaaaaaaaaaaaa".into(),
            body: "bbbbbbbbbbbbbbbb".into(),
            stable: "cccccccccccccccc".into(),
            stable_msgs: 42,
        };

        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            // Turn 1 establishes the prefix the next turn should read back.
            obs.begin_request(
                "r1",
                "conv-x".into(),
                Some("sess-x"),
                Some("tools".into()),
                Some(fp.clone()),
            );
            obs.complete("r1", 300, 0, 10_000, None);
            // Turn 2 reads back almost nothing while re-writing: a recache.
            obs.begin_request(
                "r2",
                "conv-x".into(),
                Some("sess-x"),
                Some("tools".into()),
                Some(fp.clone()),
            );
            obs.complete("r2", 200, 0, 11_000, None);
        });

        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("cache_recache_observed"))
            .unwrap_or_else(|| panic!("no recache event emitted; captured:\n{joined}"));

        assert!(
            line.contains("prefix_head=aaaaaaaaaaaaaaaa"),
            "head missing: {line}"
        );
        assert!(
            line.contains("prefix_body=bbbbbbbbbbbbbbbb"),
            "body missing: {line}"
        );
        assert!(
            line.contains("prefix_stable=cccccccccccccccc"),
            "stable missing: {line}"
        );
        assert!(
            line.contains("prefix_stable_msgs=42"),
            "depth missing: {line}"
        );
        // The join key. A recache event that cannot be matched to the drift
        // event explaining it is why items 5 and 11 stayed open for a week.
        let expected = super::super::drift_detector::session_key_log_prefix("sess-x");
        assert!(
            line.contains(&format!("session_key_hash={expected}")),
            "session key missing: {line}"
        );
    }

    /// The saving and the usage it should be priced against are produced on
    /// opposite sides of the request. Answering "is this worth running" meant
    /// correlating two log events after the fact, which is why the question
    /// stayed open. This asserts the one line that already contains the answer.
    #[test]
    fn a_compressed_turn_prices_its_saving_against_the_billed_usage() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("p1", "conv-price".into(), None, None, None);
            obs.note_compression("p1", 3_673, 2_176);
            // Live zone (2176) exceeds cache_creation + input (1015), so the
            // compressed span reaches into the cached prefix: the cheap case.
            obs.complete("p1", 2, 480_000, 1_013, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("savings_placement"))
            .unwrap_or_else(|| panic!("no placement event; captured:\n{joined}"));
        assert!(line.contains("tokens_freed=1497"), "{line}");
        assert!(
            line.contains("freed_past_cache_boundary=false"),
            "2176 forwarded against a 1015-token fresh region sits inside the \
             cached prefix, so the saving is the cheap kind: {line}"
        );
    }

    /// The valuable case must be distinguishable from the cheap one, or the
    /// field says nothing.
    #[test]
    fn a_saving_past_the_cache_boundary_is_marked_as_such() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("p2", "conv-price2".into(), None, None, None);
            obs.note_compression("p2", 5_000, 900);
            // Live zone (900) fits inside cache_creation + input (4002).
            obs.complete("p2", 2, 10_000, 4_000, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("savings_placement"))
            .expect("placement event");
        assert!(line.contains("freed_past_cache_boundary=true"), "{line}");
    }

    /// A hidden continuation round is billed like any other. The ledger is
    /// handed the client baseline for classification, so without the totals it
    /// reports less cache read than the pricing counterfactual, which sums
    /// every round off the outcome.
    #[test]
    fn the_cost_ledger_bills_continuation_rounds() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("ccr-1", "conv-ccr".into(), None, None, None);
            obs.note_wire_bytes("ccr-1", 100_000, 90_000, "all_messages");
            // Client turn read 200k; the continuation round read another 150k.
            obs.note_billed_totals("ccr-1", 20, 350_000, 4_000);
            obs.complete("ccr-1", 10, 200_000, 4_000, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("turn_cost_ledger"))
            .unwrap_or_else(|| panic!("no ledger event; captured:\n{joined}"));
        assert!(line.contains("cache_read_input_tokens=350000"), "{line}");
        assert!(line.contains("input_tokens=20"), "{line}");
        // 20 + 350000*0.1 + 4000*1.25 = 40020
        assert!(line.contains("billed_fresh_equivalents=40020"), "{line}");
    }

    /// The ledger exists because every other savings figure here is produced
    /// by the component doing the saving. This one must be built only from the
    /// provider's own usage numbers, or it is worth no more than the rest.
    #[test]
    fn the_cost_ledger_uses_only_the_providers_numbers() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("g1", "conv-ledger".into(), None, None, None);
            obs.note_wire_bytes("g1", 100_000, 90_000, "all_messages");
            // The compressor claims a huge saving; the ledger must ignore it.
            obs.note_compression("g1", 999_999, 1);
            obs.complete("g1", 10, 200_000, 4_000, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("turn_cost_ledger"))
            .unwrap_or_else(|| panic!("no ledger event; captured:\n{joined}"));
        // 10 + 200000*0.1 + 4000*1.25 = 25010
        assert!(line.contains("billed_fresh_equivalents=25010"), "{line}");
        assert!(line.contains("client_request_bytes=100000"), "{line}");
        assert!(line.contains("compression_mode=all_messages"), "{line}");
        // The compressor's claim must appear nowhere in it.
        assert!(
            !line.contains("999999"),
            "self-reported saving leaked in: {line}"
        );
    }

    /// A ledger that only appeared on turns the proxy did well on would be
    /// useless. It must be emitted for every completed turn.
    #[test]
    fn the_cost_ledger_is_emitted_even_when_nothing_was_compressed() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("g2", "conv-ledger2".into(), None, None, None);
            obs.complete("g2", 5, 1_000, 0, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        assert!(
            joined.contains("turn_cost_ledger"),
            "the ledger must not be conditional on a saving: {joined}"
        );
    }

    /// The proxy asks for the 1-hour cache tier, and asking is not granting.
    /// The flat creation count cannot tell a granted 1-hour write (2.0x input)
    /// from a downgraded 5-minute one (1.25x), so the split has to reach the
    /// log or the question stays unanswerable from the books.
    #[test]
    fn the_cost_ledger_names_the_ttl_the_write_was_billed_at() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("g3", "conv-ttl".into(), None, None, None);
            obs.complete("g3", 10, 0, 4_000, Some((1_000, 3_000)));
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("turn_cost_ledger"))
            .unwrap_or_else(|| panic!("no ledger event; captured:\n{joined}"));
        assert!(line.contains("cache_write_5m_tokens=1000"), "{line}");
        assert!(line.contains("cache_write_1h_tokens=3000"), "{line}");
    }

    /// A provider that publishes no breakdown must not read as one that wrote
    /// nothing at either tier — a zero here would be counted, and the count
    /// would be wrong.
    #[test]
    fn a_provider_without_a_ttl_breakdown_prints_minus_one_not_zero() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("g4", "conv-ttl-none".into(), None, None, None);
            obs.complete("g4", 10, 0, 4_000, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("turn_cost_ledger"))
            .unwrap_or_else(|| panic!("no ledger event; captured:\n{joined}"));
        assert!(line.contains("cache_write_5m_tokens=-1"), "{line}");
        assert!(line.contains("cache_write_1h_tokens=-1"), "{line}");
    }

    /// `unexplained_after_replay` carried a field set disjoint from the drift
    /// arm's, so the largest waste bucket was unexplained by construction:
    /// nothing on the line could be tested against, whatever the turn actually
    /// looked like. The evidence is already parked when this arm runs, so it
    /// must print it too.
    #[test]
    fn an_unexplained_event_carries_the_structural_evidence_the_drift_arm_prints() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp = PrefixFingerprint {
            head: "hhhhhhhhhhhhhhhh".into(),
            body: "dddddddddddddddd".into(),
            stable: "ssssssssssssssss".into(),
            stable_msgs: 17,
        };

        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request(
                "x1",
                "conv-unexplained".into(),
                None,
                None,
                Some(fp.clone()),
            );
            obs.complete("x1", 10_000, 46_985, 55_557, None);
            // A confirmed replay with no named cause is what lands on this arm.
            obs.begin_request(
                "x2",
                "conv-unexplained".into(),
                None,
                None,
                Some(fp.clone()),
            );
            obs.note_replay_applied("x2", ReplayAppliedEvidence::new(2, 2, 0));
            let class = obs.complete("x2", 9_714, 46_985, 48_669, None);
            assert_eq!(
                class,
                Some(CompletionClass::UnexplainedAfterReplay {
                    wasted_tokens: 48_669
                }),
                "the added fields must not move the classification"
            );
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .filter(|l| l.contains("cache_recache_observed"))
            .find(|l| l.contains("attribution_reason=provider_missed_newest_write"))
            .unwrap_or_else(|| panic!("no unexplained event; captured:\n{joined}"));

        assert!(line.contains("prefix_head=hhhhhhhhhhhhhhhh"), "{line}");
        assert!(line.contains("prefix_body=dddddddddddddddd"), "{line}");
        assert!(line.contains("prefix_stable=ssssssssssssssss"), "{line}");
        assert!(line.contains("prefix_stable_msgs=17"), "{line}");
        // Present-but-empty is the answer for a turn with neither, and a query
        // can only read that off a field that is always printed.
        assert!(line.contains("drift_dims="), "{line}");
        assert!(line.contains("replay_skipped="), "{line}");
    }

    /// The point of the fields above is that they can be non-empty here. A
    /// replay skip whose reason is not causal (`no_previous_turn` and friends)
    /// attributes nothing, so the turn still lands on the unexplained arm —
    /// and that reason is exactly the evidence the arm used to drop.
    #[test]
    fn an_unexplained_event_names_a_replay_skip_that_attributed_nothing() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("y1", "conv-unexplained-skip".into(), None, None, None);
            obs.complete("y1", 10_000, 46_985, 55_557, None);
            obs.begin_request("y2", "conv-unexplained-skip".into(), None, None, None);
            obs.note_replay_skip(
                "y2",
                ReplaySkipEvidence::from_inbound_original_histories(
                    ReplaySkip::NoPreviousTurn,
                    None,
                    &[serde_json::json!({"role":"user","content":"hi"})],
                ),
            );
            obs.note_replay_applied("y2", ReplayAppliedEvidence::new(3, 2, 0));
            obs.complete("y2", 9_714, 46_985, 48_669, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .filter(|l| l.contains("cache_recache_observed"))
            .find(|l| l.contains("attribution_reason=provider_missed_newest_write"))
            .unwrap_or_else(|| panic!("no unexplained event; captured:\n{joined}"));
        assert!(line.contains("replay_skipped=no_previous_turn"), "{line}");
    }

    /// Billed waste may never log as `expected`. On 2026-09-07 one
    /// conversation declined its replay on `system_adjacency_broken` for its
    /// whole life and re-cached itself 96 times; every event landed in the
    /// benign bucket at INFO with an empty `attribution_reason`, so 1,471,795
    /// charged tokens went by under a green statusline. The decline reason was
    /// in hand the entire time — the attribution ranking drops it on purpose,
    /// which is right, but dropping it must not also cost the event its
    /// severity.
    #[test]
    fn charged_waste_is_never_filed_as_an_expected_event() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("z1", "conv-uncaused-waste".into(), None, None, None);
        obs.complete("z1", 200, 0, 50_000, None);
        obs.begin_request("z2", "conv-uncaused-waste".into(), None, None, None);
        // A declined replay, so `note_replay_applied` never runs and the turn
        // cannot reach the unexplained-after-replay path. This is the exact
        // shape that filed 1.47M tokens as benign.
        obs.note_replay_skip(
            "z2",
            ReplaySkipEvidence::from_inbound_original_histories(
                ReplaySkip::SystemAdjacencyBroken,
                None,
                &[serde_json::json!({"role":"user","content":"tail"})],
            ),
        );
        obs.complete("z2", 200, 0, 50_000, None);
        let event = obs.snapshot().last_event.expect("event recorded");
        assert!(event.wasted_tokens > 0, "the turn must have been charged");
        assert_ne!(
            event.event_kind,
            RecacheEventKind::Expected,
            "waste filed as benign"
        );
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("system_adjacency_broken"),
            "the decline reason was dropped"
        );
    }

    /// A turn shorter than every tracked stream is booked a first turn and
    /// reports no waste. That may be right — a subagent forking off a shared
    /// opener had no prefix to reuse — but it is silent either way, and
    /// silence was how a re-written prefix came to look free. The event does
    /// not judge the turn; it makes the case countable.
    #[test]
    fn a_turn_shorter_than_every_stream_is_named_not_swallowed() {
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp = |msgs: usize| PrefixFingerprint {
            head: "hhhhhhhhhhhhhhhh".into(),
            body: "dddddddddddddddd".into(),
            stable: "ssssssssssssssss".into(),
            stable_msgs: msgs,
        };

        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("s1", "conv-short".into(), None, None, Some(fp(40)));
            obs.complete("s1", 10_000, 30_000, 41_000, None);
            // Half the length: matches nothing, so no waste is reported
            // however much the provider re-wrote.
            obs.begin_request("s2", "conv-short".into(), None, None, Some(fp(20)));
            obs.complete("s2", 0, 25_000, 26_000, None);
        });

        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("cache_stream_unmatched"))
            .unwrap_or_else(|| panic!("no unmatched event; captured:\n{joined}"));
        assert!(line.contains("turn_msgs=20"), "{line}");
        assert!(line.contains("longest_tracked=40"), "{line}");
        assert!(line.contains("cache_creation_input_tokens=26000"), "{line}");
        // The first turn had nothing to match against and is not the case
        // this event is for.
        assert_eq!(
            joined
                .lines()
                .filter(|l| l.contains("cache_stream_unmatched"))
                .count(),
            1,
            "{joined}"
        );
    }

    /// A turn parked without a fingerprint must not print a stale or invented
    /// one — an empty field reads as "not measured", which is the truth.
    #[test]
    fn a_turn_without_a_fingerprint_prints_empty_not_wrong() {
        // These emit real recache events, which bump the process-global
        // cache-miss counter a sibling test reads as a delta. Share its lock.
        let _guard = super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("r1", "conv-y".into(), None, Some("tools".into()), None);
            obs.complete("r1", 300, 0, 10_000, None);
            obs.begin_request("r2", "conv-y".into(), None, Some("tools".into()), None);
            obs.complete("r2", 200, 0, 11_000, None);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("cache_recache_observed"))
            .expect("recache event");
        assert!(
            line.contains("prefix_head= "),
            "expected empty head: {line}"
        );
        assert!(
            line.contains("prefix_stable_msgs=0"),
            "expected zero depth: {line}"
        );
        // A request that never reached the drift gate has no session hash to
        // print. Empty reads as "not measured"; inventing one would join to
        // nothing, which is the mistake this field was reverted for once.
        assert!(
            line.contains("session_key_hash= "),
            "expected empty session key: {line}"
        );
    }
}

/// Items 5 and 11: one `conversation_key` carries several streams, and each
/// turn must be judged against the stream it continues.
///
/// The sequences here are the live ones from 2026-08-09, not invented shapes.
#[cfg(test)]
mod stream_matching_tests {
    use super::*;

    fn fp(stable_msgs: usize) -> PrefixFingerprint {
        PrefixFingerprint {
            head: "head".into(),
            body: "body".into(),
            stable: format!("stable-{stable_msgs}"),
            stable_msgs,
        }
    }

    fn rec(msgs: Option<usize>) -> TurnRecord {
        TurnRecord {
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            at: SystemTime::now(),
            forwarded_request_bytes: None,
            msgs,
            diverged: false,
            previous_boundary: None,
            head: None,
            stock_footprint: 0,
        }
    }

    /// Replays conversation key `af7a42fd7eb2`: two streams, strictly
    /// alternating, each growing on its own. Every turn must land on the
    /// stream it continues, never on the other one.
    #[test]
    fn alternating_streams_each_match_their_own_predecessor() {
        let observed = [12, 10, 24, 18, 38, 28, 47, 34, 60, 44];
        let mut streams: Vec<TurnRecord> = Vec::new();
        // Which stream index each turn resolved to, in arrival order.
        let mut resolved = Vec::new();
        for msgs in observed {
            match match_stream(&streams, Some(msgs)) {
                Some(i) => {
                    streams[i] = rec(Some(msgs));
                    resolved.push(i);
                }
                None => {
                    streams.push(rec(Some(msgs)));
                    resolved.push(streams.len() - 1);
                }
            }
        }
        assert_eq!(
            streams.len(),
            2,
            "expected exactly two streams: {resolved:?}"
        );
        // Stream 0 took the 12/24/38/47/60 series, stream 1 the 10/18/28/34/44.
        assert_eq!(resolved, vec![0, 1, 0, 1, 0, 1, 0, 1, 0, 1]);
    }

    /// Conversation key `135358e7efd5`: the two streams share an identical
    /// first-8 fingerprint, so only the count tells them apart.
    #[test]
    fn streams_sharing_an_opener_are_still_separated() {
        let observed = [17, 16, 35, 28, 48, 36];
        let mut streams: Vec<TurnRecord> = Vec::new();
        let mut resolved = Vec::new();
        for msgs in observed {
            match match_stream(&streams, Some(msgs)) {
                Some(i) => {
                    streams[i] = rec(Some(msgs));
                    resolved.push(i);
                }
                None => {
                    streams.push(rec(Some(msgs)));
                    resolved.push(streams.len() - 1);
                }
            }
        }
        assert_eq!(streams.len(), 2);
        assert_eq!(resolved, vec![0, 1, 0, 1, 0, 1]);
    }

    /// A turn no shorter than a tracked stream continues it. This is the
    /// growth invariant the matcher rests on, stated directly.
    #[test]
    fn the_longest_stream_not_longer_than_this_turn_wins() {
        let streams = vec![rec(Some(10)), rec(Some(30)), rec(Some(20))];
        assert_eq!(match_stream(&streams, Some(25)), Some(2), "25 continues 20");
        assert_eq!(match_stream(&streams, Some(30)), Some(1), "a re-sent turn");
        assert_eq!(match_stream(&streams, Some(9)), None, "shorter than all");
    }

    /// Without a fingerprint there is nothing to match on, so the matcher must
    /// fall back to the previous single-stream behaviour rather than treating
    /// every turn as new.
    #[test]
    fn a_turn_without_a_count_falls_back_to_the_most_recent_stream() {
        let old = TurnRecord {
            at: SystemTime::now() - Duration::from_secs(60),
            ..rec(Some(10))
        };
        let streams = vec![old, rec(Some(30))];
        assert_eq!(match_stream(&streams, None), Some(1));
        assert_eq!(match_stream(&[], None), None);
    }

    /// The guard on the whole change: separating streams must not buy quiet at
    /// the cost of the busts this watchdog exists to catch. An edit inside the
    /// cached prefix leaves the message count alone, so the edited turn still
    /// matches its own stream and is still classified a re-cache.
    #[test]
    fn an_edit_inside_the_prefix_is_still_reported_as_a_bust() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request(
            "e1",
            "conv-edit".into(),
            None,
            Some("system".into()),
            Some(fp(40)),
        );
        obs.complete("e1", 300, 0, 50_000, None);
        // Same conversation, same length — one early message was rewritten.
        obs.begin_request(
            "e2",
            "conv-edit".into(),
            None,
            Some("system".into()),
            Some(fp(40)),
        );
        let class = obs.complete("e2", 300, 0, 50_000, None);
        assert!(
            matches!(class, Some(CompletionClass::PrefixChange { .. })),
            "an in-place prefix edit must still be a bust, got {class:?}"
        );
    }

    /// The false positive this change removes, end to end: two streams whose
    /// prefixes differ in size, interleaved under one key. Judged against the
    /// other stream's much larger prefix, the smaller stream's healthy turn
    /// looks like a collapse and was booked as waste.
    #[test]
    fn a_healthy_turn_is_not_charged_for_the_other_streams_prefix() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        // Stream A opens small, stream B opens five times larger.
        obs.begin_request("a1", "conv-mix".into(), None, None, Some(fp(17)));
        obs.complete("a1", 200, 0, 10_000, None);
        obs.begin_request("b1", "conv-mix".into(), None, None, Some(fp(16)));
        obs.complete("b1", 200, 0, 50_000, None);
        // A's next turn reads back exactly A's prefix and writes a small tail.
        obs.begin_request("a2", "conv-mix".into(), None, None, Some(fp(35)));
        let class = obs.complete("a2", 200, 10_000, 500, None);
        assert_eq!(
            class, None,
            "A continued A healthily; charging it against B's 50K prefix is item 11's artefact"
        );
    }

    /// Replacing only the inbound live tail creates a new branch cache. The
    /// provider reports creation tokens, but no reusable prefix was destroyed,
    /// so neither the health snapshot nor durable completion may call it waste.
    #[test]
    fn exact_inbound_final_message_replacement_is_a_zero_waste_tail_build() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("t1", "conv-tail".into(), None, None, Some(fp(3)));
        obs.complete("t1", 200, 0, 50_000, None);

        let prior = [
            serde_json::json!({"role":"user","content":"open"}),
            serde_json::json!({"role":"assistant","content":"answer"}),
            serde_json::json!({"role":"user","content":"old tail"}),
        ];
        let current = [
            prior[0].clone(),
            prior[1].clone(),
            serde_json::json!({"role":"user","content":"replacement tail"}),
        ];
        obs.begin_request("t2", "conv-tail".into(), None, None, Some(fp(3)));
        obs.note_replay_skip(
            "t2",
            ReplaySkipEvidence::from_inbound_original_histories(
                ReplaySkip::PrefixContentDiverged {
                    first_diff_index: 2,
                    replayed_prefix_msgs: 2,
                },
                Some(&prior),
                &current,
            ),
        );

        let class = obs.complete("t2", 200, 0, 50_000, None);
        assert_eq!(class, None, "a branch cache build is not a cache miss");

        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 1, "the cache build is recorded");
        assert_eq!(snap.recache_wasted_tokens_total, 0);
        let event = snap.last_event.expect("tail build event recorded");
        assert_eq!(event.event_kind, RecacheEventKind::Branch);
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("inbound_tail_replaced")
        );
        assert_eq!(event.origin.as_deref(), Some("inbound"));
        assert_eq!(event.scope.as_deref(), Some("final_message"));
        assert_eq!(event.wasted_tokens, 0);
        assert_eq!(event.cache_creation_input_tokens, 50_000);
    }

    #[test]
    fn inbound_tail_replacement_requires_equal_nonzero_counts_and_final_diff() {
        let one = [serde_json::json!({"role":"user","content":"one"})];
        let two = [
            one[0].clone(),
            serde_json::json!({"role":"assistant","content":"two"}),
        ];
        let empty: [serde_json::Value; 0] = [];

        assert!(!ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0,
                replayed_prefix_msgs: 0,
            },
            Some(&one),
            &two,
        )
        .is_inbound_tail_replacement());
        assert!(!ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0,
                replayed_prefix_msgs: 0,
            },
            Some(&empty),
            &empty,
        )
        .is_inbound_tail_replacement());
        assert!(!ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::ForwardedCountMismatch,
            Some(&one),
            &one,
        )
        .is_inbound_tail_replacement());
    }

    /// A bust whose divergence sits below the drift detector's window used to
    /// be filed as `Expected` — "no cause found" — and written off as a session
    /// reset. A declined prefix replay names that cause. Measured: 98% of the
    /// tokens in the supposedly-benign bucket were turns like this one.
    #[test]
    fn a_declined_replay_makes_an_unattributed_bust_a_named_one() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("s1", "conv-skip".into(), None, None, Some(fp(40)));
        obs.complete("s1", 200, 0, 50_000, None);
        // No drift dims: the detector saw nothing in system/tools/first-3.
        obs.begin_request("s2", "conv-skip".into(), None, None, Some(fp(41)));
        // But the prefix could not be replayed, which explains the bust.
        let prior = [
            serde_json::json!({"role":"user","content":"a"}),
            serde_json::json!({"role":"assistant","content":"b"}),
            serde_json::json!({"role":"user","content":"c"}),
        ];
        let current = [
            prior[0].clone(),
            serde_json::json!({"role":"assistant","content":"edited"}),
            prior[2].clone(),
        ];
        obs.note_replay_skip(
            "s2",
            ReplaySkipEvidence::from_inbound_original_histories(
                ReplaySkip::PrefixContentDiverged {
                    first_diff_index: 1,
                    replayed_prefix_msgs: 1,
                },
                Some(&prior),
                &current,
            ),
        );
        let class = obs.complete("s2", 200, 0, 50_000, None);
        assert!(
            matches!(class, Some(CompletionClass::PrefixChange { .. })),
            "a declined replay is a named cause, not an unattributable reset; got {class:?}"
        );
        let event = obs.snapshot().last_event.expect("event recorded");
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("prefix_content_diverged")
        );
        assert_eq!(event.event_kind, RecacheEventKind::Drift);
        assert_eq!(event.wasted_tokens, 50_000);
        assert_eq!(obs.snapshot().recache_wasted_tokens_total, 50_000);
    }

    #[test]
    fn non_causal_replay_skips_name_the_skip_without_ranking_it() {
        let _guard = super::tests::miss_metric_test_lock();
        for (i, reason) in [ReplaySkip::NoPreviousTurn].into_iter().enumerate() {
            let obs = UsageObserver::new();
            let conversation = format!("conv-non-causal-{i}");
            obs.begin_request("n1", conversation.clone(), None, None, Some(fp(40)));
            obs.complete("n1", 200, 0, 50_000, None);
            obs.begin_request("n2", conversation, None, None, Some(fp(41)));
            let current = [serde_json::json!({"role":"user","content":"tail"})];
            obs.note_replay_skip(
                "n2",
                ReplaySkipEvidence::from_inbound_original_histories(reason, None, &current),
            );

            obs.complete("n2", 200, 0, 50_000, None);
            // Still not ranked as a cause — origin and scope stay unset — but
            // the name survives to the event, and billed tokens keep it out of
            // the benign bucket.
            let event = obs.snapshot().last_event.expect("event recorded");
            assert_eq!(
                event.attribution_reason.as_deref(),
                Some("no_previous_turn"),
                "reason={reason:?}"
            );
            assert_eq!(
                event.event_kind,
                RecacheEventKind::Unexplained,
                "reason={reason:?}"
            );
        }
    }

    /// The other half of the same rule. With no drift dims and no declined
    /// replay there genuinely is no cause to name — but the rebuild was still
    /// billed, and the split that matters to a reader is charged vs free, not
    /// named vs unnamed. It says `no_cause_found` and logs at WARN.
    #[test]
    fn a_bust_with_no_cause_at_all_is_named_no_cause_found() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("u1", "conv-nocause".into(), None, None, Some(fp(40)));
        obs.complete("u1", 200, 0, 50_000, None);
        obs.begin_request("u2", "conv-nocause".into(), None, None, Some(fp(41)));
        obs.complete("u2", 200, 0, 50_000, None);
        let event = obs.snapshot().last_event.expect("event recorded");
        assert!(event.wasted_tokens > 0);
        assert_eq!(event.event_kind, RecacheEventKind::Unexplained);
        assert_eq!(event.attribution_reason.as_deref(), Some("no_cause_found"));
    }

    #[test]
    fn confirmed_replay_with_provider_shortfall_is_named_without_guessing() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("p1", "conv-provider".into(), None, None, Some(fp(20)));
        obs.note_wire_bytes("p1", 158_474, 147_638, "all_messages");
        obs.complete("p1", 10_000, 46_985, 55_557, None);

        obs.begin_request("p2", "conv-provider".into(), None, None, Some(fp(22)));
        obs.note_wire_bytes("p2", 167_578, 130_528, "all_messages");
        obs.note_replay_applied("p2", ReplayAppliedEvidence::new(2, 2, 0));
        let class = obs.complete("p2", 9_714, 46_985, 48_669, None);

        assert_eq!(
            class,
            Some(CompletionClass::UnexplainedAfterReplay {
                wasted_tokens: 48_669
            })
        );
        let event = obs.snapshot().last_event.expect("provider miss recorded");
        assert_eq!(event.event_kind, RecacheEventKind::Unexplained);
        // Read 46_985 both turns: the write p1 made was never found.
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("provider_missed_newest_write")
        );
        assert_eq!(event.origin.as_deref(), Some("unknown"));
        assert_eq!(event.scope.as_deref(), Some("replayed_prefix"));
        assert!(event.replayed_prefix);
        assert_eq!(event.replay_chain_id, Some(2));
        assert_eq!(event.breakpoints_placed, Some(2));
        assert_eq!(event.system_markers_dropped, Some(0));
        assert_eq!(event.previous_forwarded_request_bytes, Some(147_638));
        assert_eq!(event.forwarded_request_bytes, Some(130_528));
        assert_eq!(event.wasted_tokens, 48_669);
    }

    /// Noting a skip for a request the observer never parked must not panic or
    /// invent an entry — the replay stage runs on paths the observer skips.
    #[test]
    fn noting_a_skip_for_an_unknown_request_is_harmless() {
        let obs = UsageObserver::new();
        let current = [serde_json::json!({"role":"user","content":"tail"})];
        obs.note_replay_skip(
            "never-parked",
            ReplaySkipEvidence::from_inbound_original_histories(
                ReplaySkip::NoPreviousTurn,
                None,
                &current,
            ),
        );
        assert_eq!(obs.complete("never-parked", 1, 0, 0, None), None);
    }

    /// Memory bound: a key that keeps spawning streams must not grow forever.
    #[test]
    fn streams_per_conversation_are_capped() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        // Each turn is shorter than every tracked stream, so each starts a new
        // one — the worst case for the cap.
        for i in 0..(MAX_STREAMS_PER_CONVERSATION + 4) {
            let msgs = 500 - i * 10;
            obs.begin_request("c", "conv-cap".into(), None, None, Some(fp(msgs)));
            obs.complete("c", 100, 0, 1_000, None);
        }
        let inner = obs.lock();
        let streams = inner.conversations.peek("conv-cap").expect("key tracked");
        assert_eq!(streams.len(), MAX_STREAMS_PER_CONVERSATION);
    }
}

#[cfg(test)]
mod first_turn_attribution_tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    #[derive(Default)]
    struct Captured {
        lines: Vec<String>,
    }

    struct CaptureFields(Arc<StdMutex<Captured>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureFields {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            struct V(String);
            impl tracing::field::Visit for V {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={:?} ", f.name(), v));
                }
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    self.0.push_str(&format!("{}={} ", f.name(), v));
                }
            }
            let mut v = V(String::new());
            event.record(&mut v);
            self.0.lock().unwrap().lines.push(v.0);
        }
    }

    fn ctx(msgs: usize, hash: &str, compaction: bool) -> FirstTurnContext {
        FirstTurnContext {
            msgs,
            message_zero_hash: Some(hash.into()),
            compaction_restart: compaction,
            model: Some("claude-sonnet-5".into()),
        }
    }

    /// Run `f` against a fresh observer and return every
    /// `first_turn_write_observed` line it emitted.
    fn first_turn_lines(f: impl FnOnce(&UsageObserver)) -> Vec<String> {
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));
        tracing::subscriber::with_default(sub, || f(&UsageObserver::new()));
        let lines = cap.lock().unwrap().lines.clone();
        lines
            .into_iter()
            .filter(|l| l.contains("first_turn_write_observed"))
            .collect()
    }

    fn one_turn(obs: &UsageObserver, rid: &str, conv: &str, c: FirstTurnContext, creation: u64) {
        obs.begin_request(rid, conv.into(), Some(&format!("sess-{conv}")), None, None);
        obs.note_first_turn_context(rid, c);
        obs.complete(rid, 300, 0, creation, None);
    }

    #[test]
    fn reason_precedence() {
        let adoption = PrefixAdoption {
            donor_session_key_hash: "donor".into(),
        };
        // Compaction beats everything, even a found donor and a fan-out hit.
        assert_eq!(
            first_turn_reason(&ctx(2, "h", true), Some(&adoption), true),
            "compaction_restart"
        );
        assert_eq!(
            first_turn_reason(&ctx(2, "h", false), Some(&adoption), true),
            "session_key_drift"
        );
        assert_eq!(
            first_turn_reason(&ctx(2, "h", false), None, true),
            "identical_prompt_fanout"
        );
        assert_eq!(
            first_turn_reason(&ctx(2, "h", false), None, false),
            "fresh_session"
        );
        assert_eq!(
            first_turn_reason(&ctx(9, "h", false), None, false),
            "arrived_with_history"
        );
        // Fan-out only means something for an opener; a long history that
        // happens to share message 0 is still new content.
        assert_eq!(
            first_turn_reason(&ctx(9, "h", false), None, true),
            "arrived_with_history"
        );
    }

    #[test]
    fn fresh_session_write_is_attributed() {
        let lines =
            first_turn_lines(|obs| one_turn(obs, "r1", "conv-a", ctx(1, "h1", false), 9_000));
        assert_eq!(lines.len(), 1, "{lines:?}");
        let l = &lines[0];
        assert!(l.contains("attribution_reason=fresh_session"), "{l}");
        assert!(l.contains("msgs=1 "), "{l}");
        assert!(l.contains("cache_creation_input_tokens=9000"), "{l}");
        assert!(l.contains("model=claude-sonnet-5"), "{l}");
        assert!(l.contains("conversation_key=conv-a"), "{l}");
        assert!(!l.contains("adopted="), "no adoption ran: {l}");
    }

    #[test]
    fn compaction_restart_is_attributed() {
        let lines =
            first_turn_lines(|obs| one_turn(obs, "r1", "conv-a", ctx(1, "h1", true), 9_000));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("attribution_reason=compaction_restart"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn arrived_with_history_is_attributed() {
        let lines =
            first_turn_lines(|obs| one_turn(obs, "r1", "conv-a", ctx(17, "h1", false), 9_000));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("attribution_reason=arrived_with_history"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_found_donor_means_session_key_drift() {
        let lines = first_turn_lines(|obs| {
            obs.begin_request("r1", "conv-a".into(), Some("sess-a"), None, None);
            obs.note_first_turn_context("r1", ctx(17, "h1", false));
            obs.note_prefix_adoption(
                "r1",
                PrefixAdoption {
                    donor_session_key_hash: "d0n0r".into(),
                },
            );
            obs.complete("r1", 300, 0, 9_000, None);
        });
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert!(l.contains("attribution_reason=session_key_drift"), "{l}");
        assert!(l.contains("adopted=false"), "{l}");
        assert!(l.contains("donor_session_key_hash=d0n0r"), "{l}");
    }

    #[test]
    fn the_same_opener_under_another_key_is_fanout() {
        let lines = first_turn_lines(|obs| {
            one_turn(obs, "r1", "conv-a", ctx(1, "same", false), 9_000);
            one_turn(obs, "r2", "conv-b", ctx(1, "same", false), 9_000);
            // A different opener is a fresh session in its own right.
            one_turn(obs, "r3", "conv-c", ctx(1, "other", false), 9_000);
        });
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("attribution_reason=fresh_session"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("attribution_reason=identical_prompt_fanout"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("attribution_reason=fresh_session"),
            "{}",
            lines[2]
        );
    }

    #[test]
    fn a_second_turn_under_the_key_emits_nothing() {
        let lines = first_turn_lines(|obs| {
            one_turn(obs, "r1", "conv-a", ctx(1, "h1", false), 9_000);
            // Second turn re-writes the whole prefix: a recache, not a first
            // turn, and it must not be booked here.
            one_turn(obs, "r2", "conv-a", ctx(3, "h1", false), 12_000);
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("request_id=r1"), "{}", lines[0]);
    }

    #[test]
    fn context_is_read_off_the_body() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "This session is being continued from a previous conversation. Summary:"}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "go"}
            ]
        });
        let c = first_turn_context(&body);
        assert_eq!(c.msgs, 3);
        assert!(c.compaction_restart);
        assert_eq!(c.model.as_deref(), Some("claude-opus-5"));
        // The hash ignores cache_control, so two subagents whose openers
        // differ only in breakpoints still fan out together.
        let mut with_cc = body.clone();
        with_cc["messages"][0]["content"][0]["cache_control"] =
            serde_json::json!({"type": "ephemeral"});
        assert_eq!(
            c.message_zero_hash,
            first_turn_context(&with_cc).message_zero_hash
        );
        let plain = first_turn_context(
            &serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
        );
        assert!(!plain.compaction_restart);
        assert_ne!(plain.message_zero_hash, c.message_zero_hash);
        assert!(first_turn_context(&serde_json::json!({}))
            .message_zero_hash
            .is_none());
    }

    #[test]
    fn a_first_turn_that_wrote_nothing_is_silent() {
        let lines = first_turn_lines(|obs| {
            one_turn(
                obs,
                "r1",
                "conv-a",
                ctx(1, "h1", false),
                RECACHE_SLACK_TOKENS,
            );
        });
        assert!(lines.is_empty(), "{lines:?}");
    }
}

/// What the working-directory and role-sentence holds actually bought.
///
/// The counters live here rather than in a simulator because the holds are
/// previewed for the structural hash and then *restored*, so the fingerprint
/// the observer keeps is the client's own. Every turn below is a real verdict
/// the provider handed down, not a replay.
#[cfg(test)]
mod stabilization_meter_tests {
    use super::*;

    /// A fingerprint that differs from `fp` only in the hot zone.
    fn hot(head: &str) -> PrefixFingerprint {
        PrefixFingerprint {
            head: head.into(),
            body: "body".into(),
            stable: "stable".into(),
            stable_msgs: 4,
        }
    }

    /// The stabilisation meter is *observed*, not simulated. `head_changed`
    /// compares the fingerprint taken on the client-shaped body -- the holds
    /// are previewed for the structural hash and then restored before
    /// `begin_request` runs -- so it says the client moved its model, system
    /// or tools. When the provider reads the prefix back anyway, a hold
    /// absorbed the move, and that is worth exactly the prefix it saved.
    #[test]
    fn a_hot_zone_change_the_provider_read_through_is_an_absorbed_turn() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.complete("r1", 100, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("bbbb")));
        obs.complete("r2", 100, 10_000, 200, None);

        let s = obs.snapshot();
        assert_eq!(s.hot_zone_changes_total, 1);
        assert_eq!(s.stabilization_absorbed_total, 1);
        assert_eq!(s.hot_zone_recaches_total, 0);
        assert_eq!(
            s.stabilization_absorbed_tokens_total, 10_000,
            "an absorbed turn is worth the prefix that survived it"
        );
        assert_eq!(s.stabilization_absorb_pct, 100.0);
    }

    /// The other side of the same coin, and the one that keeps the meter
    /// honest: the hot zone moved and the provider threw the prefix away.
    #[test]
    fn a_hot_zone_change_that_busted_the_prefix_is_counted_against_us() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.complete("r1", 100, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("bbbb")));
        obs.complete("r2", 100, 0, 10_200, None);

        let s = obs.snapshot();
        assert_eq!(s.hot_zone_changes_total, 1);
        assert_eq!(s.hot_zone_recaches_total, 1);
        assert_eq!(s.stabilization_absorbed_total, 0);
        assert_eq!(s.stabilization_absorbed_tokens_total, 0);
        assert_eq!(s.stabilization_absorb_pct, 0.0);
    }

    /// A recache with a steady hot zone is somebody else's fault -- a body
    /// edit, a dropped tool result -- and must not be charged to the holds,
    /// or the rate reads as a hold failure every time the transcript churns.
    #[test]
    fn a_recache_with_a_steady_hot_zone_never_reaches_the_stabilization_meter() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.complete("r1", 100, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.complete("r2", 100, 0, 10_200, None);

        let s = obs.snapshot();
        assert_eq!(s.recache_events_total, 1, "it is still a recache");
        assert_eq!(s.hot_zone_changes_total, 0);
        assert_eq!(s.hot_zone_recaches_total, 0);
        assert_eq!(
            s.stabilization_absorb_pct, 100.0,
            "nothing judged yet, so the meter reports no failures rather than \
             a zero it cannot support"
        );
    }

    /// The denominator is `absorbed + recaches`, not every hot-zone change.
    /// A first turn has no prefix to lose and a TTL expiry lost it to the
    /// clock; scoring either would move the rate for reasons the holds had
    /// no say in.
    #[test]
    fn only_turns_the_holds_could_have_decided_are_in_the_denominator() {
        let obs = UsageObserver::new();

        // First turn on the conversation: a head change is unobservable,
        // there being nothing to compare against.
        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.complete("r1", 100, 0, 10_000, None);
        assert_eq!(obs.snapshot().hot_zone_changes_total, 0);

        // Two changes, one of each verdict.
        obs.begin_request("r2", "conv".into(), None, None, Some(hot("bbbb")));
        obs.complete("r2", 100, 10_000, 200, None);
        obs.begin_request("r3", "conv".into(), None, None, Some(hot("cccc")));
        obs.complete("r3", 100, 0, 10_500, None);

        let s = obs.snapshot();
        assert_eq!(s.hot_zone_changes_total, 2);
        assert_eq!(s.stabilization_absorbed_total, 1);
        assert_eq!(s.hot_zone_recaches_total, 1);
        assert_eq!(s.stabilization_absorb_pct, 50.0);
    }
}

/// The stock arm: what a plain Claude Code client would have been billed for
/// the same traffic, run beside the real request rather than instead of it.
#[cfg(test)]
mod stock_baseline_tests {
    use super::*;

    fn hot(head: &str) -> PrefixFingerprint {
        PrefixFingerprint {
            head: head.into(),
            body: "body".into(),
            stable: "stable".into(),
            stable_msgs: 4,
        }
    }

    /// Baseline: nothing was removed from the body and the hot zone held
    /// steady, so the two arms are the same request and must price the same.
    /// A comparison that shows a win here is measuring itself.
    #[test]
    fn a_turn_we_did_not_touch_prices_identically_in_both_arms() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 0, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r2", 1_000, 1_000, "off");
        obs.complete("r2", 100, 10_000, 200, None);

        let s = obs.snapshot();
        assert_eq!(s.stock_turns_compared, 2);
        assert_eq!(s.ours_effective_tokens, s.stock_effective_tokens);
        assert_eq!(s.vs_stock_saving_pct, 0.0);
    }

    /// A body we shrank. The stock client carries the whole thing every turn,
    /// so it writes a bigger prefix on the cold turn and reads a bigger one
    /// back on the warm turn -- both scaled from the wire bytes, which are
    /// measured at the point the request leaves.
    #[test]
    fn a_body_we_shrank_costs_the_stock_client_the_full_size_every_turn() {
        let obs = UsageObserver::new();

        // Client sent 2 KB, we forwarded 1 KB: stock's prompt is twice ours.
        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 2_000, 1_000, "on");
        obs.complete("r1", 0, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r2", 2_000, 1_000, "on");
        obs.complete("r2", 100, 10_000, 200, None);

        let s = obs.snapshot();
        // Ours: 10_000 written cold (x1.25), then 100 fresh + 10_000 read
        // (x0.1) + 200 written (x1.25).
        assert_eq!(s.ours_effective_tokens, 13_850);
        // Stock: 20_000 written cold, then 20_000 read back, 500 of growth
        // written, and the same 100-token fresh tail we were billed for.
        assert_eq!(s.stock_effective_tokens, 27_725);
        assert!(
            (s.vs_stock_saving_pct - 50.05).abs() < 0.01,
            "{}",
            s.vs_stock_saving_pct
        );
    }

    /// A 30-minute gap ends a five-minute prefix but not an hour one. Same
    /// billed turn, two different stock verdicts, decided by the noted tier —
    /// and the ours arm prices identically either way.
    #[test]
    fn stock_arm_honours_the_noted_tier_across_a_30min_gap() {
        for (ttl, want_stock) in [
            (
                crate::cache_stabilization::cache_ttl::ClientTtl::AllFiveMinutes,
                // Cold turn writes the whole 10_000 at 1.25x; the warm turn
                // rebuilds everything past a lapsed prefix at the same rate.
                12_500 + 200 + (12_500.0 * 1.25) as u64,
            ),
            (
                crate::cache_stabilization::cache_ttl::ClientTtl::AllOneHour,
                // Same cold turn, then a 10_000 read plus the 2_500 of
                // growth since, written at the hour rate.
                12_500 + 200 + 1_000 + 2_500 * 2,
            ),
        ] {
            let obs = UsageObserver::new();
            obs.begin_request("g0", "conv-gap".into(), None, None, Some(hot("aaaa")));
            obs.complete("g0", 0, 0, 10_000, None);
            {
                // Date the established footprint 30 minutes back: past the
                // five-minute horizon, inside the hour one.
                let mut inner = obs.lock();
                if let Some(streams) = inner.conversations.peek_mut("conv-gap") {
                    for rec in streams.iter_mut() {
                        rec.at = SystemTime::now() - Duration::from_secs(30 * 60);
                    }
                }
            }
            obs.begin_request("g1", "conv-gap".into(), None, None, Some(hot("aaaa")));
            obs.note_client_cache_ttl("g1", ttl);
            obs.complete("g1", 200, 12_000, 500, None);
            let s = obs.snapshot();
            assert_eq!(s.stock_effective_tokens, want_stock, "{ttl:?}");
            // Ours never depends on the noted tier: 12_500 cold plus
            // 200 fresh + 12_000 read + 500 written at 1.25x.
            assert_eq!(s.ours_effective_tokens, 12_500 + 2025);
        }
    }

    /// The holds' contribution, priced. The client moved its hot zone and our
    /// prefix survived; the stock client has no hold, so the same move costs
    /// it the whole prefix again.
    /// The window is the point: a win early in a session is diluted out of the
    /// lifetime ratio by every ordinary turn that follows, because an ordinary
    /// turn prices the same on both arms. The lifetime figure slides toward
    /// zero while nothing is getting worse, and only the window says so.
    #[test]
    fn ordinary_turns_dilute_the_lifetime_figure_but_empty_the_window() {
        let obs = UsageObserver::new();

        // One real win: client sent 2 KB, we forwarded 1 KB.
        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 2_000, 1_000, "on");
        obs.complete("r1", 0, 0, 10_000, None);
        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r2", 2_000, 1_000, "on");
        obs.complete("r2", 100, 10_000, 200, None);

        let after_win = obs.snapshot();
        assert!(after_win.vs_stock_saving_pct > 50.0);
        assert!(after_win.vs_stock_saving_pct_recent.unwrap() > 50.0);

        // Then a full window of cold/warm pairs we did not touch. Each pair
        // prices identically on both arms, so each contributes nothing either
        // way -- exactly the ordinary traffic that dilutes a lifetime ratio.
        for i in 0..(RECENT_SAMPLE_CAPACITY / 2) {
            let conv = format!("quiet{i}");
            let cold = format!("c{i}");
            let warm = format!("w{i}");
            obs.begin_request(&cold, conv.clone(), None, None, Some(hot("bbbb")));
            obs.note_wire_bytes(&cold, 1_000, 1_000, "off");
            obs.complete(&cold, 0, 0, 10_000, None);
            obs.begin_request(&warm, conv, None, None, Some(hot("bbbb")));
            obs.note_wire_bytes(&warm, 1_000, 1_000, "off");
            obs.complete(&warm, 100, 10_000, 200, None);
        }

        let s = obs.snapshot();
        assert_eq!(s.vs_stock_turns_recent, RECENT_SAMPLE_CAPACITY);
        // The win has been pushed out of the window entirely.
        assert_eq!(s.vs_stock_saving_pct_recent, Some(0.0));
        // But it is still in the lifetime figure, which is why that one keeps
        // reporting a saving the proxy is no longer making.
        assert!(
            s.vs_stock_saving_pct > 0.0,
            "lifetime still carries the win: {}",
            s.vs_stock_saving_pct
        );
        assert!(
            s.vs_stock_saving_pct < after_win.vs_stock_saving_pct,
            "and it decays toward the window: {} -> {}",
            after_win.vs_stock_saving_pct,
            s.vs_stock_saving_pct
        );
    }

    /// The hour we pay for is an hour we get. Pricing our write at 2.0x while
    /// handing the modelled client a prefix that never expires is the bias
    /// that drove this comparison steadily negative under
    /// `--force-1h-cache-ttl`; a gap past the five-minute tier has to cost the
    /// stock arm its cache.
    #[test]
    fn a_gap_only_an_hour_marker_survives_rebuilds_the_stock_prefix() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 0, 0, 10_000, None);

        // Twenty minutes later: past five minutes, inside the hour we pinned.
        obs.age_conversation("conv", Duration::from_secs(20 * 60));

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r2", 1_000, 1_000, "off");
        // Our prefix survived: we read it back and paid the hour's rate.
        obs.complete("r2", 100, 10_000, 200, Some((0, 200)));

        let s = obs.snapshot();
        let recent = s.vs_stock_saving_pct_recent.unwrap();
        assert!(
            recent > 0.0,
            "surviving a gap the stock client could not is a saving, not a \
             loss: {recent}"
        );
    }

    #[test]
    fn a_hot_zone_change_we_absorbed_is_a_full_rebuild_for_the_stock_client() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 0, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("bbbb")));
        obs.note_wire_bytes("r2", 1_000, 1_000, "off");
        obs.complete("r2", 100, 10_000, 200, None);

        let s = obs.snapshot();
        assert_eq!(s.stabilization_absorbed_total, 1, "sanity: we absorbed it");
        assert_eq!(s.ours_effective_tokens, 13_850);
        // Stock re-writes the whole 10_200 prefix rather than reading it.
        assert_eq!(s.stock_effective_tokens, 12_500 + 12_850);
        assert!(
            (s.vs_stock_saving_pct - 45.37).abs() < 0.01,
            "{}",
            s.vs_stock_saving_pct
        );
    }

    /// The self-check. The stock arm's one modelled rule is scored every turn
    /// against our own billed reads, so the counterfactual carries its own
    /// error bar instead of asking to be believed.
    #[test]
    fn the_read_rule_is_scored_against_our_own_billed_reads() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 0, 0, 10_000, None);
        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r2", 1_000, 1_000, "off");
        obs.complete("r2", 100, 10_000, 200, None);
        assert_eq!(
            obs.snapshot().predicted_read_error_pct,
            0.0,
            "the rule called this one exactly"
        );

        // Now a turn the rule gets wrong: it expects the whole 10_200 prefix
        // back and only half of it comes.
        obs.begin_request("r3", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r3", 1_000, 1_000, "off");
        obs.complete("r3", 100, 5_000, 5_300, None);

        let s = obs.snapshot();
        assert_eq!(s.stock_turns_compared, 3);
        // |10_200 - 5_000| of error against 15_000 of reads observed so far.
        assert!(
            (s.predicted_read_error_pct - 34.67).abs() < 0.01,
            "{}",
            s.predicted_read_error_pct
        );
    }

    /// Same-lane subagent streams sharing one conversation key must price
    /// against their own prefix, not each other's. The stock footprint used
    /// to be one `u64` per key, so a large stream following a small fork read
    /// the fork's prefix back and rebuilt the difference at 1.25x — reporting
    /// +38% saving on byte-identical traffic neither arm touched.
    #[test]
    fn interleaved_same_lane_streams_price_against_their_own_prefix() {
        fn lane_fp(msgs: usize) -> PrefixFingerprint {
            PrefixFingerprint {
                head: "aaaa".into(),
                body: "body".into(),
                stable: "stable".into(),
                stable_msgs: msgs,
            }
        }
        let obs = UsageObserver::new();

        // Main stream cold: 50 msgs, writes 50k.
        obs.begin_request("r1", "conv".into(), None, None, Some(lane_fp(50)));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 100, 0, 50_000, None);
        // Subagent fork on the same lane: 10 msgs, writes 8k. Shorter than
        // every tracked stream, so booked a first turn of its own stream.
        obs.begin_request("r2", "conv".into(), None, None, Some(lane_fp(10)));
        obs.note_wire_bytes("r2", 1_000, 1_000, "off");
        obs.complete("r2", 100, 0, 8_000, None);
        // Main stream warm: reads its own 50k back, writes 500 of growth.
        obs.begin_request("r3", "conv".into(), None, None, Some(lane_fp(52)));
        obs.note_wire_bytes("r3", 1_000, 1_000, "off");
        obs.complete("r3", 100, 50_000, 500, None);
        // Subagent warm: reads its own 8k back, writes 300 of growth.
        obs.begin_request("r4", "conv".into(), None, None, Some(lane_fp(12)));
        obs.note_wire_bytes("r4", 1_000, 1_000, "off");
        obs.complete("r4", 100, 8_000, 300, None);

        let s = obs.snapshot();
        assert_eq!(s.stock_turns_compared, 4);
        // Neither arm was touched and the traffic is identical, so both arms
        // price the same: 62_600 + 10_100 + 5_725 + 1_275.
        assert_eq!(s.ours_effective_tokens, 79_700);
        assert_eq!(s.ours_effective_tokens, s.stock_effective_tokens);
        assert_eq!(s.vs_stock_saving_pct, 0.0);
        assert_eq!(s.vs_stock_saving_pct_recent, Some(0.0));
    }

    /// Hidden continuation rounds are billed but are not in the client
    /// baseline `complete` receives. The stock client never runs them, so the
    /// stock arm stays on the baseline — but the ours arm must add them back,
    /// or the comparison reports less than the bill.
    #[test]
    fn hidden_continuation_rounds_count_on_the_ours_arm_only() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        // Baseline prices 100 + 10_000 + 200; the proxy burned an extra 5_000
        // fresh, 5_000 read and 500 written behind the client's back.
        obs.note_billed_totals("r1", 5_100, 15_000, 700);
        obs.complete("r1", 100, 10_000, 200, None);

        let s = obs.snapshot();
        // Ours: 1_350 baseline + 5_000 + 500 + 625 hidden.
        assert_eq!(s.ours_effective_tokens, 7_475);
        // Stock: first turn, full 10_200 rebuild at the 5-minute rate.
        assert_eq!(s.stock_effective_tokens, 12_850);
        assert!(
            (s.vs_stock_saving_pct - 41.83).abs() < 0.01,
            "{}",
            s.vs_stock_saving_pct
        );
    }
}
