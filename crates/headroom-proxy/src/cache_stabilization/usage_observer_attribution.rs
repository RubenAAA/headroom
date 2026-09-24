//! usage_observer::attribution — split from usage_observer.rs (pure move, no logic change).
use super::*;

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
pub(super) enum CacheLanding {
    /// `actual == prev_read`: the write the previous turn made was not found.
    /// Typical at gaps under 3s.
    MissedNewestWrite,
    /// `prev_read < actual < prev_boundary`: the read stops inside the
    /// previous write. Clusters just inside the boundary (30/39 events below
    /// one-third of the write on 2026-09-17) — the sliver served varies by
    /// conversation, so no fixed token constant holds. Fable once showed a
    /// fixed 69–114 tokens; the same window elsewhere ran 209–4575.
    PartialOfPreviousWrite,
    /// `actual == prevprev_boundary` while `prev_read > prevprev_boundary`:
    /// the previous turn read past anything ever written, so the provider
    /// served a segment it never persisted, then lost it.
    FreeReadNotPersisted,
    /// `actual < prevprev_boundary` (or `actual < prev_read` when no earlier
    /// boundary is known): an older entry is gone, with the prefix stable.
    DroppedOlderEntry,
    /// Below `prev_read` but at or above the older boundary. Measured
    /// 2026-09-17 landing at that boundary plus a few-hundred-token sliver
    /// (median fractional position 0.01, 28/29 events): a full-generation-
    /// stale snapshot serve, not a position between entries. Non-monotonic —
    /// served less than the stream previously read — so commit latency alone
    /// cannot produce it; replica lag or eviction must cover these.
    BetweenEntries,
}

