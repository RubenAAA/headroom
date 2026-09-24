//! usage_observer::events — split from usage_observer.rs (pure move, no logic change).
use super::*;

/// Provider label for the cache-miss attribution metric. This observer only
/// ever sees Anthropic usage counters (see the module docs), so the label is
/// constant rather than threaded through every call site.
pub(super) const MISS_ATTRIBUTION_PROVIDER: &str = "anthropic";

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
    /// The same axes measured on the body actually forwarded. Published beside
    /// `drift_dims` because only this one is a cache-key input, and the two
    /// disagreeing is the signature of a stabilizer holding a client edit back:
    /// `Some("")` says the forwarded body held still in every dimension.
    /// `None` says no comparison was available, which is not the same thing.
    pub outbound_drift_dims: Option<String>,
    /// Stable, explicit cause derived only from direct evidence. This is a
    /// structural drift dimension or a causal prefix-replay skip reason;
    /// `None` means the event is genuinely unattributed.
    ///
    /// Deliberately never a [`CacheLanding`] boundary name: the residual
    /// `unexplained_after_replay` keeps its own reason and the landing rides
    /// alongside in [`RecacheEvent::landing`]. Overwriting the reason with the
    /// landing is what sent readers hunting a provider bug for a boundary
    /// position the evidence does not explain.
    pub attribution_reason: Option<String>,
    /// Where the provider's cache read landed against the two previous
    /// boundaries — a [`CacheLanding`] name such as
    /// `provider_missed_newest_write`. `Some` only on residual
    /// `unexplained_after_replay` events, where it is the boundary position,
    /// not a proved provider-internal cause. `None` everywhere else, including
    /// on `uncaused_waste` events whose reason names the replay skip instead.
    pub landing: Option<String>,
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
    /// Forwarded beta-header digest and marker layout of this turn (see
    /// `PendingRequest::forward_beta`), so an unexplained event can be checked
    /// against a key rotation neither drift lane sees. Observational only.
    pub forward_beta: Option<String>,
    pub forward_markers: Option<String>,
    /// Forwarded (post-router) model string of this turn, with the
    /// turn-apart comparison below. The router can rewrite the model after
    /// the compared fingerprint is taken; only this value says what the
    /// provider keyed on. Observational only.
    pub forward_model: Option<String>,
    /// Whether the beta header / marker layout moved against the previous
    /// completed turn of the same stream. Both sides known and different;
    /// unknown on either side reads as "not known", never as moved.
    pub beta_changed: bool,
    pub markers_changed: bool,
    /// Whether the forwarded (post-router) model string moved against the
    /// previous completed turn of the same stream. Same both-known rule.
    /// Witness only — `recache_attribution` does not consult it until a
    /// first real flap is measured.
    pub model_changed: bool,
    /// Timing witnesses from `PendingRequest`: a same-key turn completed just
    /// before this one began (commit race suspect), or a sibling key of the
    /// same session did (fan-out / re-key context). Neither attributes.
    pub commit_race_suspect: bool,
    pub sibling_completed_recently: bool,
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
    /// Anthropic-billed turns only: routed/OpenAI turns stay out (different
    /// cache universe, different pricing), while the watchdog still scores
    /// them.
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
pub(super) struct CostSample {
    pub(super) cache_read_tokens: u64,
    pub(super) cache_write_tokens: u64,
    pub(super) forwarded_bytes: u64,
    pub(super) billed_fresh_equivalents: f64,
}

/// One turn's contribution to the fleet-wide hit-rate window.
pub(super) struct RecentHitRateSample {
    pub(super) rate: f64,
    /// False when the provider turn reported no cache-usage data at all (no
    /// cache fields in the usage block): "no signal", not a cache miss.
    pub(super) cache_capable: bool,
}

/// One cleanly completed turn, for the recency witnesses in `begin_request`.
///
/// Carries the session hash alongside the conversation key so a re-keyed
/// continuation — same session, fresh key after a compaction, model switch,
/// or system rewrite — can be joined offline to the sibling completion that
/// preceded it, instead of reading as an unrelated cold start.
pub(super) struct CompletedTurn {
    pub(super) conversation_key: String,
    pub(super) session_key_hash: Option<String>,
    pub(super) completed_at: Instant,
}

