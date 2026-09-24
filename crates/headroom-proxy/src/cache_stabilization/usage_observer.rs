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

#[path = "usage_observer_attribution.rs"]
mod attribution;
#[path = "usage_observer_events.rs"]
mod events;
#[path = "usage_observer_fingerprint.rs"]
mod fingerprint;
#[path = "usage_observer_first_turn.rs"]
mod first_turn;
#[path = "usage_observer_keys.rs"]
mod keys;
#[path = "usage_observer_replay.rs"]
mod replay;
#[path = "usage_observer_turn.rs"]
mod turn;
#[path = "usage_observer_write_split.rs"]
mod write_split;

use self::attribution::recache_attribution;
use self::attribution::CacheLanding;
use self::attribution::RecacheAttribution;
use self::events::CacheHealthSnapshot;
use self::events::CompletedTurn;
use self::events::CompletionClass;
use self::events::CostSample;
use self::events::RecacheEvent;
pub use self::events::RecacheEventKind;
use self::events::RecentHitRateSample;
use self::events::MISS_ATTRIBUTION_PROVIDER;
use self::fingerprint::hex16;
use self::fingerprint::PrefixFingerprint;
use self::first_turn::first_turn_reason;
use self::first_turn::FirstTurnContext;
use self::keys::DigestSink;
use self::keys::CACHE_READ_MULTIPLIER;
use self::keys::CACHE_WRITE_1H_MULTIPLIER;
use self::keys::CACHE_WRITE_5M_MULTIPLIER;
use self::keys::COMMIT_LATENCY_WINDOW;
use self::keys::CONVERSATION_CAPACITY;
use self::keys::FIRST_TURN_OPENER_CAPACITY;
use self::keys::IDENTICAL_PROMPT_FANOUT_WINDOW;
use self::keys::IN_FLIGHT_HORIZON;
use self::keys::PENDING_CAPACITY;
use self::keys::RECENT_COMPLETION_CAPACITY;
use self::keys::RECENT_SAMPLE_CAPACITY;
use self::keys::SIBLING_COMPLETION_WINDOW;
use self::turn::classify_turn;
use self::turn::match_stream;
use self::turn::TurnClass;
use self::turn::TurnRecord;
use self::turn::MAX_STREAMS_PER_CONVERSATION;
use self::write_split::split_cache_write;
use self::write_split::RECACHE_SLACK_TOKENS;
use self::write_split::UNEARNED_WRITE_FLOOR_TOKENS;

pub use self::events::ActiveConversation;
pub use self::events::ANTHROPIC_CACHE_TTL;
pub use self::events::ANTHROPIC_CACHE_TTL_1H;
pub use self::fingerprint::prefix_fingerprint;
pub use self::fingerprint::prefix_fingerprint_with_model;
pub use self::first_turn::first_turn_context;
pub use self::first_turn::PrefixAdoption;
pub use self::keys::conversation_key;
pub use self::replay::ReplayAppliedEvidence;
pub use self::replay::ReplaySkipEvidence;

/// A completed turn's fingerprint, digested for the per-stream comparison
/// in [`UsageObserver::complete_with_cache_capability`].
#[derive(Debug, Clone, Copy)]
struct TurnFingerprint {
    msgs: Option<usize>,
    head: Option<u64>,
    head_model: Option<u64>,
    head_system: Option<u64>,
    head_tools: Option<u64>,
    beta: Option<u64>,
    markers: Option<u64>,
    forward_model: Option<u64>,
}

/// Billed-usage inputs for one completing turn, bundled so the stream-match
/// helper takes four arguments instead of nine.
/// Extracted from `complete_with_cache_capability` without behavior change.
struct TurnUsage<'a> {
    request_id: &'a str,
    input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_write_ttl_split: Option<(u64, u64)>,
    cache_ttl: Duration,
    now: SystemTime,
}

/// Stream match + classification for one completing turn: which tracked
/// stream this turn continues, what the provider billed last turn, and which
/// head components moved.
/// Extracted from `complete_with_cache_capability` without behavior change.
struct StreamMatch {
    class: TurnClass,
    expected_cache_read: u64,
    idle_gap: Duration,
    previous_forwarded_request_bytes: Option<u64>,
    previous_turn_diverged: bool,
    previous_turn_had_continuation: bool,
    previous_cache_read: u64,
    previous_previous_boundary: Option<u64>,
    head_changed: bool,
    head_model_changed: bool,
    head_system_changed: bool,
    head_tools_changed: bool,
    beta_changed: bool,
    markers_changed: bool,
    model_changed: bool,
    matched_stream_msgs: Option<usize>,
    streams_tracked: usize,
    matched_stream_idx: usize,
    matched_stock_prior: u64,
}

/// Per-component head comparison between a tracked previous turn and the
/// completing turn's fingerprint. See `compare_heads` for the rule.
#[derive(Debug, Clone, Copy)]
struct HeadMoves {
    head: bool,
    head_model: bool,
    head_system: bool,
    head_tools: bool,
    beta: bool,
    markers: bool,
    forward_model: bool,
}

/// Request-side context parked until the response's usage arrives.
#[derive(Debug, Clone)]
struct PendingRequest {
    conversation_key: String,
    /// Canonical project directory for this turn (see
    /// [`crate::proxy::resolve_ctx_project`]), filled by [`UsageObserver::note_project`]
    /// right after [`UsageObserver::begin_request`] parks the entry. `None`
    /// when the caller never resolved one — the endpoint reports those turns
    /// without a project rather than dropping them.
    project: Option<String>,
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
    /// A turn of this same conversation completed within
    /// [`COMMIT_LATENCY_WINDOW`] before this one began. The provider may not
    /// have committed that turn's cache write yet even though nothing is still
    /// in flight, so a shortfall here is timing-suspect. Witness only: it does
    /// not attribute a recache, it lets an offline query separate commit races
    /// (`commit_race_suspect` + `provider_missed_newest_write`) from evictions.
    commit_race_suspect: bool,
    /// A turn under a *different* conversation key of the same session
    /// completed within [`SIBLING_COMPLETION_WINDOW`] before this one began.
    /// Same-opener subagent streams share one key by construction, so this is
    /// either a re-keyed continuation (compaction, model switch, system
    /// rewrite minted a fresh key) or genuinely different work on one session.
    /// Witness only, for the same offline join as above.
    sibling_completed_recently: bool,
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
    /// Short hashes of the cache-key inputs that sit outside the drift
    /// dimensions: the forwarded `anthropic-beta` header value and the
    /// `cache_control` marker layout, as logged by `turn_cache_fingerprint`.
    /// Both are provider cache-key inputs the drift detector never sees (it
    /// hashes the body only, and strips `cache_control`), so a rotation here
    /// busts the cache with both lanes quiet. Set by
    /// [`UsageObserver::note_forward_witnesses`]; `None` on a turn that never
    /// reached the fingerprint stage (birth turns, routed forwards).
    forward_beta: Option<String>,
    forward_markers: Option<String>,
    /// The forwarded `model` string itself (small cardinality — logged raw,
    /// not hashed), from the same stage. The cost-aware router can rewrite
    /// it after the compared fingerprint is taken, so only this post-router
    /// value speaks for what the provider keyed on. Witness only, like the
    /// two above: unranked until a first real flap exists.
    forward_model: Option<String>,
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
    /// False when this turn bills from a non-Anthropic cache universe
    /// (routed/OpenAI translation): no cache-creation counter, no TTL
    /// telemetry, different pricing and retention. The watchdog still
    /// scores the turn, but the Anthropic-priced stock arm stays out —
    /// pricing it at 1.25x/2.0x with 5m/1h horizons would invent a write
    /// premium the provider never charged.
    stock_eligible: bool,
}

