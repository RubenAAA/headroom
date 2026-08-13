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

use crate::observability::proxy_counters::record_cache_miss_attribution;

use super::prefix_replay::ReplaySkip;

/// Provider label for the cache-miss attribution metric. This observer only
/// ever sees Anthropic usage counters (see the module docs), so the label is
/// constant rather than threaded through every call site.
const MISS_ATTRIBUTION_PROVIDER: &str = "anthropic";

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

/// Watchdog conversation key — SHA-256 over the auth-derived session key
/// plus the FIRST MESSAGE ONLY, deliberately excluding `system`.
///
/// This differs from [`crate::ctx::identity::conversation_key`] (which also
/// hashes `system`) on purpose: a mutated system prompt IS a cache bust, and
/// the watchdog can only classify it as one if the conversation identity
/// survives the mutation. Keying on `system` made the watchdog blind to
/// exactly that failure — the busted turn hashed to a fresh key and was
/// classified `FirstTurn` instead of `Recache` (proven live, 2026-07-04).
/// The CTX capture/injection stores keep the system-inclusive key; only the
/// watchdog needs bust-surviving identity.
pub fn conversation_key(parsed: &serde_json::Value, session_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    if let Some(first) = parsed.get("messages").and_then(|m| m.get(0)) {
        hasher.update(first.to_string().as_bytes());
    }
    let out = hasher.finalize();
    let mut s = String::with_capacity(16);
    for b in &out[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    use sha2::{Digest, Sha256};

    let mut head = Sha256::new();
    if let Some(model) = parsed.get("model").and_then(|v| v.as_str()) {
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
    let mut s = String::with_capacity(16);
    for b in &digest[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    let gap = now.duration_since(prev.at).unwrap_or(Duration::ZERO);
    if gap > ANTHROPIC_CACHE_TTL {
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
                ReplaySkip::PrefixContentDiverged { first_diff_index }
            ) if first_diff_index == prior_count - 1
        )
    }
}

/// Request-side context parked until the response's usage arrives.
#[derive(Debug, Clone)]
struct PendingRequest {
    conversation_key: String,
    /// The *drift detector's* session hash, parked verbatim so a recache
    /// event joins to the drift and volatile events on the same request.
    /// It must be `drift_detector::session_key_log_prefix(session_key)` and
    /// nothing else: an earlier attempt logged `hash(conversation_key)` under
    /// this name, which is a different value that joins to nothing and reads
    /// as "these events are unrelated". `None` when the request never reached
    /// the drift gate.
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
    replay_skip: Option<ReplaySkipEvidence>,
    replay_applied: Option<ReplayAppliedEvidence>,
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
        return RecacheAttribution {
            reason: Some(dims),
            origin: None,
            scope: None,
            counts_as_waste: true,
        };
    }

    let reason = replay_skip.map(|evidence| evidence.reason.as_str());
    let reason = match reason {
        Some(
            reason @ ("prefix_content_diverged"
            | "forwarded_count_mismatch"
            | "shorter_than_stored_prefix"
            | "optimized_shorter_than_prefix"),
        ) => Some(reason),
        _ => None,
    };
    if reason.is_none() && replay_applied.is_some() {
        return RecacheAttribution {
            reason: Some("provider_miss_after_replay"),
            origin: Some("provider_cache"),
            scope: Some("replayed_prefix"),
            counts_as_waste: true,
        };
    }
    RecacheAttribution {
        reason,
        origin: None,
        scope: None,
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
    ProviderMiss,
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
    /// Several streams can share one key — see [`match_stream`].
    conversations: LruCache<String, Vec<TurnRecord>>,
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
    ProviderMissAfterReplay { wasted_tokens: u64 },
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
            CompletionClass::ProviderMissAfterReplay { wasted_tokens } => {
                ("unknown", wasted_tokens.min(i64::MAX as u64) as i64)
            }
            CompletionClass::Unknown => ("unknown", 0),
        }
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
        session_key_hash: Option<String>,
        drift_dims: Option<String>,
        prefix: Option<PrefixFingerprint>,
    ) {
        self.lock().pending.put(
            request_id.to_string(),
            PendingRequest {
                conversation_key,
                session_key_hash,
                drift_dims,
                replay_skip: None,
                replay_applied: None,
                compression: None,
                client_request_bytes: None,
                forwarded_request_bytes: None,
                compression_mode: None,
                prefix,
            },
        );
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
    pub fn note_replay_applied(&self, request_id: &str, evidence: ReplayAppliedEvidence) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.replay_applied = Some(evidence);
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
    pub fn complete(
        &self,
        request_id: &str,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) -> Option<CompletionClass> {
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
            let billed_fresh_equivalents = input_tokens as f64
                + (cache_read_input_tokens as f64 * 0.1)
                + (cache_creation_input_tokens as f64 * 1.25);
            tracing::info!(
                event = "turn_cost_ledger",
                request_id = %request_id,
                conversation_key = %pending.conversation_key,
                // Anthropic's own numbers, unmodified.
                input_tokens = input_tokens,
                cache_read_input_tokens = cache_read_input_tokens,
                cache_creation_input_tokens = cache_creation_input_tokens,
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
        let (class, expected_cache_read, idle_gap, previous_forwarded_request_bytes) = {
            if inner.conversations.get(&pending.conversation_key).is_none() {
                inner
                    .conversations
                    .put(pending.conversation_key.clone(), Vec::new());
            }
            let streams = inner
                .conversations
                .get_mut(&pending.conversation_key)
                .expect("just inserted");
            let matched = match_stream(streams, turn_msgs);
            let outcome = match matched {
                None => (TurnClass::FirstTurn, 0, Duration::ZERO, None),
                Some(i) => {
                    let prev = streams[i];
                    (
                        classify_turn(
                            &prev,
                            now,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                        ),
                        prev.cache_read_input_tokens
                            .saturating_add(prev.cache_creation_input_tokens),
                        // How long this stream sat idle. On a TTL expiry it is
                        // the whole story: a five-minute-plus gap means the
                        // provider's cache died on its own.
                        now.duration_since(prev.at).unwrap_or(Duration::ZERO),
                        prev.forwarded_request_bytes,
                    )
                }
            };
            let record = TurnRecord {
                cache_read_input_tokens,
                cache_creation_input_tokens,
                at: now,
                forwarded_request_bytes: pending.forwarded_request_bytes,
                msgs: turn_msgs,
            };
            match matched {
                Some(i) => streams[i] = record,
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
                }
            }
            outcome
        };

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
                    pending.replay_skip,
                    pending.replay_applied,
                );
                let charged_wasted_tokens = if attribution.counts_as_waste {
                    wasted_tokens
                } else {
                    0
                };
                inner.recache_wasted_tokens_total += charged_wasted_tokens;
                let event_kind = if attribution.reason == Some("inbound_tail_replaced") {
                    RecacheEventKind::Branch
                } else if attribution.reason == Some("provider_miss_after_replay") {
                    RecacheEventKind::ProviderMiss
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
                            RecacheEventKind::ProviderMiss | RecacheEventKind::Expected => {
                                "unknown"
                            }
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
                        drift_dims = event.drift_dims.as_deref().unwrap_or(""),
                        replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
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
                        drift_dims = event.drift_dims.as_deref().unwrap_or(""),
                        replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
                        attribution_reason = "inbound_tail_replaced",
                        origin = "inbound",
                        scope = "final_message",
                        event_kind = "branch",
                        wasted_tokens = 0,
                        prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
                        prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
                        prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
                        prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
                        expected_cache_read = expected_cache_read,
                        actual_cache_read = cache_read_input_tokens,
                        cache_creation_input_tokens = cache_creation_input_tokens,
                        "prompt cache built for an inbound final-message replacement; branch creation, not waste"
                    ),
                    RecacheEventKind::ProviderMiss => tracing::warn!(
                        event = "cache_recache_observed",
                        request_id = %request_id,
                        conversation_key = %event.conversation_key,
                        session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                        attribution_reason = "provider_miss_after_replay",
                        origin = "provider_cache",
                        scope = "replayed_prefix",
                        event_kind = "provider_miss",
                        replayed_prefix = true,
                        replay_chain_id = event.replay_chain_id.unwrap_or(0),
                        breakpoints_placed = event.breakpoints_placed.unwrap_or(0),
                        system_markers_dropped = event.system_markers_dropped.unwrap_or(0),
                        previous_forwarded_request_bytes = event.previous_forwarded_request_bytes.unwrap_or(0),
                        forwarded_request_bytes = event.forwarded_request_bytes.unwrap_or(0),
                        wasted_tokens = charged_wasted_tokens,
                        expected_cache_read = expected_cache_read,
                        actual_cache_read = cache_read_input_tokens,
                        cache_creation_input_tokens = cache_creation_input_tokens,
                        "provider did not reuse the expected cache footprint after a confirmed prefix replay"
                    ),
                    RecacheEventKind::Expected => tracing::info!(
                        event = "cache_recache_observed",
                        request_id = %request_id,
                        conversation_key = %event.conversation_key,
                        session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
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
                    RecacheEventKind::ProviderMiss => {
                        CompletionClass::ProviderMissAfterReplay { wasted_tokens }
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
    use crate::observability::proxy_counters::cache_miss_attribution_for_test;

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
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        // Turn 1.
        obs.begin_request("req-1", "conv-a".into(), None, None, None);
        obs.complete("req-1", 300, 0, 10_000);
        // Turn 2: healthy.
        obs.begin_request("req-2", "conv-a".into(), None, None, None);
        obs.complete("req-2", 200, 10_000, 800);
        let snap = obs.snapshot();
        assert_eq!(snap.recache_events_total, 0);
        // Turn 3: recache, drift detector blamed tools.
        obs.begin_request("req-3", "conv-a".into(), None, Some("tools".into()), None);
        obs.complete("req-3", 200, 0, 11_000);
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

    #[test]
    fn recache_without_drift_dims_is_expected_kind() {
        // Subagent close / `/clear`: cache busted upstream but the
        // drift detector saw stable bytes → Expected, not Drift.
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("req-1", "conv-a".into(), None, None, None);
        obs.complete("req-1", 300, 0, 10_000);
        obs.begin_request("req-2", "conv-a".into(), None, None, None);
        obs.complete("req-2", 200, 0, 11_000);
        let ev = obs.snapshot().last_event.expect("event recorded");
        assert_eq!(ev.event_kind, RecacheEventKind::Expected);
    }

    #[test]
    fn recache_with_empty_string_drift_dims_is_expected_kind() {
        let _guard = miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("req-1", "conv-a".into(), None, Some(String::new()), None);
        obs.complete("req-1", 300, 0, 10_000);
        obs.begin_request("req-2", "conv-a".into(), None, Some(String::new()), None);
        obs.complete("req-2", 200, 0, 11_000);
        let ev = obs.snapshot().last_event.expect("event recorded");
        assert_eq!(ev.event_kind, RecacheEventKind::Expected);
    }

    #[test]
    fn concurrent_conversations_do_not_cross_talk() {
        // Two conversations from the same client (main session +
        // subagent) interleave; neither must flag the other.
        let obs = UsageObserver::new();
        obs.begin_request("req-a1", "conv-a".into(), None, None, None);
        obs.complete("req-a1", 300, 0, 50_000);
        obs.begin_request("req-b1", "conv-b".into(), None, None, None);
        obs.complete("req-b1", 300, 0, 2_000);
        obs.begin_request("req-a2", "conv-a".into(), None, None, None);
        obs.complete("req-a2", 200, 50_000, 900);
        obs.begin_request("req-b2", "conv-b".into(), None, None, None);
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
        obs.complete("r1", 10, 0, 8400);
        // Turn 2: healthy — reads what turn 1 created.
        obs.begin_request("r2", conversation_key(&body1, "sess"), None, None, None);
        obs.complete("r2", 10, 8400, 0);
        // Turn 3: mutated system → cache busted upstream (read 0, big creation).
        obs.begin_request("r3", conversation_key(&body3, "sess"), None, None, None);
        obs.complete("r3", 10, 0, 8410);

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
        obs.complete("m-d1", 300, 0, 10_000);
        obs.begin_request(
            "m-d2",
            "conv-drift".into(),
            None,
            Some("tools".into()),
            None,
        );
        obs.complete("m-d2", 200, 0, 11_000);

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
        obs.complete("m-u1", 300, 0, 10_000);
        obs.begin_request("m-u2", "conv-unknown".into(), None, None, None);
        obs.complete("m-u2", 200, 0, 11_000);

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
        let class = obs.complete("m-d1", 200, 0, 12_500);

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
                }],
            );
        }
        obs.begin_request("m-t9", "conv-ttl2".into(), None, None, None);
        let class = obs.complete("m-t9", 200, 0, 12_500);
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
        assert_eq!(obs.complete("m-h1", 200, 10_000, 0), None);
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
                }],
            );
        }
        obs.begin_request("m-t1", "conv-ttl".into(), None, None, None);
        obs.complete("m-t1", 200, 0, 12_500);

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
        obs.complete("m-h1", 300, 0, 10_000);
        // Healthy: reads back everything turn 1 wrote.
        obs.begin_request(
            "m-h2",
            "conv-healthy".into(),
            None,
            Some("tools".into()),
            None,
        );
        obs.complete("m-h2", 200, 10_000, 800);

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
                Some("ssssssssssssssss".into()),
                Some("tools".into()),
                Some(fp.clone()),
            );
            obs.complete("r1", 300, 0, 10_000);
            // Turn 2 reads back almost nothing while re-writing: a recache.
            obs.begin_request(
                "r2",
                "conv-x".into(),
                Some("ssssssssssssssss".into()),
                Some("tools".into()),
                Some(fp.clone()),
            );
            obs.complete("r2", 200, 0, 11_000);
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
        assert!(
            line.contains("session_key_hash=ssssssssssssssss"),
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
            obs.complete("p1", 2, 480_000, 1_013);
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
            obs.complete("p2", 2, 10_000, 4_000);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("savings_placement"))
            .expect("placement event");
        assert!(line.contains("freed_past_cache_boundary=true"), "{line}");
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
            obs.complete("g1", 10, 200_000, 4_000);
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
            obs.complete("g2", 5, 1_000, 0);
        });
        let joined = cap.lock().unwrap().fields.join("\n");
        assert!(
            joined.contains("turn_cost_ledger"),
            "the ledger must not be conditional on a saving: {joined}"
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
            obs.complete("r1", 300, 0, 10_000);
            obs.begin_request("r2", "conv-y".into(), None, Some("tools".into()), None);
            obs.complete("r2", 200, 0, 11_000);
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
        obs.complete("e1", 300, 0, 50_000);
        // Same conversation, same length — one early message was rewritten.
        obs.begin_request(
            "e2",
            "conv-edit".into(),
            None,
            Some("system".into()),
            Some(fp(40)),
        );
        let class = obs.complete("e2", 300, 0, 50_000);
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
        obs.complete("a1", 200, 0, 10_000);
        obs.begin_request("b1", "conv-mix".into(), None, None, Some(fp(16)));
        obs.complete("b1", 200, 0, 50_000);
        // A's next turn reads back exactly A's prefix and writes a small tail.
        obs.begin_request("a2", "conv-mix".into(), None, None, Some(fp(35)));
        let class = obs.complete("a2", 200, 10_000, 500);
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
        obs.complete("t1", 200, 0, 50_000);

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
                },
                Some(&prior),
                &current,
            ),
        );

        let class = obs.complete("t2", 200, 0, 50_000);
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
                first_diff_index: 0
            },
            Some(&one),
            &two,
        )
        .is_inbound_tail_replacement());
        assert!(!ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 0
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
        obs.complete("s1", 200, 0, 50_000);
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
                },
                Some(&prior),
                &current,
            ),
        );
        let class = obs.complete("s2", 200, 0, 50_000);
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
    fn non_causal_replay_skips_leave_the_bust_unattributed() {
        let _guard = super::tests::miss_metric_test_lock();
        for (i, reason) in [ReplaySkip::NoPreviousTurn].into_iter().enumerate() {
            let obs = UsageObserver::new();
            let conversation = format!("conv-non-causal-{i}");
            obs.begin_request("n1", conversation.clone(), None, None, Some(fp(40)));
            obs.complete("n1", 200, 0, 50_000);
            obs.begin_request("n2", conversation, None, None, Some(fp(41)));
            let current = [serde_json::json!({"role":"user","content":"tail"})];
            obs.note_replay_skip(
                "n2",
                ReplaySkipEvidence::from_inbound_original_histories(reason, None, &current),
            );

            assert_eq!(
                obs.complete("n2", 200, 0, 50_000),
                Some(CompletionClass::Unknown)
            );
            let event = obs.snapshot().last_event.expect("event recorded");
            assert_eq!(event.attribution_reason, None, "reason={reason:?}");
            assert_eq!(
                event.event_kind,
                RecacheEventKind::Expected,
                "reason={reason:?}"
            );
        }
    }

    /// The other half of the same rule: with no drift dims AND no declined
    /// replay there genuinely is no cause to name, and the event must stay
    /// `Expected` so the two buckets keep meaning different things.
    #[test]
    fn a_bust_with_no_cause_at_all_stays_expected() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("u1", "conv-nocause".into(), None, None, Some(fp(40)));
        obs.complete("u1", 200, 0, 50_000);
        obs.begin_request("u2", "conv-nocause".into(), None, None, Some(fp(41)));
        let class = obs.complete("u2", 200, 0, 50_000);
        assert_eq!(class, Some(CompletionClass::Unknown));
    }

    #[test]
    fn confirmed_replay_with_provider_shortfall_is_named_without_guessing() {
        let _guard = super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("p1", "conv-provider".into(), None, None, Some(fp(20)));
        obs.note_wire_bytes("p1", 158_474, 147_638, "all_messages");
        obs.complete("p1", 10_000, 46_985, 55_557);

        obs.begin_request("p2", "conv-provider".into(), None, None, Some(fp(22)));
        obs.note_wire_bytes("p2", 167_578, 130_528, "all_messages");
        obs.note_replay_applied("p2", ReplayAppliedEvidence::new(2, 2, 0));
        let class = obs.complete("p2", 9_714, 46_985, 48_669);

        assert_eq!(
            class,
            Some(CompletionClass::ProviderMissAfterReplay {
                wasted_tokens: 48_669
            })
        );
        let event = obs.snapshot().last_event.expect("provider miss recorded");
        assert_eq!(event.event_kind, RecacheEventKind::ProviderMiss);
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("provider_miss_after_replay")
        );
        assert_eq!(event.origin.as_deref(), Some("provider_cache"));
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
        assert_eq!(obs.complete("never-parked", 1, 0, 0), None);
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
            obs.complete("c", 100, 0, 1_000);
        }
        let inner = obs.lock();
        let streams = inner.conversations.peek("conv-cap").expect("key tracked");
        assert_eq!(streams.len(), MAX_STREAMS_PER_CONVERSATION);
    }
}