impl CacheLanding {
    pub(super) fn classify(
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

    pub(super) fn as_str(self) -> &'static str {
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
pub(super) struct RecacheAttribution<'a> {
    pub(super) reason: Option<&'a str>,
    pub(super) origin: Option<&'static str>,
    pub(super) scope: Option<&'static str>,
    pub(super) counts_as_waste: bool,
}

/// Whether every dimension the client moved was held back before the wire.
///
/// Only `body_to_send` is a cache-key input, and the stabilizers exist to stop
/// a client edit from reaching it: the roster pin puts a flapped tool back, the
/// working-directory hold puts the old `cd` back. When one of them absorbs the
/// edit, the inbound dims still name it, and naming it as the cause of a
/// re-cache blames a change that never left this machine. On 2026-09-15 a
/// client dropped `WaitForMcpServers` from `tools[]`; the pin held the
/// forwarded roster byte-identical across the turn (outbound dims said
/// `early_messages`, never `tools`), and the event still reported `tools`.
///
/// Both lanes hash the same three dimensions, so a shared name is enough to
/// clear the edit: it says this dimension moved on the wire too, and the client
/// moving it first is then the cause the caller already prefers. Only disjoint
/// sets are absorption, and then the caller falls through to the evidence that
/// speaks for the forwarded body.
///
/// The outbound side is a tri-state and all three matter. Dims that overlap
/// clear the edit; dims that do not are absorption; and an *empty* string is
/// the strongest absorption there is — the lane was compared and the forwarded
/// body held still in every dimension, so nothing the client did reached the
/// provider. Only `None`, which means no comparison was available (a birth
/// turn, or a body that never reached the forwarding path), is an absence, and
/// then the inbound reading stands.
///
/// An empty inbound side is never absorption either: it is the case the head
/// check below exists for, and suppressing that on a silent lane would lose the
/// one cause those turns have.
pub(super) fn client_edit_was_absorbed(
    inbound_dims: Option<&str>,
    outbound_dims: Option<&str>,
) -> bool {
    let (Some(inbound), Some(outbound)) =
        (inbound_dims.filter(|dims| !dims.is_empty()), outbound_dims)
    else {
        return false;
    };
    !inbound
        .split(',')
        .any(|dim| outbound.split(',').any(|other| other == dim))
}

// One flat parameter per independent evidence flag; bundling them into a
// struct would churn every call site below for no gain.
#[allow(clippy::too_many_arguments)]
pub(super) fn recache_attribution<'a>(
    drift_dims: Option<&'a str>,
    head_changed: bool,
    beta_changed: bool,
    outbound_drift_dims: Option<&'a str>,
    replay_skip: Option<ReplaySkipEvidence>,
    replay_applied: Option<ReplayAppliedEvidence>,
    previous_turn_diverged: bool,
    previous_turn_had_continuation: bool,
    concurrent_with_in_flight: bool,
) -> RecacheAttribution<'a> {
    let absorbed = client_edit_was_absorbed(drift_dims, outbound_drift_dims);

    // A final-message replacement is a branch build only when the hot zone
    // held still. The tail check looks at message counts alone, so a turn
    // that also moved system/tools (or the cacheable head) would otherwise
    // file genuine waste as a zero-waste branch. Hot-zone evidence wins here;
    // the ranking below then names it.
    if replay_skip.is_some_and(ReplaySkipEvidence::is_inbound_tail_replacement)
        && drift_dims
            .filter(|dims| !dims.is_empty() && !absorbed)
            .is_none()
        && (!head_changed || absorbed)
    {
        return RecacheAttribution {
            reason: Some("inbound_tail_replaced"),
            origin: Some("inbound"),
            scope: Some("final_message"),
            counts_as_waste: false,
        };
    }

    if let Some(dims) = drift_dims.filter(|dims| !dims.is_empty() && !absorbed) {
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
    //
    // Skipped when the dims above were absorbed, because then this check is
    // reading the same held-back edit one turn wider: the roster pin keeps the
    // forwarded tools steady but the fingerprint is taken on the client's own
    // body, so a flapped tool moves the head here while the provider was keyed
    // on bytes that never changed. An empty `drift_dims` is not that case and
    // still lands here, which is the whole point of the check.
    if head_changed && !absorbed {
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

    // The forwarded `anthropic-beta` header rotated since the previous
    // completed turn of this stream. The header is client-supplied and part
    // of the provider's cache key, so a rotation voids the whole prefix with
    // both drift lanes quiet: the lanes hash the body only. Measured
    // 2026-09-17: on 3 recache turns the forwarded model/system/tools
    // digests held still across 100–200-turn sessions while beta flipped
    // exactly on the bust turn (88t, 88t, 15,171t).
    //
    // Ranked below the client structural evidence above, never above it:
    // when drift, head, or a declined replay already names a client cause,
    // that label stands — this arm is for the otherwise-clean miss that
    // used to file as a commit race or unexplained. Ranked above the
    // outbound hash by the standing rule that client-origin evidence beats
    // proxy-origin: the client sent this header, the proxy only forwards it.
    //
    // Deliberately beta only, not `markers_changed`: the marker layout moves
    // with normal conversation growth (the breakpoint stage runs every
    // turn), so ranking it would fire constantly; beta headers hold still
    // for hundreds of turns, which is what makes a rotation a signal.
    // The inbound-tail branch gate above is untouched: a genuine tail build
    // stays a tail build even when beta moved, and its shortfall already
    // rides the line as `uncharged_shortfall_tokens`.
    if beta_changed {
        return RecacheAttribution {
            reason: Some("forwarded_beta_rotated"),
            origin: Some("client"),
            scope: Some("cache_key"),
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
        // The previous turn ran hidden CCR continuation rounds: its cache
        // write committed a prefix with proxy-private retrieval messages
        // appended, so this turn's replayed client-baseline prefix cannot
        // match the provider's newest write. Measured 2026-09-24: 135 of
        // 140 unexplained recaches had a continuation on the previous turn
        // (1% base rate). Named, still waste — the rewrite is real.
        if previous_turn_had_continuation {
            return RecacheAttribution {
                reason: Some("aftershock_of_continuation"),
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