struct Inner {
    pending: LruCache<String, PendingRequest>,
    /// Cleanly completed turns, newest last, for the commit-race and sibling
    /// recency witnesses computed in [`UsageObserver::begin_request`].
    /// Pruned by window on read, capped on write.
    recently_completed: VecDeque<CompletedTurn>,
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
    ///
    /// Value is the evicted prefix footprint (last turn's read + creation, 0
    /// when unknown), so the forgotten floor can be token-sized offline.
    forgotten: LruCache<String, u64>,
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
                recently_completed: VecDeque::with_capacity(RECENT_COMPLETION_CAPACITY),
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
        // Recency witnesses against clean completions. Pruned by window here;
        // a stale entry is not evidence of anything. These never attribute —
        // they only ride along so an unexplained shortfall can be checked
        // against a commit race or a sibling re-key without log mining.
        let session_hash = session_key.map(super::drift_detector::session_key_log_prefix);
        let mut commit_race_suspect = false;
        let mut sibling_completed_recently = false;
        inner
            .recently_completed
            .retain(|c| now.duration_since(c.completed_at) < SIBLING_COMPLETION_WINDOW);
        for c in inner.recently_completed.iter() {
            let age = now.duration_since(c.completed_at);
            if c.conversation_key == conversation_key && age < COMMIT_LATENCY_WINDOW {
                commit_race_suspect = true;
            } else if session_hash.as_deref().is_some_and(|h| {
                Some(h) == c.session_key_hash.as_deref() && c.conversation_key != conversation_key
            }) && age < SIBLING_COMPLETION_WINDOW
            {
                sibling_completed_recently = true;
            }
            if commit_race_suspect && sibling_completed_recently {
                break;
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
                commit_race_suspect,
                sibling_completed_recently,
                conversation_key,
                project: None,
                first_turn: None,
                adoption: None,
                session_key_hash: session_hash,
                drift_dims,
                outbound_drift_dims: None,
                forward_beta: None,
                forward_markers: None,
                forward_model: None,
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
                stock_eligible: true,
            },
        );
        if let Some((evicted_id, _)) = evicted {
            if evicted_id != request_id {
                inner.abandoned_requests_total += 1;
            }
        }
    }

    /// Attach the resolved project directory to an already-parked turn.
    ///
    /// Split from [`begin_request`](Self::begin_request) so that method's
    /// signature — and its hundred-odd test call sites — stays put: project
    /// resolution needs the headers plus the parsed body, which the two
    /// production callers have in hand right after parking. No entry (shed,
    /// completed, or unknown id) is a silent no-op, never a panic.
    pub fn note_project(&self, request_id: &str, project: String) {
        let mut inner = self.lock();
        if let Some(entry) = inner.pending.get_mut(request_id) {
            entry.project = Some(project);
        }
    }

    /// Snapshot every turn still in flight: parked, under the horizon, and
    /// not yet completed. Sorted oldest-first so the longest-running turn
    /// leads. Pure read under one lock; the endpoint formats it.
    pub fn active_conversations(&self) -> Vec<ActiveConversation> {
        let inner = self.lock();
        let now = Instant::now();
        let mut out: Vec<ActiveConversation> = inner
            .pending
            .iter()
            .filter_map(|(_, p)| {
                let age = now.duration_since(p.began);
                if age >= IN_FLIGHT_HORIZON {
                    return None;
                }
                Some(ActiveConversation {
                    conversation: p.conversation_key.clone(),
                    project: p.project.clone(),
                    age_secs: age.as_secs(),
                })
            })
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.age_secs));
        out
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

    /// Mark a parked turn as billed outside the Anthropic cache universe, so
    /// the stock arm skips it. The watchdog (hit rate, recache, earned
    /// write, first turn) still scores the turn; only the comparison built
    /// on Anthropic read/write multipliers and TTL horizons stays out.
    pub fn note_stock_ineligible(&self, request_id: &str) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.stock_eligible = false;
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

    /// Record the forwarded beta-header digest and marker layout for this
    /// turn — the two provider cache-key inputs neither drift lane sees.
    ///
    /// Called from the `turn_cache_fingerprint` stage, which already computed
    /// both strings for the log. Short opaque digests only, never header
    /// values. A turn that never reaches that stage keeps `None`, which
    /// compares as "not known", never as "unchanged".
    pub fn note_forward_witnesses(
        &self,
        request_id: &str,
        beta: String,
        markers: String,
        model: String,
    ) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(request_id) {
            pending.forward_beta = Some(beta);
            pending.forward_markers = Some(markers);
            pending.forward_model = Some(model);
        }
    }

    /// Digest a witness string for the per-stream comparison in `TurnRecord`.
    /// In-process only, like the stream matching it serves.
    fn witness_digest(value: &str) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// Normalized marker layout for the `markers_changed` witness.
    ///
    /// The raw layout string (`sys[1]:1h,m0.2:1h,m56.2:1h`) embeds message
    /// indices, and the tail breakpoint follows conversation growth by
    /// design — every appended message renumbers the tail entries, so the
    /// raw digest differs on consecutive turns of any growing conversation
    /// (measured 2026-09-18: true on 41/41 residual events, all pure
    /// growth). Comparing raw strings makes the witness a growth detector,
    /// not a rotation detector.
    ///
    /// What actually voids the provider prefix is the breakpoint *shape*:
    /// how many breakpoints exist, of which kind, at which TTL. Normalize
    /// to that (`n=4|m:1h,m:1h,m:1h,sys:1h`) and digest the normalized
    /// form. A TTL downgrade, a dropped marker, or a kind change still
    /// flags; pure growth no longer does. Entries of unknown shape keep
    /// their full text as the kind, so a future format change fails loud
    /// (flags) rather than blind (never flags).
    fn normalize_marker_layout(markers: &str) -> String {
        let mut kinds: Vec<String> = markers
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| {
                let (loc, ttl) = e.rsplit_once(':').unwrap_or((e, ""));
                let kind = if loc.starts_with("sys") {
                    "sys"
                } else if loc.starts_with("tools") {
                    "tools"
                } else if loc.starts_with('m') {
                    "m"
                } else {
                    // Unknown shape: stay sensitive, not silent.
                    loc
                };
                if ttl.is_empty() {
                    kind.to_string()
                } else {
                    format!("{kind}:{ttl}")
                }
            })
            .collect();
        kinds.sort();
        format!("n={}|{}", kinds.len(), kinds.join(","))
    }

    /// Digest of the marker layout's cache-relevant shape (see
    /// [`Self::normalize_marker_layout`]). Both sides of the per-stream
    /// comparison go through this, so shape-equal layouts compare equal
    /// however far the tail indices advanced.
    fn marker_layout_digest(layout: &str) -> u64 {
        Self::witness_digest(&Self::normalize_marker_layout(layout))
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
        Self::record_hit_rate(
            &mut inner,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_capable,
        );

        let Some(pending) = Self::pop_pending(&mut inner, request_id) else {
            // Request never went through the compression gate
            // (compression off, non-JSON, …) — no conversation
            // identity, so no per-turn classification. The rolling
            // rate above still counted it.
            return None;
        };

        // Log this clean completion for the recency witnesses: a same-key
        // turn beginning within `COMMIT_LATENCY_WINDOW` may be racing the
        // provider's commit of this write, and a sibling key of the same
        // session completing nearby is the re-key/fan-out join. Only clean
        // completions reach here — abandoned entries never pop — so this is
        // exactly the committed set.
        Self::note_completion(&mut inner, &pending, now_instant);

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
        Self::record_savings_placement(
            request_id,
            &pending,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        );

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
        Self::record_cost_ledger(
            &mut inner,
            request_id,
            &pending,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_write_ttl_split,
        );

        // Classify against the stream this turn continues, not against
        // whatever turn happened to arrive last under the same key.
        let fingerprint = Self::fingerprint_turn(&pending);
        let turn_msgs = fingerprint.msgs;
        let usage = TurnUsage {
            request_id,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            cache_write_ttl_split,
            cache_ttl,
            now,
        };
        let stream_match =
            Self::match_and_classify_turn(&mut inner, &pending, &fingerprint, &usage);
        let head_moved_which = Self::head_moved_string(&stream_match);
        Self::price_stock_arm(
            &mut inner,
            &pending,
            &stream_match,
            &usage,
            &head_moved_which,
        );
        let class = &stream_match.class;
        let expected_cache_read = stream_match.expected_cache_read;
        let head_changed = stream_match.head_changed;
        let streams_tracked = stream_match.streams_tracked;

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
            Self::count_hot_zone_turn(&mut inner, class, expected_cache_read);
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
        // Anthropic-billed turns only. Routed/OpenAI turns bill from a
        // different cache universe (no creation counter, no TTL split,
        // different pricing and retention); the watchdog above still
        // scores them, but the comparison below is priced in Anthropic
        // input-equivalents and must not touch them.
        // Every completed turn that wrote anything, not just the healthy ones.
        // Gating on `Healthy` made this dead arithmetic: healthy means
        // `read + RECACHE_SLACK_TOKENS >= previous footprint`, which forces
        // `unearned <= RECACHE_SLACK_TOKENS` — under the warning floor, always.
        // The turns actually re-writing ground they already held are the ones
        // the gate threw away. Recache turns are counted here *and* by the
        // recache detector; the two measure different things (this one, tokens
        // re-written; that one, prefix not read) and must not be added up.
        Self::record_unearned_write(
            &mut inner,
            request_id,
            &pending,
            class,
            expected_cache_read,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        );
        // First completed turn under this key. The recache classifier has
        // nothing to score it against, so without this its cache write —
        // 41% of all write tokens, live — went unattributed.
        Self::record_first_turn_write(
            &mut inner,
            &pending,
            request_id,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            streams_tracked,
            now_instant,
        );

        // D0 diagnostic for first-turn-write-sharing.md: one line per turn
        // classified FirstTurn, with none of the write branch's gates
        // (`streams_tracked == 0`, >64 tokens), so tracked first turns (the
        // arrived-with-history shape the counters never see) and tiny turns
        // are measured too. Pure observation: no counter moves here, the
        // fan-out table is untouched, and the `reason` from
        // `first_turn_reason` is derived offline from the inputs below
        // (message_zero_hash joins across conversations for the fan-out
        // check) rather than recomputed against live tables.
        //
        // Deliberately joined, not self-contained: marker layout rides on
        // `turn_cache_fingerprint`, forwarded sys/tools hashes on
        // `prefix_composition`, and beta/auth digests on the former — all keyed
        // by request_id. This line carries only what no other line has: the
        // request-path cache-key controls, the outer-vs-rounds usage split the
        // ledger folds together, and the message-0 composition sizes.
        Self::emit_first_turn_diagnostic(&pending, request_id, &usage, class);

        Self::finish_completion(
            &mut inner,
            &pending,
            &stream_match,
            &usage,
            &head_moved_which,
            turn_msgs,
        )
    }

    /// Book the turn's completion class: TTL expiries pass through, recaches
    /// are attributed, booked as events, and mapped to a durable class.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn finish_completion(
        inner: &mut Inner,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
    ) -> Option<CompletionClass> {
        match m.class.clone() {
            TurnClass::FirstTurn | TurnClass::Healthy => None,
            TurnClass::TtlExpiry => Self::book_ttl_expiry(inner, pending, m, usage, turn_msgs),
            TurnClass::Recache { wasted_tokens } => Self::book_recache_completion(
                inner,
                pending,
                m,
                usage,
                head_moved_which,
                turn_msgs,
                wasted_tokens,
            ),
        }
    }

    /// Book a TTL-expiry turn: legitimate cache loss, counted but never a
    /// defect. Raised from `debug!` deliberately — at the proxy's `info`
    /// level this event could never appear, so its count read zero whether
    /// TTL expiries happened constantly or never.
    /// Extracted from `finish_completion` without behavior change.
    fn book_ttl_expiry(
        inner: &mut Inner,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        turn_msgs: Option<usize>,
    ) -> Option<CompletionClass> {
        inner.ttl_expiries_total += 1;
        record_cache_miss_attribution(MISS_ATTRIBUTION_PROVIDER, "ttl_expiry");
        // A TTL expiry is the *legitimate* cache loss: Anthropic's prefix
        // cache lives 5 minutes, so coming back to a session after a
        // break costs a full re-cache that is nobody's defect. Telling
        // that apart from a real bust is the difference between waste
        // the proxy caused and waste it merely witnessed.
        tracing::info!(
            event = "cache_recache_ttl_expiry",
            request_id = %usage.request_id,
            conversation_key = %pending.conversation_key,
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            // Which tracked stream the arithmetic was done against.
            // `-1` = matched nothing, so this was booked a first turn.
            matched_stream_msgs = m.matched_stream_msgs.map_or(-1_i64, |m| m as i64),
            turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
            streams_tracked = m.streams_tracked,
            cache_creation_input_tokens = usage.cache_creation_input_tokens,
            idle_seconds = m.idle_gap.as_secs(),
            "prefix re-written after cache TTL expiry (idle > 5 min); expected, not a defect"
        );
        Some(CompletionClass::TtlExpiry)
    }

    /// Attribute a recache turn, book its event, and map it to a durable
    /// completion class.
    /// Extracted from `finish_completion` without behavior change.
    fn book_recache_completion(
        inner: &mut Inner,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
        wasted_tokens: u64,
    ) -> Option<CompletionClass> {
        inner.recache_events_total += 1;
        let (event, event_kind, counts_as_waste) =
            Self::attribute_recache_event(inner, pending, m, usage, wasted_tokens);
        // Whether the *forwarded* prefix head moved, which is the
        // question `rebuild_boundary` is asked on the request side and
        // answers from the client's body instead. Derived from the
        // outbound dims rather than the inbound ones, and deliberately
        // ignoring `early_messages`: the prior-thinking drop and the
        // history offload rewrite messages, so on a turn where they
        // fired that dimension is partly our own doing and cannot be
        // used to judge whether the boundary that unlocked them was
        // real. `system` and `tools` are untouched by both, so they
        // still speak for the provider. `-1` where no comparison was
        // available.
        let forwarded_head_moved = event.outbound_drift_dims.as_deref().map_or(-1_i64, |dims| {
            i64::from(dims.split(',').any(|dim| matches!(dim, "system" | "tools")))
        });
        Self::emit_recache_event(
            &event,
            pending,
            m,
            usage,
            head_moved_which,
            turn_msgs,
            forwarded_head_moved,
            wasted_tokens,
        );
        crate::observability::observe_recache_event(
            event.attribution_reason.as_deref(),
            counts_as_waste.then_some(wasted_tokens),
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

    /// Emit the booking event for a recache kind: one log line per kind with
    /// the evidence that priced it.
    /// Extracted from `book_recache_completion` without behavior change.
    #[allow(clippy::too_many_arguments)]
    fn emit_recache_event(
        event: &RecacheEvent,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
        forwarded_head_moved: i64,
        wasted_tokens: u64,
    ) {
        match event.event_kind {
            RecacheEventKind::Drift => Self::emit_drift_event(
                event,
                pending,
                m,
                usage,
                head_moved_which,
                turn_msgs,
                forwarded_head_moved,
            ),
            RecacheEventKind::Branch => Self::emit_branch_event(
                event,
                pending,
                m,
                usage,
                head_moved_which,
                turn_msgs,
                forwarded_head_moved,
                wasted_tokens,
            ),
            RecacheEventKind::Unexplained => Self::emit_unexplained_event(
                event,
                pending,
                m,
                usage,
                head_moved_which,
                turn_msgs,
                forwarded_head_moved,
            ),
            RecacheEventKind::Expected => Self::emit_expected_event(
                event,
                pending,
                m,
                usage,
                head_moved_which,
                turn_msgs,
                forwarded_head_moved,
            ),
        }
    }

    /// Emit the drift-booking line: a structural bust where bytes inside the
    /// cached prefix moved. `event.wasted_tokens` is the charged share by
    /// construction.
    /// Extracted from `emit_recache_event` without behavior change.
    fn emit_drift_event(
        event: &RecacheEvent,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
        forwarded_head_moved: i64,
    ) {
        tracing::warn!(
            event = "cache_recache_observed",
            request_id = %usage.request_id,
            conversation_key = %event.conversation_key,
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            // Which tracked stream the arithmetic was done against.
            // `-1` = matched nothing, so this was booked a first turn.
            matched_stream_msgs = m.matched_stream_msgs.map_or(-1_i64, |m| m as i64),
            turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
            streams_tracked = m.streams_tracked,
            drift_dims = event.drift_dims.as_deref().unwrap_or(""),
            outbound_drift_dims =
                event.outbound_drift_dims.as_deref().unwrap_or("?"),
            forwarded_head_moved = forwarded_head_moved,
            // Which client-head component moved (model|system|tools),
            // same both-known rule as `head_changed`. Empty when the
            // fused head held still or was not comparable.
            head_moved = head_moved_which,
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
            wasted_tokens = event.wasted_tokens,
            prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
            prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
            prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
            prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
            // Same key witnesses as the unexplained arm: a drift
            // event can ride alongside a beta rotation or a commit
            // race, and the line should say so.
            forward_beta = event.forward_beta.as_deref().unwrap_or(""),
            forward_markers = event.forward_markers.as_deref().unwrap_or(""),
            beta_changed = event.beta_changed,
            markers_changed = event.markers_changed,
            // Forwarded (post-router) model: the router can rewrite
            // it after the compared fingerprint is taken, so this
            // is the only line that says what the provider keyed
            // on. Witness only, unranked.
            forward_model = event.forward_model.as_deref().unwrap_or(""),
            model_changed = event.model_changed,
            commit_race_suspect = event.commit_race_suspect,
            sibling_completed_recently = event.sibling_completed_recently,
            expected_cache_read = m.expected_cache_read,
            // Float, unlike the TTL line's truncated int: the 5s
            // commit window is queryable only with sub-second gap.
            idle_seconds = m.idle_gap.as_secs_f64(),
            actual_cache_read = usage.cache_read_input_tokens,
            cache_creation_input_tokens = usage.cache_creation_input_tokens,
            "prompt cache re-written inside the TTL window: billed tokens wasted re-caching"
        );
    }

    /// Emit the branch-creation line: the tail really did change, so the
    /// rebuild was earned — reported uncharged so the bucket can be audited
    /// instead of reading as a flat zero.
    /// Extracted from `emit_recache_event` without behavior change.
    #[allow(clippy::too_many_arguments)]
    fn emit_branch_event(
        event: &RecacheEvent,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
        forwarded_head_moved: i64,
        wasted_tokens: u64,
    ) {
        tracing::info!(
            event = "cache_recache_observed",
            request_id = %usage.request_id,
            conversation_key = %event.conversation_key,
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            // Which tracked stream the arithmetic was done against.
            // `-1` = matched nothing, so this was booked a first turn.
            matched_stream_msgs = m.matched_stream_msgs.map_or(-1_i64, |m| m as i64),
            turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
            streams_tracked = m.streams_tracked,
            drift_dims = event.drift_dims.as_deref().unwrap_or(""),
            outbound_drift_dims =
                event.outbound_drift_dims.as_deref().unwrap_or("?"),
            forwarded_head_moved = forwarded_head_moved,
            head_moved = head_moved_which,
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
            expected_cache_read = m.expected_cache_read,
            // Float, unlike the TTL line's truncated int: the 5s
            // commit window is queryable only with sub-second gap.
            idle_seconds = m.idle_gap.as_secs_f64(),
            actual_cache_read = usage.cache_read_input_tokens,
            cache_creation_input_tokens = usage.cache_creation_input_tokens,
            "prompt cache built for an inbound final-message replacement; branch creation, not waste"
        );
    }

    /// Emit the unexplained-booking line: attribution runs before these
    /// fields are read, so anything reaching here was recorded as causeless
    /// without ever being shown against the evidence. `event.wasted_tokens`
    /// is the charged share by construction.
    /// Extracted from `emit_recache_event` without behavior change.
    fn emit_unexplained_event(
        event: &RecacheEvent,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
        forwarded_head_moved: i64,
    ) {
        tracing::warn!(
            event = "cache_recache_observed",
            request_id = %usage.request_id,
            conversation_key = %event.conversation_key,
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            // Which tracked stream the arithmetic was done against.
            // `-1` = matched nothing, so this was booked a first turn.
            matched_stream_msgs = m.matched_stream_msgs.map_or(-1_i64, |m| m as i64),
            turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
            streams_tracked = m.streams_tracked,
            // The cause the evidence named (`unexplained_after_replay`
            // when none did; a replay-skip reason on the uncaused
            // path). The boundary position rides alongside in
            // `landing` — it is not the cause.
            attribution_reason = event.attribution_reason.as_deref().unwrap_or(""),
            landing = event.landing.as_deref().unwrap_or(""),
            origin = event.origin.as_deref().unwrap_or(""),
            scope = event.scope.as_deref().unwrap_or(""),
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
            outbound_drift_dims =
                event.outbound_drift_dims.as_deref().unwrap_or("?"),
            forwarded_head_moved = forwarded_head_moved,
            head_moved = head_moved_which,
            replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
            prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
            prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
            prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
            prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
            // Cache-key witnesses neither drift lane sees: a beta
            // rotation or marker-layout move busts the cache with
            // both lanes quiet. The `*_changed` flags compare
            // against the previous completed turn of the same
            // stream; unknown on either side reads as false.
            // `commit_race_suspect` says the previous turn of this
            // stream completed just before this one began, so the
            // write may not have committed yet.
            forward_beta = event.forward_beta.as_deref().unwrap_or(""),
            forward_markers = event.forward_markers.as_deref().unwrap_or(""),
            beta_changed = event.beta_changed,
            markers_changed = event.markers_changed,
            // Forwarded (post-router) model: the router can rewrite
            // it after the compared fingerprint is taken, so this
            // is the only line that says what the provider keyed
            // on. Witness only, unranked.
            forward_model = event.forward_model.as_deref().unwrap_or(""),
            model_changed = event.model_changed,
            commit_race_suspect = event.commit_race_suspect,
            sibling_completed_recently = event.sibling_completed_recently,
            replayed_prefix = event.replayed_prefix,
            replay_chain_id = event.replay_chain_id.unwrap_or(0),
            breakpoints_placed = event.breakpoints_placed.unwrap_or(0),
            system_markers_dropped = event.system_markers_dropped.unwrap_or(0),
            previous_forwarded_request_bytes =
                event.previous_forwarded_request_bytes.unwrap_or(0),
            forwarded_request_bytes = event.forwarded_request_bytes.unwrap_or(0),
            wasted_tokens = event.wasted_tokens,
            expected_cache_read = m.expected_cache_read,
            // Float, unlike the TTL line's truncated int: the 5s
            // commit window is queryable only with sub-second gap.
            idle_seconds = m.idle_gap.as_secs_f64(),
            actual_cache_read = usage.cache_read_input_tokens,
            // The three boundaries the landing was read against,
            // so the classification can be audited off the line.
            // `-1` = no turn before the previous one.
            previous_cache_read = m.previous_cache_read,
            previous_boundary = m.expected_cache_read,
            previous_previous_boundary = m
                .previous_previous_boundary
                .map_or(-1_i64, |b| b as i64),
            cache_creation_input_tokens = usage.cache_creation_input_tokens,
            "prompt cache re-written inside the TTL window without an attributed cause"
        );
    }

    /// Emit the expected-booking line: a re-cache with no direct causal
    /// evidence. `event.wasted_tokens` is the charged share by construction.
    /// Extracted from `emit_recache_event` without behavior change.
    fn emit_expected_event(
        event: &RecacheEvent,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
        turn_msgs: Option<usize>,
        forwarded_head_moved: i64,
    ) {
        tracing::info!(
            event = "cache_recache_observed",
            request_id = %usage.request_id,
            conversation_key = %event.conversation_key,
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            // Which tracked stream the arithmetic was done against.
            // `-1` = matched nothing, so this was booked a first turn.
            matched_stream_msgs = m.matched_stream_msgs.map_or(-1_i64, |m| m as i64),
            turn_msgs = turn_msgs.map_or(-1_i64, |m| m as i64),
            streams_tracked = m.streams_tracked,
            drift_dims = "",
            outbound_drift_dims =
                event.outbound_drift_dims.as_deref().unwrap_or("?"),
            forwarded_head_moved = forwarded_head_moved,
            head_moved = head_moved_which,
            replay_skipped = pending.replay_skip.map(|e| e.reason.as_str()).unwrap_or(""),
            attribution_reason = "",
            origin = "",
            scope = "",
            event_kind = "expected",
            wasted_tokens = event.wasted_tokens,
            prefix_head = pending.prefix.as_ref().map(|p| p.head.as_str()).unwrap_or(""),
            prefix_body = pending.prefix.as_ref().map(|p| p.body.as_str()).unwrap_or(""),
            prefix_stable = pending.prefix.as_ref().map(|p| p.stable.as_str()).unwrap_or(""),
            prefix_stable_msgs = pending.prefix.as_ref().map(|p| p.stable_msgs).unwrap_or(0),
            expected_cache_read = m.expected_cache_read,
            // Float, unlike the TTL line's truncated int: the 5s
            // commit window is queryable only with sub-second gap.
            idle_seconds = m.idle_gap.as_secs_f64(),
            actual_cache_read = usage.cache_read_input_tokens,
            cache_creation_input_tokens = usage.cache_creation_input_tokens,
            "prompt cache re-written inside the TTL window with no causal evidence: cause unattributed"
        );
    }

    /// Attribute a recache turn and build its bookable event: the cause, the
    /// kind, and the waste charged to it. The miss-attribution line fires
    /// here so the Python `total = ttl_expiry + prefix_change + unknown`
    /// invariant holds.
    /// Extracted from `book_recache_completion` without behavior change.
    fn attribute_recache_event(
        inner: &mut Inner,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        wasted_tokens: u64,
    ) -> (RecacheEvent, RecacheEventKind, bool) {
        let attribution = recache_attribution(
            pending.drift_dims.as_deref(),
            m.head_changed,
            m.beta_changed,
            pending.outbound_drift_dims.as_deref(),
            pending.replay_skip,
            pending.replay_applied,
            m.previous_turn_diverged,
            m.previous_turn_had_continuation,
            pending.concurrent_with_in_flight,
        );
        // Where the provider's read landed against the two previous
        // boundaries. Computed for every recache so the line is
        // auditable, but only *stored* on the residual path: the
        // reason keeps what the evidence named (`unexplained_after_replay`
        // when nothing did), and the landing rides alongside it.
        // Overwriting the reason with the landing is what used to send
        // readers hunting a provider bug for a boundary position the
        // evidence does not explain.
        let landing = CacheLanding::classify(
            usage.cache_read_input_tokens,
            m.previous_cache_read,
            m.expected_cache_read,
            m.previous_previous_boundary,
        );
        let unexplained = attribution.reason == Some("unexplained_after_replay");
        let landing = unexplained.then(|| landing.as_str().to_owned());
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
            at_unix: usage
                .now
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs(),
            conversation_key: pending.conversation_key.clone(),
            session_key_hash: pending.session_key_hash.clone(),
            drift_dims: pending.drift_dims.clone(),
            outbound_drift_dims: pending.outbound_drift_dims.clone(),
            attribution_reason: attribution.reason.map(str::to_owned),
            landing,
            origin: attribution.origin.map(str::to_owned),
            scope: attribution.scope.map(str::to_owned),
            replayed_prefix: pending.replay_applied.is_some(),
            replay_chain_id: pending.replay_applied.map(|e| e.chain_id),
            breakpoints_placed: pending.replay_applied.map(|e| e.breakpoints_placed),
            system_markers_dropped: pending.replay_applied.map(|e| e.system_markers_dropped),
            previous_forwarded_request_bytes: m.previous_forwarded_request_bytes,
            forwarded_request_bytes: pending.forwarded_request_bytes,
            event_kind,
            forward_beta: pending.forward_beta.clone(),
            forward_markers: pending.forward_markers.clone(),
            forward_model: pending.forward_model.clone(),
            beta_changed: m.beta_changed,
            markers_changed: m.markers_changed,
            model_changed: m.model_changed,
            commit_race_suspect: pending.commit_race_suspect,
            sibling_completed_recently: pending.sibling_completed_recently,
            wasted_tokens: charged_wasted_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            expected_cache_read: m.expected_cache_read,
            actual_cache_read: usage.cache_read_input_tokens,
        };
        let counts_as_waste = attribution.counts_as_waste;
        (event, event_kind, counts_as_waste)
    }

    /// This turn's fingerprint, digested for the per-stream comparison.
    /// `head` is the hex of eight digest bytes (see `hex16`), so it reads
    /// back as the `u64` the record holds. Same for the per-component heads
    /// and the forwarded witnesses: unknown on either side compares as "not
    /// known", never moved. Markers compare on normalized shape (count +
    /// kind + TTL), not raw indices — the tail breakpoint renumbers with
    /// every appended message (see `normalize_marker_layout`). The forwarded
    /// (post-router) model is what the provider keyed on, since the router
    /// can rewrite the model after the compared fingerprint is taken.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn fingerprint_turn(pending: &PendingRequest) -> TurnFingerprint {
        TurnFingerprint {
            msgs: pending.prefix.as_ref().map(|p| p.stable_msgs),
            head: pending
                .prefix
                .as_ref()
                .and_then(|p| u64::from_str_radix(&p.head, 16).ok()),
            head_model: pending
                .prefix
                .as_ref()
                .and_then(|p| u64::from_str_radix(&p.head_model, 16).ok()),
            head_system: pending
                .prefix
                .as_ref()
                .and_then(|p| u64::from_str_radix(&p.head_system, 16).ok()),
            head_tools: pending
                .prefix
                .as_ref()
                .and_then(|p| u64::from_str_radix(&p.head_tools, 16).ok()),
            beta: pending.forward_beta.as_deref().map(Self::witness_digest),
            markers: pending
                .forward_markers
                .as_deref()
                .map(Self::marker_layout_digest),
            forward_model: pending.forward_model.as_deref().map(Self::witness_digest),
        }
    }

    /// Match this turn against the conversation's tracked streams, file its
    /// record, and classify it — returning everything the recache, stock and
    /// first-turn sections below consume.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn match_and_classify_turn(
        inner: &mut Inner,
        pending: &PendingRequest,
        fingerprint: &TurnFingerprint,
        usage: &TurnUsage<'_>,
    ) -> StreamMatch {
        Self::ensure_conversation_entry(inner, pending, usage);
        let streams = inner
            .conversations
            .get_mut(&pending.conversation_key)
            .expect("just inserted");
        let matched = match_stream(streams, fingerprint.msgs);
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
                request_id = %usage.request_id,
                conversation_key = %pending.conversation_key,
                session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                turn_msgs = fingerprint.msgs.unwrap_or(0),
                longest_tracked = streams.iter().filter_map(|r| r.msgs).max().unwrap_or(0),
                streams_tracked = streams.len(),
                cache_creation_input_tokens = usage.cache_creation_input_tokens,
                cache_read_input_tokens = usage.cache_read_input_tokens,
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
                false,
                0,
                None,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            Some(i) => {
                let prev = streams[i];
                let moves = Self::compare_heads(&prev, fingerprint);
                (
                    classify_turn(
                        &prev,
                        usage.now,
                        usage.input_tokens,
                        usage.cache_read_input_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_ttl,
                    ),
                    prev.cache_read_input_tokens
                        .saturating_add(prev.cache_creation_input_tokens),
                    // How long this stream sat idle. On a TTL expiry it is
                    // the whole story: a five-minute-plus gap means the
                    // provider's cache died on its own.
                    usage.now.duration_since(prev.at).unwrap_or(Duration::ZERO),
                    prev.forwarded_request_bytes,
                    prev.diverged,
                    prev.had_continuation,
                    prev.cache_read_input_tokens,
                    prev.previous_boundary,
                    moves.head,
                    moves.head_model,
                    moves.head_system,
                    moves.head_tools,
                    moves.beta,
                    moves.markers,
                    moves.forward_model,
                )
            }
        };
        let record = TurnRecord {
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            at: usage.now,
            forwarded_request_bytes: pending.forwarded_request_bytes,
            msgs: fingerprint.msgs,
            // Read straight off the skip evidence rather than off the
            // attribution below, which is computed after this record is
            // stored. Same source either way: `recache_attribution` derives
            // `prefix_content_diverged` from this very field.
            diverged: pending
                .replay_skip
                .as_ref()
                .is_some_and(|e| e.reason.as_str() == "prefix_content_diverged"),
            // Whether this turn ran hidden CCR continuation rounds. The
            // billed_totals note lands before complete() on the response
            // path, so by the time this record is stored the flag is set.
            had_continuation: pending.billed_totals.is_some(),
            previous_boundary: matched.map(|i| {
                streams[i]
                    .cache_read_input_tokens
                    .saturating_add(streams[i].cache_creation_input_tokens)
            }),
            head: fingerprint.head,
            head_model: fingerprint.head_model,
            head_system: fingerprint.head_system,
            head_tools: fingerprint.head_tools,
            beta: fingerprint.beta,
            markers: fingerprint.markers,
            forward_model: fingerprint.forward_model,
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
        let (
            class,
            expected,
            gap,
            bytes,
            diverged,
            had_continuation,
            prev_read,
            prevprev_boundary,
            head_moved,
            head_model_moved,
            head_system_moved,
            head_tools_moved,
            beta_moved,
            markers_moved,
            model_moved,
        ) = outcome;
        StreamMatch {
            class,
            expected_cache_read: expected,
            idle_gap: gap,
            previous_forwarded_request_bytes: bytes,
            previous_turn_diverged: diverged,
            previous_turn_had_continuation: had_continuation,
            previous_cache_read: prev_read,
            previous_previous_boundary: prevprev_boundary,
            head_changed: head_moved,
            head_model_changed: head_model_moved,
            head_system_changed: head_system_moved,
            head_tools_changed: head_tools_moved,
            beta_changed: beta_moved,
            markers_changed: markers_moved,
            model_changed: model_moved,
            matched_stream_msgs,
            streams_tracked,
            matched_stream_idx,
            matched_stock_prior,
        }
    }

    /// Ensure this conversation has a tracked-stream entry, accounting an
    /// eviction when one had to make room. A conversation evicted before its
    /// next turn is booked as a first turn, so any cache write it just paid
    /// for goes uncounted.
    /// Extracted from `match_and_classify_turn` without behavior change.
    fn ensure_conversation_entry(
        inner: &mut Inner,
        pending: &PendingRequest,
        usage: &TurnUsage<'_>,
    ) {
        if inner.conversations.get(&pending.conversation_key).is_some() {
            return;
        }
        if let Some(evicted_footprint) = inner.forgotten.pop(&pending.conversation_key) {
            inner.forgotten_conversations_total += 1;
            tracing::warn!(
                event = "cache_conversation_forgotten",
                conversation_key = %pending.conversation_key,
                session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
                capacity = CONVERSATION_CAPACITY,
                forgotten_total = inner.forgotten_conversations_total,
                evicted_footprint_tokens = evicted_footprint,
                cache_creation_input_tokens = usage.cache_creation_input_tokens,
                cache_read_input_tokens = usage.cache_read_input_tokens,
                "conversation evicted before its next turn; booked as a first turn, \
                 so any cache write it just paid for goes uncounted"
            );
        }
        // `push` reports what fell off the end; `put` does not, and the
        // eviction is the whole signal. Park the evicted prefix
        // footprint with the key so the forgotten line above can size
        // the floor it reports.
        if let Some((evicted, evicted_streams)) = inner
            .conversations
            .push(pending.conversation_key.clone(), Vec::new())
        {
            if evicted != pending.conversation_key {
                let footprint = evicted_streams
                    .last()
                    .map(|r| {
                        r.cache_read_input_tokens
                            .saturating_add(r.cache_creation_input_tokens)
                    })
                    .unwrap_or(0);
                inner.forgotten.put(evicted, footprint);
            }
        }
    }

    /// Per-component head comparison between the tracked previous turn and
    /// this turn's fingerprint: both sides known and different. An unknown
    /// head on either side is not comparable, and reporting a change from it
    /// would blame the client for a missing measurement. Same rule for the
    /// forwarded beta/marker witnesses (unknown is not a move) and the
    /// forwarded model (unknown is not a flap).
    /// Extracted from `match_and_classify_turn` without behavior change.
    fn compare_heads(prev: &TurnRecord, fingerprint: &TurnFingerprint) -> HeadMoves {
        HeadMoves {
            head: matches!(
                (prev.head, fingerprint.head),
                (Some(p), Some(c)) if p != c
            ),
            head_model: matches!(
                (prev.head_model, fingerprint.head_model),
                (Some(p), Some(c)) if p != c
            ),
            head_system: matches!(
                (prev.head_system, fingerprint.head_system),
                (Some(p), Some(c)) if p != c
            ),
            head_tools: matches!(
                (prev.head_tools, fingerprint.head_tools),
                (Some(p), Some(c)) if p != c
            ),
            beta: matches!(
                (prev.beta, fingerprint.beta),
                (Some(p), Some(c)) if p != c
            ),
            markers: matches!(
                (prev.markers, fingerprint.markers),
                (Some(p), Some(c)) if p != c
            ),
            forward_model: matches!(
                (prev.forward_model, fingerprint.forward_model),
                (Some(p), Some(c)) if p != c
            ),
        }
    }

    /// Count a hot-zone move in the absorb ledger: a hot-zone change that
    /// still read its cache is one the stabilisation absorbed; one that
    /// re-cached is one it did not.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn count_hot_zone_turn(inner: &mut Inner, class: &TurnClass, expected_cache_read: u64) {
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

    /// Which head component moved, for the recache lines below. Same
    /// both-known-and-different rule as `head_changed` itself: unknown on
    /// either side is "not known", never a move. Empty when the fused head
    /// held still (or was not comparable). Additive logging only — never a
    /// re-gating input (see the absorbed-head note on `recache_attribution`).
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn head_moved_string(m: &StreamMatch) -> String {
        [
            ("model", m.head_model_changed),
            ("system", m.head_system_changed),
            ("tools", m.head_tools_changed),
        ]
        .iter()
        .filter_map(|(name, moved)| moved.then_some(*name))
        .collect::<Vec<_>>()
        .join("|")
    }

    /// Price this turn against a stock client: what the same turn would have
    /// cost a plain Claude Code client with no compression, offload or holds.
    /// It runs beside the real request rather than instead of it, so there is
    /// no A/B split and no session is ever served the worse arm.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn price_stock_arm(
        inner: &mut Inner,
        pending: &PendingRequest,
        m: &StreamMatch,
        usage: &TurnUsage<'_>,
        head_moved_which: &str,
    ) {
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
        // Anthropic-billed turns only. Routed/OpenAI turns bill from a
        // different cache universe (no creation counter, no TTL split,
        // different pricing and retention); the watchdog above still
        // scores them, but the comparison below is priced in Anthropic
        // input-equivalents and must not touch them.
        let ours_prompt =
            usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
        if ours_prompt > 0 && pending.stock_eligible {
            // Bytes to tokens by proportion, either direction. Both numbers
            // are the same kind of JSON measured at the same place, so the
            // ratio carries over even though neither side is a token count.
            // Injections can make the forwarded body larger than what the
            // client sent; scaling down there is the same assumption as
            // scaling up for compression, and what keeps the arm symmetric.
            // (The fresh tail below is still shared unscaled, so an
            // injection into the tail overstates stock slightly — the
            // injected fresh bytes are unseparated here.)
            let stock_prompt = match (
                pending.client_request_bytes,
                pending.forwarded_request_bytes,
            ) {
                (Some(sent), Some(fwd)) if fwd > 0 => {
                    ((ours_prompt as f64) * (sent as f64) / (fwd as f64)).round() as u64
                }
                // No wire sizes to scale by: the stock client would have
                // sent what we sent.
                _ => ours_prompt,
            };

            // This stream's own prior footprint: sibling streams sharing one
            // conversation key (a main loop and its subagent fork) must not
            // price against each other. A new lineage starts at 0, which reads
            // as a full rebuild below — the same assumption the classifier
            // makes when it books the turn `FirstTurn`, on the grounds that a
            // fork had no prefix to reuse.
            let prior = m.matched_stock_prior;
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
            let stock_kept = matches!(m.class, TurnClass::Healthy)
                && !m.head_changed
                && m.idle_gap <= stock_horizon;
            // The tail after the last breakpoint is billed as fresh input on
            // both arms. Which message the breakpoint lands on is the client's
            // shape, not ours, so handing the stock arm a cheaper tail than we
            // got would be inventing a difference the transforms did not make.
            let stock_cacheable = stock_prompt.saturating_sub(usage.input_tokens);
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
                if let Some(rec) = streams.get_mut(m.matched_stream_idx) {
                    rec.stock_footprint = stock_read + stock_write;
                }
            }

            // Priced in input-equivalents rather than dollars: Anthropic's
            // multipliers (read 0.1x, 5-minute write 1.25x, 1-hour write 2.0x)
            // are the same for every model, so the ratio of the two arms holds
            // whatever was routed where, and no price table has to be right
            // for the comparison to be.
            let (w5, w1h) = match usage.cache_write_ttl_split {
                Some((five, hour)) => (five, hour),
                None => (usage.cache_creation_input_tokens, 0),
            };
            let mut ours_effective = usage.input_tokens as f64
                + usage.cache_read_input_tokens as f64 * CACHE_READ_MULTIPLIER
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
                    billed_input.saturating_sub(usage.input_tokens) as f64
                        + billed_read.saturating_sub(usage.cache_read_input_tokens) as f64
                            * CACHE_READ_MULTIPLIER
                        + billed_write.saturating_sub(usage.cache_creation_input_tokens) as f64
                            * CACHE_WRITE_5M_MULTIPLIER
                }
                None => 0.0,
            };
            ours_effective += ccr_hidden_effective;
            // The stock client pays the tier its own markers bought: hour
            // writes at 2.0x, five-minute writes at 1.25x.
            let stock_effective = usage.input_tokens as f64
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
                request_id = %usage.request_id,
                conversation_key = %pending.conversation_key,
                turn_class = ?m.class,
                head_changed = m.head_changed,
                head_moved = head_moved_which,
                stock_kept,
                client_ttl = ?pending.client_ttl,
                ours_effective = ours_effective.round() as u64,
                stock_effective = stock_effective.round() as u64,
                ours_input = usage.input_tokens,
                ours_read = usage.cache_read_input_tokens,
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
            let predicted_ours_read = m.expected_cache_read.min(ours_prompt);
            inner.predicted_read_tokens += predicted_ours_read;
            inner.observed_read_tokens += usage.cache_read_input_tokens;
            inner.predicted_read_abs_error +=
                predicted_ours_read.abs_diff(usage.cache_read_input_tokens);
        } else if ours_prompt > 0 {
            // Ineligible turn on a tracked stream: carry the eligible
            // lineage's footprint forward so the next compared turn prices
            // against it, instead of a zero this turn never earned. The
            // watchdog record above already carries this turn's own billed
            // footprint for classification; this is only the stock arm's.
            if let Some(streams) = inner.conversations.peek_mut(&pending.conversation_key) {
                if let Some(rec) = streams.get_mut(m.matched_stream_idx) {
                    rec.stock_footprint = m.matched_stock_prior;
                }
            }
        }
    }

    /// Count tokens re-written over ground the conversation already held.
    /// Every completed turn that wrote anything, not just the healthy ones —
    /// gating on `Healthy` made this dead arithmetic. Recache turns are
    /// counted here *and* by the recache detector; the two measure different
    /// things (this one, tokens re-written; that one, prefix not read) and
    /// must not be added up.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn record_unearned_write(
        inner: &mut Inner,
        request_id: &str,
        pending: &PendingRequest,
        class: &TurnClass,
        expected_cache_read: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) {
        if cache_creation_input_tokens == 0 {
            return;
        }
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

    /// Attribute the first completed turn under a conversation key. The
    /// recache classifier has nothing to score it against, so without this
    /// its cache write — 41% of all write tokens, live — went unattributed.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn record_first_turn_write(
        inner: &mut Inner,
        pending: &PendingRequest,
        request_id: &str,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        streams_tracked: usize,
        now_instant: Instant,
    ) {
        if streams_tracked != 0 {
            return;
        }
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
            let reason = first_turn_reason(&ctx, pending.adoption.as_ref(), opener_seen_elsewhere);
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

    /// D0 diagnostic for first-turn-write-sharing.md: one line per turn
    /// classified FirstTurn, with none of the write branch's gates, so
    /// tracked first turns and tiny turns are measured too. Pure observation:
    /// no counter moves here.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn emit_first_turn_diagnostic(
        pending: &PendingRequest,
        request_id: &str,
        usage: &TurnUsage<'_>,
        class: &TurnClass,
    ) {
        if !matches!(class, TurnClass::FirstTurn) {
            return;
        }
        let dctx = pending.first_turn.clone().unwrap_or_default();
        let (rounds_in, rounds_read, rounds_write) =
            pending.billed_totals.map_or((0, 0, 0), |(bi, bcr, bcw)| {
                (
                    bi.saturating_sub(usage.input_tokens),
                    bcr.saturating_sub(usage.cache_read_input_tokens),
                    bcw.saturating_sub(usage.cache_creation_input_tokens),
                )
            });
        let (write_5m, write_1h) = usage
            .cache_write_ttl_split
            .map_or((-1_i64, -1_i64), |(m5, h1)| (m5 as i64, h1 as i64));
        tracing::info!(
            event = "first_turn_prefix_diagnostic",
            request_id = %request_id,
            conversation_key = %pending.conversation_key,
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            model = dctx.model.as_deref().unwrap_or(""),
            msgs = dctx.msgs,
            message_zero_hash = dctx.message_zero_hash.as_deref().unwrap_or(""),
            compaction_restart = dctx.compaction_restart,
            adopted = pending.adoption.is_some(),
            replay_applied = pending.replay_applied.is_some(),
            tool_choice = dctx.tool_choice.as_deref().unwrap_or("absent"),
            thinking = dctx.thinking.as_deref().unwrap_or("absent"),
            effort = dctx.effort.as_deref().unwrap_or("absent"),
            images_in_m0 = dctx.images_in_m0,
            opens_with_scaffolding = dctx.opens_with_scaffolding,
            m0_scaffold_bytes = dctx.m0_scaffold_bytes,
            m0_rest_bytes = dctx.m0_rest_bytes,
            outer_input_tokens = usage.input_tokens,
            outer_cache_read = usage.cache_read_input_tokens,
            outer_cache_write = usage.cache_creation_input_tokens,
            outer_write_5m = write_5m,
            outer_write_1h = write_1h,
            rounds_input_tokens = rounds_in,
            rounds_cache_read = rounds_read,
            rounds_cache_write = rounds_write,
            "first turn under its conversation key completed; cache-key controls and outer-vs-rounds split"
        );
    }

    /// Fleet-wide rolling hit rate (statusline ambient signal).
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn record_hit_rate(
        inner: &mut Inner,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_capable: bool,
    ) {
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
    }

    /// Pop this request's pending turn. `None` when the request never went
    /// through the compression gate (compression off, non-JSON, …) — no
    /// conversation identity, so no per-turn classification.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn pop_pending(inner: &mut Inner, request_id: &str) -> Option<PendingRequest> {
        inner.pending.pop(request_id)
    }

    /// Log a clean completion for the recency witnesses: a same-key turn
    /// beginning within `COMMIT_LATENCY_WINDOW` may be racing the provider's
    /// commit of this write, and a sibling key of the same session completing
    /// nearby is the re-key/fan-out join. Only clean completions reach here —
    /// abandoned entries never pop — so this is exactly the committed set.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn note_completion(inner: &mut Inner, pending: &PendingRequest, now_instant: Instant) {
        inner.recently_completed.push_back(CompletedTurn {
            conversation_key: pending.conversation_key.clone(),
            session_key_hash: pending.session_key_hash.clone(),
            completed_at: now_instant,
        });
        while inner.recently_completed.len() > RECENT_COMPLETION_CAPACITY {
            inner.recently_completed.pop_front();
        }
    }

    /// Price this turn's saving against the usage actually billed for it.
    ///
    /// A token removed from the request is worth what it *would have cost*,
    /// and on a cached workload that is not one number. Tokens inside the
    /// cached prefix bill at the cache-read rate; tokens past it bill at the
    /// cache-write or fresh-input rate, which is over 12x more. Reporting a
    /// saving without saying which it was overstates it by that factor —
    /// item 10, and the reason the headline figure read 10x high.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn record_savings_placement(
        request_id: &str,
        pending: &PendingRequest,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) {
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
    }

    /// Ground-truth cost ledger, built from figures the proxy cannot
    /// influence — the `usage` block Anthropic returns, which is the bill.
    /// `billed_fresh_equivalents` restates that bill in one comparable unit,
    /// weighting each class by its published price relative to fresh input.
    /// Extracted from `complete_with_cache_capability` without behavior change.
    fn record_cost_ledger(
        inner: &mut Inner,
        request_id: &str,
        pending: &PendingRequest,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_write_ttl_split: Option<(u64, u64)>,
    ) {
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
        // Hidden continuation rounds, split out so the ledger joins to
        // `ccr_continuation_usage` without recomputing the difference:
        // billed totals minus the client baseline `complete` was given.
        // Zero on the common single-round path. Existing fields stay as
        // they are.
        let (rounds_input_tokens, rounds_cache_read_tokens) =
            pending.billed_totals.map_or((0, 0), |(bi, bcr, _)| {
                (
                    bi.saturating_sub(input_tokens),
                    bcr.saturating_sub(cache_read_input_tokens),
                )
            });
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
            // Join key for the re-key floor: a continuation under a fresh
            // key shares the session hash, not the conversation key.
            session_key_hash = pending.session_key_hash.as_deref().unwrap_or(""),
            // Anthropic's own numbers, summed over every round the proxy
            // ran and otherwise unmodified.
            input_tokens = billed_input,
            cache_read_input_tokens = billed_cache_read,
            cache_creation_input_tokens = billed_cache_write,
            // Hidden-round split: billed totals minus the client baseline,
            // so the ledger joins to `ccr_continuation_usage` directly.
            rounds_input_tokens = rounds_input_tokens,
            rounds_cache_read_tokens = rounds_cache_read_tokens,
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

    /// Ten alternating user/assistant messages: past the fingerprint fixed
    /// depth (8), so `body` is comparable and `stable` covers the first 9.
    fn ten_turn_body() -> serde_json::Value {
        let messages: Vec<serde_json::Value> = (0..10)
            .map(|i| {
                serde_json::json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("message-{i}"),
                })
            })
            .collect();
        serde_json::json!({
            "model": "claude-opus-4-6",
            "system": "you are a test",
            "tools": [],
            "messages": messages,
        })
    }

    /// C8 frozen vector: the watchdog conversation key folds the session
    /// key and the first message only — a growing tail must not re-key.
    #[test]
    fn conversation_key_vector_is_frozen() {
        let key = conversation_key(&ten_turn_body(), "ip:10.0.0.3:-");
        assert_eq!(key, "8f2d781234961e29");
        assert_eq!(key.len(), 16, "8 bytes hex");
        // A new latest message leaves the key alone (only msg0 is folded).
        let mut grown = ten_turn_body();
        grown["messages"]
            .as_array_mut()
            .expect("messages")
            .push(serde_json::json!({"role": "user", "content": "message-10"}));
        assert_eq!(conversation_key(&grown, "ip:10.0.0.3:-"), key);
        // A rewritten opener re-keys.
        let mut rewritten = ten_turn_body();
        rewritten["messages"][0]["content"] = serde_json::json!("a different opener");
        assert_ne!(conversation_key(&rewritten, "ip:10.0.0.3:-"), key);
    }

    /// C8 frozen vector: the prefix fingerprint's head/body/stable split
    /// for a fixed body, with the documented comparability depths.
    #[test]
    fn prefix_fingerprint_vector_is_frozen() {
        let fp = prefix_fingerprint(&ten_turn_body());
        assert_eq!(fp.head, "0d053f35d9401183");
        assert_eq!(fp.body, "7c4b1d729536c4b7");
        assert_eq!(fp.stable, "d6e9603c54bc03e2");
        assert_eq!(fp.stable_msgs, 9, "every message except the live tail");
        // Below the fixed depth the body reports incomparable, never stale.
        let short = serde_json::json!({
            "model": "claude-opus-4-6",
            "system": "you are a test",
            "tools": [],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let short_fp = prefix_fingerprint(&short);
        assert!(short_fp.body.is_empty(), "not comparable yet");
        assert_eq!(short_fp.stable_msgs, 0, "single message is all tail");
        // A system change moves the head and leaves the body alone.
        let mut resys = ten_turn_body();
        resys["system"] = serde_json::json!("you are a different test");
        let resys_fp = prefix_fingerprint(&resys);
        assert_ne!(resys_fp.head, fp.head);
        assert_eq!(resys_fp.body, fp.body);
    }

    /// Eviction and never-seen used to be the same observation, so a turn
    /// that came back after its conversation fell out of the map was booked
    /// a first turn and its cache write went uncounted, silently.
    #[test]
    fn a_conversation_evicted_before_its_next_turn_is_counted_as_forgotten() {
        let obs = UsageObserver::new();
        let fp = |msgs| PrefixFingerprint {
            head: "h".into(),
            head_model: "m".into(),
            head_system: "s".into(),
            head_tools: "t".into(),
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
            false,
            None,
            None,
            Some(applied_evidence()),
            true,
            false,
            false,
        );
        assert_eq!(a.reason, Some("aftershock_of_diverged_prefix"));
        assert_eq!(a.origin, Some("previous_turn"));
        assert!(a.counts_as_waste, "the rewrite is still real waste");
    }

    #[test]
    fn a_turn_after_a_continuation_names_the_previous_turn() {
        let a = recache_attribution(
            None,
            false,
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
            true,
            false,
        );
        assert_eq!(a.reason, Some("aftershock_of_continuation"));
        assert_eq!(a.origin, Some("previous_turn"));
        assert!(a.counts_as_waste, "the rewrite is still real waste");
    }

    /// The residual marker reaches the event as the reason, with the boundary
    /// position riding alongside in `landing` — never swapped for it.
    #[test]
    fn without_a_previous_divergence_the_residual_is_left_for_the_landing() {
        let a = recache_attribution(
            None,
            false,
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
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
            head_model: "m".into(),
            head_system: "s".into(),
            head_tools: "t".into(),
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
            Some("unexplained_after_replay"),
            "the residual keeps its own reason; the landing rides alongside"
        );
        assert_eq!(
            event.landing.as_deref(),
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
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
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
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
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
            false,
            None,
            None,
            Some(applied_evidence()),
            false,
            false,
            true,
        );
        assert_eq!(a.reason, Some("prefix_head_changed"));
        assert_eq!(a.origin, Some("client"));
        assert_eq!(a.scope, Some("hot_zone"));
        assert!(a.counts_as_waste, "the prefix was genuinely re-written");
    }

    /// A rotated `anthropic-beta` header voids the provider's prefix with
    /// both drift lanes quiet. Measured 2026-09-17: 3 recache turns whose
    /// forwarded model/system/tools held still while beta flipped exactly
    /// on the bust turn. Client origin — the client sent the header — so it
    /// outranks proxy causes and timing suspects, but yields to the
    /// structural client evidence above.
    #[test]
    fn a_rotated_beta_header_is_a_named_client_cause() {
        let a = recache_attribution(None, false, true, None, None, None, false, false, false);
        assert_eq!(a.reason, Some("forwarded_beta_rotated"));
        assert_eq!(a.origin, Some("client"));
        assert_eq!(a.scope, Some("cache_key"));
        assert!(a.counts_as_waste, "the rotation re-billed the prefix");
    }

    /// Ordering: drift, head, and a declined replay all outrank beta, so the
    /// three simultaneously-true turns from 2026-09-17 keep their existing
    /// labels. Beta outranks the proxy outbound lane and the commit race,
    /// which is where otherwise-clean beta busts used to land.
    #[test]
    fn beta_yields_to_structural_client_evidence_but_beats_proxy_and_timing() {
        // Head wins.
        let a = recache_attribution(None, true, true, None, None, None, false, false, false);
        assert_eq!(a.reason, Some("prefix_head_changed"));
        // Inbound drift wins.
        let a = recache_attribution(
            Some("tools"),
            false,
            true,
            None,
            None,
            None,
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("tools"));
        // Proxy outbound loses to client beta.
        let a = recache_attribution(
            None,
            false,
            true,
            Some("tools"),
            None,
            None,
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("forwarded_beta_rotated"));
        // Commit race loses to measured evidence.
        let a = recache_attribution(None, false, true, None, None, None, false, false, true);
        assert_eq!(a.reason, Some("forwarded_beta_rotated"));
    }

    /// The declined replay outranks beta too: on the three measured turns all
    /// of head, divergence, and beta were true, and the label stays head —
    /// with divergence second, beta third.
    #[test]
    fn a_declined_replay_outranks_a_rotated_beta() {
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
            true,
            None,
            Some(skip),
            None,
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("prefix_content_diverged"));
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
            false,
            Some("early_messages"),
            Some(skip),
            None,
            false,
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
            false,
            Some("tools,messages[0]"),
            None,
            Some(applied_evidence()),
            false,
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
            false,
            Some("system,tools"),
            None,
            Some(applied_evidence()),
            false,
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
    fn an_absorbed_client_edit_is_not_the_cause() {
        // 2026-09-15: a client dropped one tool from `tools[]`, the roster pin
        // put it back, and the forwarded roster went out byte-identical — the
        // outbound lane moved on `early_messages` and never on `tools`. The
        // event still read `tools`, sending the reader after a client the pin
        // had already handled instead of after the proxy stage that rewrote
        // the history.
        let a = recache_attribution(
            Some("tools"),
            false,
            false,
            Some("early_messages"),
            None,
            Some(applied_evidence()),
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("early_messages"));
        assert_eq!(a.origin, Some("proxy"));
        assert_eq!(a.scope, Some("forwarded_hot_zone"));
        assert!(a.counts_as_waste);
    }

    #[test]
    fn an_unobserved_outbound_lane_keeps_the_inbound_reading() {
        // `None` is not absorption: the lane reads `None` both when nothing
        // drifted and when the forwarding path never ran. Discarding the
        // inbound dims on that would throw away the one cause the turn has.
        let a = recache_attribution(
            Some("tools"),
            false,
            false,
            None,
            None,
            None,
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("tools"));
        assert_eq!(a.origin, Some("client"));
        assert_eq!(a.scope, Some("hot_zone"));
    }

    #[test]
    fn a_forwarded_body_that_held_still_absorbs_everything() {
        // `Some("")` is the strongest absorption evidence there is: the lane
        // was compared and not one of the three dimensions moved on the wire,
        // so nothing the client did reached the provider. Reading that as "no
        // information" — which it was until the call site started
        // distinguishing it from a birth turn — threw away the only proof a
        // stabilizer had done its job.
        let a = recache_attribution(
            Some("tools"),
            true,
            false,
            Some(""),
            None,
            Some(applied_evidence()),
            false,
            false,
            false,
        );
        assert_ne!(a.reason, Some("tools"));
        assert_ne!(
            a.reason,
            Some("prefix_head_changed"),
            "the head check reads the same absorbed edit one turn wider"
        );
    }

    #[test]
    fn a_partly_absorbed_client_edit_still_names_the_client() {
        // One shared dimension is enough. `tools` was absorbed and `system`
        // was not, and the client moving `system` first is the cause however
        // the rest of its edit fared.
        let a = recache_attribution(
            Some("system,tools"),
            false,
            false,
            Some("system"),
            None,
            Some(applied_evidence()),
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("system,tools"));
        assert_eq!(a.origin, Some("client"));
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
            false,
            None,
            Some(skip),
            Some(applied_evidence()),
            true,
            false,
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
            had_continuation: false,
            previous_boundary: None,
            head: None,
            head_model: None,
            head_system: None,
            head_tools: None,
            beta: None,
            markers: None,
            forward_model: None,
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
            head_model: "m".into(),
            head_system: "s".into(),
            head_tools: "t".into(),
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

    /// /debug/active-conversations: parked turns show with their project,
    /// completed turns leave, and noting a project for an unknown id is silent.
    #[test]
    fn active_conversations_reports_in_flight_with_project() {
        let obs = UsageObserver::new();
        obs.begin_request("r1", "conv-a".into(), None, None, None);
        obs.note_project("r1", "/repo/a".into());
        obs.begin_request("r2", "conv-b".into(), None, None, None);

        let active = obs.active_conversations();
        assert_eq!(active.len(), 2);
        let a = active
            .iter()
            .find(|c| c.conversation == "conv-a")
            .expect("conv-a present");
        assert_eq!(a.project.as_deref(), Some("/repo/a"));
        let b = active
            .iter()
            .find(|c| c.conversation == "conv-b")
            .expect("conv-b present");
        assert_eq!(b.project, None, "un-noted turns report without a project");

        // Unknown ids never panic and never create entries.
        obs.note_project("nope", "/repo/x".into());
        assert_eq!(obs.active_conversations().len(), 2);

        obs.complete("r1", 10, 0, 0, None);
        let active = obs.active_conversations();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].conversation, "conv-b");
    }

    /// Past the horizon a parked turn is no longer in flight, so the endpoint
    /// stops reporting it even before the next arrival sweeps it.
    #[test]
    fn active_conversations_hides_past_horizon_turns() {
        let obs = UsageObserver::new();
        obs.begin_request("old", "conv-old".into(), None, None, None);
        obs.age_pending("old", IN_FLIGHT_HORIZON);
        assert!(obs.active_conversations().is_empty());
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
                ..Default::default()
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
                ..Default::default()
            },
        );
        obs.complete("f1", 100, 0, 50_000, None);
        let snap = obs.snapshot();
        assert_eq!(snap.first_turn_writes_total, 1);
        assert_eq!(snap.first_turn_contradictions_total, 0);
    }

    #[test]
    fn first_turn_context_captures_d0_cache_key_controls() {
        let parsed = serde_json::json!({
            "model": "claude-opus-5",
            "tool_choice": {"type": "tool", "name": "Bash"},
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "effort": "high",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "<system-reminder>shared digest"},
                    {"type": "text", "text": "do the task"},
                    {"type": "image", "source": {"type": "base64", "data": "x"}},
                ],
            }],
        });
        let ctx = first_turn_context(&parsed);
        assert_eq!(ctx.tool_choice.as_deref(), Some("tool:Bash"));
        assert_eq!(ctx.thinking.as_deref(), Some("enabled:10000"));
        assert_eq!(ctx.effort.as_deref(), Some("high"));
        assert!(ctx.images_in_m0);
        assert!(ctx.opens_with_scaffolding);
        assert_eq!(
            ctx.m0_scaffold_bytes,
            "<system-reminder>shared digest".len() as u64
        );
        assert_eq!(
            ctx.m0_rest_bytes,
            "do the task".len() as u64,
            "non-text blocks carry no text bytes"
        );

        // Absent controls stay absent rather than degrading to guesses.
        let bare = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let ctx = first_turn_context(&bare);
        assert_eq!(ctx.tool_choice, None);
        assert_eq!(ctx.thinking, None);
        assert_eq!(ctx.effort, None);
        assert!(!ctx.images_in_m0);
        assert!(!ctx.opens_with_scaffolding);
        assert_eq!((ctx.m0_scaffold_bytes, ctx.m0_rest_bytes), (0, 5));
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
                    had_continuation: false,
                    previous_boundary: None,
                    head: None,
                    head_model: None,
                    head_system: None,
                    head_tools: None,
                    beta: None,
                    markers: None,
                    forward_model: None,
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
                    had_continuation: false,
                    previous_boundary: None,
                    head: None,
                    head_model: None,
                    head_system: None,
                    head_tools: None,
                    beta: None,
                    markers: None,
                    forward_model: None,
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
                    had_continuation: false,
                    previous_boundary: None,
                    head: None,
                    head_model: None,
                    head_system: None,
                    head_tools: None,
                    beta: None,
                    markers: None,
                    forward_model: None,
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