/// One conversation with a turn currently in flight, for the loopback
/// `/debug/active-conversations` endpoint.
///
/// `conversation` is the same opaque 16-hex usage key the observer tracks
/// everywhere else — it joins to nothing outside this process.
/// `project` is the canonical project directory the turn resolved to
/// (or the shared unresolved bucket); full local paths stay behind the
/// loopback guard with the rest of the `/debug/*` surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveConversation {
    pub conversation: String,
    pub project: Option<String>,
    pub age_secs: u64,
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
        let _guard = super::super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp = PrefixFingerprint {
            head: "aaaaaaaaaaaaaaaa".into(),
            head_model: "mmmmmmmmmmmmmmmm".into(),
            head_system: "ssssssssssssssss".into(),
            head_tools: "tttttttttttttttt".into(),
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
        let expected = super::super::super::drift_detector::session_key_log_prefix("sess-x");
        assert!(
            line.contains(&format!("session_key_hash={expected}")),
            "session key missing: {line}"
        );
    }

    /// `head_moved` names the component behind a `prefix_head_changed` turn.
    /// Two fingerprints identical except the system component: the recache
    /// must report `head_moved=system` and nothing else.
    #[test]
    fn a_recache_event_names_which_head_component_moved() {
        let _guard = super::super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp_a = PrefixFingerprint {
            head: "aaaaaaaaaaaaaaaa".into(),
            head_model: "1111111111111111".into(),
            head_system: "2222222222222222".into(),
            head_tools: "3333333333333333".into(),
            body: "bbbbbbbbbbbbbbbb".into(),
            stable: "cccccccccccccccc".into(),
            stable_msgs: 42,
        };
        let fp_b = PrefixFingerprint {
            head_system: "4444444444444444".into(),
            ..fp_a.clone()
        };

        tracing::subscriber::with_default(sub, || {
            let obs = UsageObserver::new();
            obs.begin_request("r1", "conv-hm".into(), None, None, Some(fp_a));
            obs.complete("r1", 300, 0, 10_000, None);
            obs.begin_request("r2", "conv-hm".into(), None, None, Some(fp_b));
            obs.complete("r2", 200, 0, 11_000, None);
        });

        let joined = cap.lock().unwrap().fields.join("\n");
        let line = joined
            .lines()
            .find(|l| l.contains("cache_recache_observed"))
            .unwrap_or_else(|| panic!("no recache event emitted; captured:\n{joined}"));
        assert!(
            line.contains("head_moved=system"),
            "which-moved missing: {line}"
        );
    }

    /// The saving and the usage it should be priced against are produced on
    /// opposite sides of the request. Answering "is this worth running" meant
    /// correlating two log events after the fact, which is why the question
    /// stayed open. This asserts the one line that already contains the answer.
    #[test]
    fn a_compressed_turn_prices_its_saving_against_the_billed_usage() {
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        // Hidden-round split: billed totals minus the client baseline.
        assert!(line.contains("rounds_input_tokens=10"), "{line}");
        assert!(line.contains("rounds_cache_read_tokens=150000"), "{line}");
        // 20 + 350000*0.1 + 4000*1.25 = 40020
        assert!(line.contains("billed_fresh_equivalents=40020"), "{line}");
    }

    /// The ledger exists because every other savings figure here is produced
    /// by the component doing the saving. This one must be built only from the
    /// provider's own usage numbers, or it is worth no more than the rest.
    #[test]
    fn the_cost_ledger_uses_only_the_providers_numbers() {
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp = PrefixFingerprint {
            head: "hhhhhhhhhhhhhhhh".into(),
            head_model: "mmmmmmmmmmmmmmmm".into(),
            head_system: "ssssssssssssssss".into(),
            head_tools: "tttttttttttttttt".into(),
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
            .find(|l| l.contains("attribution_reason=unexplained_after_replay"))
            .unwrap_or_else(|| panic!("no unexplained event; captured:\n{joined}"));
        assert!(
            line.contains("landing=provider_missed_newest_write"),
            "{line}"
        );

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
        let _guard = super::super::tests::miss_metric_test_lock();
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
            .find(|l| l.contains("attribution_reason=unexplained_after_replay"))
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
        let cap = Arc::new(StdMutex::new(Captured::default()));
        let sub = tracing_subscriber::registry().with(CaptureFields(cap.clone()));

        let fp = |msgs: usize| PrefixFingerprint {
            head: "hhhhhhhhhhhhhhhh".into(),
            head_model: "mmmmmmmmmmmmmmmm".into(),
            head_system: "ssssssssssssssss".into(),
            head_tools: "tttttttttttttttt".into(),
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
            // Mirror the head so a hot-zone change moves the components with
            // it; short hex strings parse the same way `complete` parses them.
            head_model: head.into(),
            head_system: head.into(),
            head_tools: head.into(),
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
            // Mirror the head so a hot-zone change moves the components with
            // it; short hex strings parse the same way `complete` parses them.
            head_model: head.into(),
            head_system: head.into(),
            head_tools: head.into(),
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
                head_model: "m".into(),
                head_system: "s".into(),
                head_tools: "t".into(),
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

    /// Bytes we added scale the stock prompt down, symmetrically with how
    /// removed bytes scale it up. Injections (recall, proactive expansion)
    /// make the forwarded body larger than what the client sent; pricing
    /// stock at our inflated size would report no difference where we
    /// added real cost.
    #[test]
    fn bytes_we_added_scale_the_stock_prompt_down() {
        let obs = UsageObserver::new();

        // Client sent 1 KB, we forwarded 2 KB: the stock prompt is half ours.
        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 2_000, "on");
        obs.complete("r1", 100, 0, 10_000, None);

        let s = obs.snapshot();
        // Ours: 100 fresh + 10_000 written at 1.25x.
        assert_eq!(s.ours_effective_tokens, 12_600);
        // Stock: 5_050 prompt, 100 fresh tail kept, 4_950 rebuilt at 1.25x.
        assert_eq!(s.stock_effective_tokens, 6_288);
        assert!(
            s.vs_stock_saving_pct < 0.0,
            "doubling the wire size with no cache benefit is a loss, not par: {}",
            s.vs_stock_saving_pct
        );
    }

    /// Translated (non-Anthropic-billed) turns stay out of the comparison.
    /// They report no creation counter and no TTL split, and their provider
    /// charges no Anthropic write premium — pricing them at 1.25x/2.0x
    /// invents cost the bill never had. The watchdog still books them.
    #[test]
    fn ineligible_turns_skip_the_stock_arm_but_keep_the_watchdog() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_stock_ineligible("r1");
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 100, 0, 10_000, None);

        let s = obs.snapshot();
        assert_eq!(s.stock_turns_compared, 0);
        assert_eq!(s.vs_stock_saving_pct, 0.0);
        assert_eq!(s.vs_stock_saving_pct_recent, None);
        assert_eq!(s.vs_stock_turns_recent, 0);
        // The watchdog still saw it: one capable sample in the hit-rate
        // window, even though the stock arm booked nothing.
        assert_eq!(s.samples, 1);
    }

    /// An ineligible turn between two compared turns of one stream must not
    /// reset the eligible lineage: the next compared turn prices against
    /// the footprint the last compared turn filed, not a zero.
    #[test]
    fn an_ineligible_turn_does_not_reset_the_eligible_lineage() {
        let obs = UsageObserver::new();

        obs.begin_request("r1", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r1", 1_000, 1_000, "off");
        obs.complete("r1", 100, 0, 10_000, None);

        obs.begin_request("r2", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_stock_ineligible("r2");
        obs.note_wire_bytes("r2", 1_000, 1_000, "off");
        // No growth on the ineligible turn, so the footprints on either
        // side agree and the carry-forward is exactly observable.
        obs.complete("r2", 100, 10_000, 0, None);

        obs.begin_request("r3", "conv".into(), None, None, Some(hot("aaaa")));
        obs.note_wire_bytes("r3", 1_000, 1_000, "off");
        obs.complete("r3", 100, 10_000, 200, None);

        let s = obs.snapshot();
        assert_eq!(s.stock_turns_compared, 2);
        assert_eq!(s.vs_stock_turns_recent, 2);
        // r3 reads the 10_000 footprint r1 filed, carried through r2: both
        // arms price 100 + 1_000 + 250, so the comparison is par.
        assert_eq!(s.ours_effective_tokens, s.stock_effective_tokens);
        assert_eq!(s.vs_stock_saving_pct, 0.0);
    }
}
