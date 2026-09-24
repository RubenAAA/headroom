//! usage_observer::turn — split from usage_observer.rs (pure move, no logic change).
use super::*;

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
    /// The previous turn ran hidden CCR continuation rounds: its provider
    /// write committed a prefix with proxy-private retrieval messages
    /// appended, so the next replayed client-baseline prefix cannot match.
    /// Carried forward one turn so the aftershock can be named instead of
    /// landing in the residual bucket.
    pub had_continuation: bool,
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
    /// Per-component heads (`PrefixFingerprint::head_model/system/tools`),
    /// same encoding and same both-known-and-different rule as `head`. Carried
    /// so a `prefix_head_changed` turn can log *which* component moved without
    /// re-reading the body, which is long gone by the response side.
    pub head_model: Option<u64>,
    pub head_system: Option<u64>,
    pub head_tools: Option<u64>,
    /// `DefaultHasher` digests of this turn's forwarded `forward_beta` /
    /// `forward_markers` witness strings (see `PendingRequest`). In-process
    /// only, like the stream matching they serve: the next turn of the same
    /// stream compares them to say whether the beta header or the marker
    /// layout moved, which neither drift lane can see. `None` propagates
    /// "not known", never "unchanged".
    pub beta: Option<u64>,
    pub markers: Option<u64>,
    /// `DefaultHasher` digest of this turn's forwarded `forward_model` (the
    /// post-router model string). Compared turn-apart under the same
    /// both-known rule; unranked witness until a first real flap exists.
    pub forward_model: Option<u64>,
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
pub(super) const MAX_STREAMS_PER_CONVERSATION: usize = 8;

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
pub(super) fn match_stream(streams: &[TurnRecord], msgs: Option<usize>) -> Option<usize> {
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
            head_model: "m".into(),
            head_system: "s".into(),
            head_tools: "t".into(),
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
            had_continuation: false,
            previous_boundary: None,
            head: None,
            head_model: None,
            head_system: None,
            head_tools: None,
            beta: None,
            markers: None,
            forward_model: None,
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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

    /// A final-message replacement that also moved the hot zone is a bust,
    /// not a branch: the tail check sees message counts only, so without this
    /// gate a head change files as a zero-waste branch and real money goes
    /// uncounted.
    #[test]
    fn a_tail_replacement_that_moved_the_head_is_a_bust_not_a_branch() {
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
        let skip = ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 2,
                replayed_prefix_msgs: 2,
            },
            Some(&prior),
            &current,
        );
        assert!(
            skip.is_inbound_tail_replacement(),
            "test setup must be a tail replacement"
        );
        let a = recache_attribution(
            None,
            true,
            false,
            None,
            Some(skip),
            None,
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("prefix_head_changed"));
        assert_eq!(a.origin, Some("client"));
        assert!(a.counts_as_waste, "the head move re-wrote the prefix");
    }

    /// Same gate for inbound drift: a tail replacement plus a tools change is
    /// the tools change's bust.
    #[test]
    fn a_tail_replacement_with_drift_dims_is_a_bust_not_a_branch() {
        let prior = [
            serde_json::json!({"role":"user","content":"open"}),
            serde_json::json!({"role":"assistant","content":"old"}),
        ];
        let current = [
            serde_json::json!({"role":"user","content":"open"}),
            serde_json::json!({"role":"assistant","content":"new"}),
        ];
        let skip = ReplaySkipEvidence::from_inbound_original_histories(
            ReplaySkip::PrefixContentDiverged {
                first_diff_index: 1,
                replayed_prefix_msgs: 1,
            },
            Some(&prior),
            &current,
        );
        assert!(
            skip.is_inbound_tail_replacement(),
            "test setup must be a tail replacement"
        );
        let a = recache_attribution(
            Some("tools"),
            false,
            false,
            None,
            Some(skip),
            None,
            false,
            false,
            false,
        );
        assert_eq!(a.reason, Some("tools"));
        assert!(a.counts_as_waste);
    }

    /// A bust whose divergence sits below the drift detector's window used to
    /// be filed as `Expected` — "no cause found" — and written off as a session
    /// reset. A declined prefix replay names that cause. Measured: 98% of the
    /// tokens in the supposedly-benign bucket were turns like this one.
    #[test]
    fn a_declined_replay_makes_an_unattributed_bust_a_named_one() {
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
        // Read 46_985 both turns: the write p1 made was never found. The
        // reason stays the residual; the boundary rides alongside in landing.
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("unexplained_after_replay")
        );
        assert_eq!(
            event.landing.as_deref(),
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

    /// A turn that begins right after its conversation's previous turn
    /// completed is a commit-race suspect: the provider may not have committed
    /// the write yet even though nothing is in flight. Witness only — the
    /// residual keeps its own reason, and the suspect flag rides alongside so
    /// an offline query can separate races from evictions.
    #[test]
    fn a_turn_right_after_a_completion_is_flagged_a_commit_race_suspect() {
        let _guard = super::super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("k1", "conv-race".into(), None, None, Some(fp(20)));
        obs.complete("k1", 10_000, 46_985, 55_557, None);
        // Immediately: inside COMMIT_LATENCY_WINDOW by construction.
        obs.begin_request("k2", "conv-race".into(), None, None, Some(fp(22)));
        obs.note_replay_applied("k2", ReplayAppliedEvidence::new(2, 2, 0));
        obs.complete("k2", 9_714, 46_985, 48_669, None);
        let event = obs.snapshot().last_event.expect("recache recorded");
        assert_eq!(event.event_kind, RecacheEventKind::Unexplained);
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("unexplained_after_replay"),
            "a suspect flag must not attribute"
        );
        assert!(event.commit_race_suspect);
        assert!(
            !event.sibling_completed_recently,
            "no sibling key completed on this session"
        );
    }

    /// A sibling key of the same session completing nearby is fan-out or
    /// re-key context on the event — again as a witness, never a cause.
    #[test]
    fn a_sibling_completion_is_recorded_as_context_not_cause() {
        let _guard = super::super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("a1", "conv-sib-a".into(), Some("sess"), None, Some(fp(20)));
        obs.complete("a1", 10_000, 46_985, 55_557, None);
        // Same session, different key: a fork or a re-keyed continuation.
        obs.begin_request("b1", "conv-sib-b".into(), Some("sess"), None, Some(fp(4)));
        obs.complete("b1", 500, 0, 5_000, None);
        // Back on the first key with drift: a Drift event carrying context.
        obs.begin_request(
            "a2",
            "conv-sib-a".into(),
            Some("sess"),
            Some("tools".into()),
            Some(fp(22)),
        );
        obs.complete("a2", 9_714, 0, 48_669, None);
        let event = obs.snapshot().last_event.expect("recache recorded");
        assert_eq!(event.event_kind, RecacheEventKind::Drift);
        assert_eq!(event.attribution_reason.as_deref(), Some("tools"));
        assert!(
            event.sibling_completed_recently,
            "sibling key b completed just before on the same session"
        );
    }

    /// A beta-header rotation between two turns of one stream is a ranked
    /// cause on the event, not just a witness. Neither drift lane sees
    /// headers; measured 2026-09-17, rotations coincide exactly with bust
    /// turns after 100+ stable turns, so filing them unexplained hid a
    /// genuine client cause in the residual bucket.
    #[test]
    fn a_beta_rotation_between_turns_is_flagged_on_the_event() {
        let _guard = super::super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("w1", "conv-wit".into(), None, None, Some(fp(20)));
        obs.note_forward_witnesses(
            "w1",
            "beta-aaa".into(),
            "sys[1]:1h,m19:1h".into(),
            "claude-opus-5".into(),
        );
        obs.complete("w1", 10_000, 46_985, 55_557, None);
        obs.begin_request("w2", "conv-wit".into(), None, None, Some(fp(22)));
        obs.note_forward_witnesses(
            "w2",
            "beta-bbb".into(),
            "sys[1]:1h,m19:1h".into(),
            "claude-opus-5".into(),
        );
        obs.note_replay_applied("w2", ReplayAppliedEvidence::new(2, 2, 0));
        obs.complete("w2", 9_714, 46_985, 48_669, None);
        let event = obs.snapshot().last_event.expect("recache recorded");
        assert_eq!(event.event_kind, RecacheEventKind::Drift);
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("forwarded_beta_rotated")
        );
        assert_eq!(event.origin.as_deref(), Some("client"));
        assert_eq!(event.scope.as_deref(), Some("cache_key"));
        assert!(event.beta_changed, "beta digest moved between turns");
        assert!(
            !event.markers_changed,
            "marker layout held still between turns"
        );
        assert_eq!(event.forward_beta.as_deref(), Some("beta-bbb"));
    }

    /// Pure conversation growth must not read as a marker-layout move. The
    /// tail breakpoint renumbers with every appended message, so the raw
    /// strings differ on any growing conversation while the breakpoint
    /// shape (count + kind + TTL) holds still. Regression test for the
    /// 2026-09-18 finding: `markers_changed` true on 41/41 residual events,
    /// all pure growth.
    #[test]
    fn growth_advanced_markers_are_not_a_layout_move() {
        let _guard = super::super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("g1", "conv-grow".into(), None, None, Some(fp(20)));
        obs.note_forward_witnesses(
            "g1",
            "beta-aaa".into(),
            "sys[1]:1h,m0.2:1h,m56.2:1h,m57.0:1h".into(),
            "claude-sonnet-5".into(),
        );
        obs.complete("g1", 10_000, 46_985, 55_557, None);
        obs.begin_request("g2", "conv-grow".into(), None, None, Some(fp(22)));
        obs.note_forward_witnesses(
            "g2",
            "beta-aaa".into(),
            "sys[1]:1h,m0.2:1h,m58.2:1h,m59.0:1h".into(),
            "claude-sonnet-5".into(),
        );
        obs.note_replay_applied("g2", ReplayAppliedEvidence::new(2, 2, 0));
        obs.complete("g2", 9_714, 46_985, 48_669, None);
        let event = obs.snapshot().last_event.expect("recache recorded");
        assert!(
            !event.markers_changed,
            "same shape at new indices is growth, not rotation"
        );
        assert_eq!(
            event.forward_markers.as_deref(),
            Some("sys[1]:1h,m0.2:1h,m58.2:1h,m59.0:1h"),
            "raw layout still logged for offline diff"
        );
    }

    /// The normalization must stay sensitive to what actually voids the
    /// prefix: a TTL move, a dropped marker, a kind change.
    #[test]
    fn marker_shape_changes_still_flag() {
        assert_ne!(
            UsageObserver::normalize_marker_layout("sys[1]:1h,m0.2:1h,m56.2:1h,m57.0:1h"),
            UsageObserver::normalize_marker_layout("sys[1]:1h,m0.2:1h,m56.2:1h,m57.0:5m"),
            "TTL downgrade flags"
        );
        assert_ne!(
            UsageObserver::normalize_marker_layout("sys[1]:1h,m0.2:1h,m56.2:1h,m57.0:1h"),
            UsageObserver::normalize_marker_layout("sys[1]:1h,m0.2:1h,m57.0:1h"),
            "dropped marker flags"
        );
        assert_ne!(
            UsageObserver::normalize_marker_layout("sys[1]:1h,m0.2:1h,m57.0:1h"),
            UsageObserver::normalize_marker_layout("m0.2:1h,m57.0:1h"),
            "dropped system marker flags"
        );
        assert_eq!(
            UsageObserver::normalize_marker_layout("sys[1]:1h,m0.2:1h,m56.2:1h,m57.0:1h"),
            UsageObserver::normalize_marker_layout("m57.0:1h,sys[1]:1h,m56.2:1h,m0.2:1h"),
            "entry order is not signal"
        );
        assert_eq!(
            UsageObserver::normalize_marker_layout(""),
            UsageObserver::normalize_marker_layout(""),
            "empty layouts compare equal, never as moved"
        );
    }

    /// A forwarded-model flap between two turns of one stream is flagged on
    /// the event as a witness only — unranked until a first real instance is
    /// measured (2026-09-17: zero flaps in-window). The router can rewrite
    /// the model after the compared fingerprint is taken, so only this
    /// post-router value says what the provider keyed on.
    #[test]
    fn a_forwarded_model_flap_between_turns_is_flagged_not_ranked() {
        let _guard = super::super::tests::miss_metric_test_lock();
        let obs = UsageObserver::new();
        obs.begin_request("m1", "conv-mod".into(), None, None, Some(fp(20)));
        obs.note_forward_witnesses(
            "m1",
            "beta-aaa".into(),
            "sys[1]:1h,m19:1h".into(),
            "claude-opus-5".into(),
        );
        obs.complete("m1", 10_000, 46_985, 55_557, None);
        obs.begin_request("m2", "conv-mod".into(), None, None, Some(fp(22)));
        obs.note_forward_witnesses(
            "m2",
            "beta-aaa".into(),
            "sys[1]:1h,m19:1h".into(),
            "claude-sonnet-5".into(),
        );
        obs.complete("m2", 9_714, 0, 48_669, None);
        let event = obs.snapshot().last_event.expect("recache recorded");
        assert!(event.model_changed, "forwarded model moved between turns");
        assert_eq!(event.forward_model.as_deref(), Some("claude-sonnet-5"));
        assert!(
            !event.beta_changed,
            "beta held still: this is a model flap, not a rotation"
        );
        assert_eq!(
            event.attribution_reason.as_deref(),
            Some("no_cause_found"),
            "witness only — unranked until a first real flap is measured"
        );
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
        let _guard = super::super::tests::miss_metric_test_lock();
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
