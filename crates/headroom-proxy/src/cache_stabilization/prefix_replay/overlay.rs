//! Prefix overlay decisions and miss/skip reporting.
//!
//! Moved out of `prefix_replay.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Why the store had no prefix to hand back for a session.
///
/// Splits the commonest replay-decline reason into causes that need different
/// responses: a first turn costs nothing, an idle gap past the TTL is already
/// lost (the provider's cache expires in 5 minutes, this store holds 10), and a
/// session that should have had a tracker but did not is a real defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixMiss {
    /// No tracker under this session key. Either the session's first turn, an
    /// LRU eviction, or — the case worth chasing — a session key that is not
    /// stable turn to turn.
    NoTrackerForSession,
    /// The tracker existed but had not been touched inside the session TTL.
    /// The provider's own cache expired long before this did, so the following
    /// re-cache is a TTL expiry rather than anything the proxy caused.
    IdlePastTtl,
    /// A tracker exists but no turn has completed on it yet, so there are no
    /// forwarded bytes to replay.
    NothingForwardedYet,
    /// Stored messages lead this turn but under a different system block.
    /// Replaying them would splice donor bytes under a system no provider
    /// cache holds — a miss that reports as a replay. The turn forwards its
    /// own bytes instead: the same provider outcome, honestly measured.
    SystemChanged,
    /// The tracker mutex was poisoned by a panicking task.
    LockPoisoned,
}

impl PrefixMiss {
    /// Stable label for logs and dashboards.
    pub fn as_str(self) -> &'static str {
        match self {
            PrefixMiss::NoTrackerForSession => "no_tracker_for_session",
            PrefixMiss::IdlePastTtl => "idle_past_ttl",
            PrefixMiss::NothingForwardedYet => "nothing_forwarded_yet",
            PrefixMiss::SystemChanged => "system_changed",
            PrefixMiss::LockPoisoned => "lock_poisoned",
        }
    }
}

/// Why a turn forwarded its own bytes instead of replaying the cached prefix.
///
/// The caller used to record only whether the overlay changed anything, which
/// collapses five very different situations into one boolean. That matters
/// because turns without a replay are where the money goes: measured over the
/// 2026-08-08/09 logs, requests that did not replay were 19% of traffic and
/// carried 97% of all booked re-cache waste — roughly 80,500 wasted tokens per
/// event against 2,300 for turns that did replay. Naming the reason is what
/// makes that 19% addressable instead of merely visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySkip {
    /// Nothing stored for this session yet: its first turn, a TTL expiry, or an
    /// eviction. Benign on a first turn and expensive on the others.
    NoPreviousTurn,
    /// The stored turn forwarded fewer messages than it took in, so the two
    /// slices no longer cover one span of the conversation.
    ForwardedCountMismatch,
    /// The replayed prefix is byte-larger than this turn's own output and
    /// nothing in it is provider-confirmed, so the fresh (usually
    /// recompressed-smaller) bytes go out instead. This is the designed
    /// cold-cache re-baseline, not a defect: when the provider count collapses
    /// the floor collapses with it and every accumulated improvement lands at
    /// once, which is what bounds long-session growth. Partial floors never
    /// report this — they split at the floor instead (see
    /// [`split_inflated_replay_at_floor`]).
    InflatedWithoutConfirmedFloor,
    /// The spliced output would put a `role: "system"` message where the API
    /// refuses one. Forwarding this turn's own bytes costs a re-cache; the 400
    /// it replaces costs the same re-cache plus the turn.
    SystemAdjacencyBroken,
    /// This turn is **shorter** than the stored prefix. A conversation only
    /// grows, so this is the fingerprint of two interleaved streams sharing one
    /// session key — the store holds one prefix per session, so the longer
    /// stream's prefix blocks the shorter one and vice versa (see item 11).
    ShorterThanStoredPrefix,
    /// The optimized output is shorter than the stored prefix — the pipeline
    /// dropped messages the prefix covers.
    OptimizedShorterThanPrefix,
    /// The optimized output holds fewer messages than the client sent, so
    /// `optimized[i]` is no longer this turn's message `i`.
    ///
    /// Everything here indexes the two lists against each other: the stored
    /// prefix is measured against `current_original_messages` and the tail is
    /// then taken from `optimized_messages` at the same offset. That only works
    /// because the passes in front of this one rewrite messages in place and
    /// append at the tail — none of them deletes from the middle.
    ///
    /// It held on every turn it could be measured (89 records carrying both
    /// counts on 2026-08-26, delta 0 in all of them), and it was an unchecked
    /// assumption for as long as the splice has existed. A pass that starts
    /// dropping a message would silently shift the tail and splice the wrong
    /// bytes onto the cached prefix, which is a correctness fault rather than a
    /// lost cache hit. So it is checked, and declining is the safe answer.
    OptimizedShorterThanOriginals,
    /// The leading messages changed under canonicalization, so replaying the
    /// stored bytes would forward content the client did not send.
    ///
    /// Refusing is correct. That was doubted for a while — one conversation
    /// declined on 38 consecutive turns while growing two messages at a time,
    /// which looked like churn the canonicalizer had failed to neutralise —
    /// but capture settled it on 2026-08-11. Of 71 events where the current
    /// message carried `thinking` blocks the stored one lacked, 64 compared a
    /// user-role `tool_result` against an assistant `thinking,tool_use` at the
    /// same index. Roles alternate, so the list had shifted: the client
    /// deletes whole messages from mid-history. Growth of two per turn hides
    /// it, because a deletion in the middle and two arrivals at the end net
    /// out to the same count.
    ///
    /// So the stored prefix holds a message the client removed, and replaying
    /// it to save the cache would re-send content the client threw out. There
    /// is nothing to win either way: measured over 311 diverged turns, median
    /// creation falls 55,524 -> 28,153 -> 18,009 as the edit moves from the
    /// first quarter of the prefix to the third, with no replay involved. The
    /// provider already reads up to the deletion and rebuilds only what
    /// follows.
    ///
    /// Carries the first message index that differs, and how many leading
    /// messages were replayed from the stored prefix anyway (see
    /// [`overlay_cached_prefix_reported`] — since 2026-08-17 that is all of them
    /// up to the divergence, where it used to be none).
    PrefixContentDiverged {
        first_diff_index: usize,
        replayed_prefix_msgs: usize,
    },
}

impl ReplaySkip {
    /// Stable label for logs and dashboards.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplaySkip::NoPreviousTurn => "no_previous_turn",
            ReplaySkip::ForwardedCountMismatch => "forwarded_count_mismatch",
            ReplaySkip::InflatedWithoutConfirmedFloor => "inflated_without_confirmed_floor",
            ReplaySkip::SystemAdjacencyBroken => "system_adjacency_broken",
            ReplaySkip::ShorterThanStoredPrefix => "shorter_than_stored_prefix",
            ReplaySkip::OptimizedShorterThanPrefix => "optimized_shorter_than_prefix",
            ReplaySkip::OptimizedShorterThanOriginals => "optimized_shorter_than_originals",
            ReplaySkip::PrefixContentDiverged { .. } => "prefix_content_diverged",
        }
    }
}

/// Compact-JSON byte length, the unit the non-inflation bound compares in.
///
/// Port of Python `_compact_json_bytes`: `serde_json::to_vec` already emits
/// compact separators with unescaped UTF-8, matching `separators=(",", ":")`
/// plus `ensure_ascii=False`. `None` when sizing cannot be proved; the bound
/// treats that as tripped, the same fail direction as upstream.
pub(super) fn compact_json_len(messages: &[Value]) -> Option<usize> {
    serde_json::to_vec(messages).ok().map(|bytes| bytes.len())
}

/// What to forward after the non-inflation bound trips on a splice.
pub(super) enum InflatedReplay {
    /// Forward the splice whole: the bound passed, the floor covers it, or
    /// the span is withdrawal-shifted under a non-zero floor (see
    /// [`split_inflated_replay_at_floor`] for why that span is atomic).
    Whole(Vec<Value>),
    /// Forward the confirmed head replayed and the rest fresh.
    Split(Vec<Value>),
    /// Nothing confirmed: forward this turn's own bytes.
    Fresh,
}

/// Split an inflated splice at the provider-confirmed floor.
///
/// `head_len` counts the splice's leading messages that come from the stored
/// forwarded prefix (`prev_fwd.len()` on the aligned path, `replay_upto` on
/// the diverged path); the rest is this turn's own tail. `floor` is already
/// clamped to `head_len`. `shifted` says the stored pair no longer shares one
/// index space — the overlay replayed a scaffolding message the client
/// withdrew, so `prev_fwd` runs longer than `prev_orig` and the two disagree
/// past the insertion point.
///
/// The bound itself is the port of upstream #3052 this overlay never had: the
/// replayed bytes must not exceed this turn's own output, so a fresh
/// compression improvement beyond the floor reaches the wire instead of being
/// pinned behind stale forwarded bytes. The floor is the port of upstream
/// `aebe9895`: inside the provider-confirmed prefix the replay source is
/// exactly what the provider hashed, so changing those bytes can only bust
/// the cache — replay there is unconditional, and a collapsed floor (cold
/// cache, TTL lapse) lets every accumulated improvement land at once, which
/// bounds long-session growth.
///
/// Which wins when the two mechanisms meet a withdrawal — the floor or the
/// alignment span: the span. Resuming `optimized` mid-span at a forwarded
/// index would read a current-space position that no longer names the same
/// message past the insertion point, dropping or duplicating client content
/// to save a cache entry. So a shifted span is atomic: with anything confirmed
/// it replays whole (the withdrawn bytes are provider-cached history —
/// forwarded last turn — small standalone scaffolding by construction, and a
/// non-zero floor says the cache is warm), and only a zero floor sends the
/// turn out fresh. Correctness outranks economy; the improvement lands on the
/// next cold turn.
/// Non-inflation bound for a replayed splice: its compact-JSON bytes must
/// not exceed this turn's own output. Sizing unprovable: same fail
/// direction as upstream — treat as tripped.
/// Extracted from `split_inflated_replay_at_floor` without behavior change.
pub(super) fn replay_within_bound(spliced: &[Value], optimized_messages: &[Value]) -> bool {
    match (
        compact_json_len(spliced),
        compact_json_len(optimized_messages),
    ) {
        (Some(replayed), Some(optimized)) => replayed <= optimized,
        // Sizing unprovable: same fail direction as upstream — treat as tripped.
        (None, _) | (_, None) => false,
    }
}

pub(super) fn split_inflated_replay_at_floor(
    spliced: Vec<Value>,
    prev_fwd: &[Value],
    optimized_messages: &[Value],
    head_len: usize,
    floor: usize,
    shifted: bool,
) -> InflatedReplay {
    if floor >= head_len {
        // The floor covers the whole replayed head: unconditional, and the
        // bound is moot — this is also the fast path, skipping two whole-body
        // serialisations on every warm-cache turn.
        return InflatedReplay::Whole(spliced);
    }
    if spliced.is_empty() || optimized_messages.is_empty() {
        // Degenerate body either way; declining can only drop what the splice
        // kept, so keep the splice.
        return InflatedReplay::Whole(spliced);
    }
    if replay_within_bound(&spliced, optimized_messages) {
        return InflatedReplay::Whole(spliced);
    }
    if floor == 0 {
        tracing::debug!(
            event = "prefix_replay_inflated_without_floor",
            replayed_head_msgs = head_len,
            "replay inflated this turn's output with nothing provider-confirmed; forwarding fresh"
        );
        return InflatedReplay::Fresh;
    }
    if shifted {
        // Atomic span (see doc comment): a mid-span split would resume the
        // tail at a shifted index. The floor is non-zero, so replay whole.
        tracing::debug!(
            event = "prefix_replay_floor_keeps_shifted_span",
            replayed_head_msgs = head_len,
            confirmed_floor_msgs = floor,
            "replay inflated but the span carries a replayed withdrawal; splitting would resume at a shifted index, so the confirmed floor keeps the whole span"
        );
        return InflatedReplay::Whole(spliced);
    }
    let floor = floor.min(optimized_messages.len());
    let mut out = prev_fwd[..floor].to_vec();
    out.extend_from_slice(&optimized_messages[floor..]);
    tracing::debug!(
        event = "prefix_replay_split_at_confirmed_floor",
        replayed_head_msgs = head_len,
        confirmed_floor_msgs = floor,
        "replay inflated beyond the confirmed floor; replaying the confirmed head, forwarding the rest fresh"
    );
    InflatedReplay::Split(out)
}

/// [`overlay_cached_prefix`], but reporting why it declined.
///
/// Returns `(messages, None)` when the whole cached prefix was replayed, and
/// `(messages, Some(reason))` otherwise. A `Some` no longer implies the messages
/// came back untouched: on a content divergence the leading run that still
/// agrees is spliced in — see the comment on that path.
///
/// `continues_chain` says whether the stored prefix is this conversation's own.
/// `previous_turn_for` falls back to the session's most recent prefix when
/// nothing continues it, reporting `chain_id = 0`; splicing another stream's
/// bytes in on the strength of a shared opener would forward compressed content
/// whose referents live in a conversation this one never had.
///
/// `confirmed_frozen_count` is the provider-confirmed floor (port of upstream
/// `aebe9895`): how many leading messages the provider has confirmed cached.
/// Inside the floor the replay is unconditional and the non-inflation bound
/// (compact-JSON bytes of the splice must not exceed this turn's own output)
/// arbitrates only beyond it — a shrinking replay still repairs drift, an
/// inflating one lets the fresh improvement through. `None` disables the
/// bound (the historical posture); production passes
/// `Some(store.confirmed_frozen_count(session_key))`, which is `Some(0)` on a
/// cold cache so improvements land at once instead of pinning forever. The
/// alignment guards and the span atomicity above are never relaxed by the
/// floor — see [`split_inflated_replay_at_floor`] for which wins where they meet.
pub fn overlay_cached_prefix_reported(
    optimized_messages: Vec<Value>,
    current_original_messages: &[Value],
    previous_original_messages: Option<&[Value]>,
    previous_forwarded_messages: Option<&[Value]>,
    continues_chain: bool,
    confirmed_frozen_count: Option<usize>,
) -> (Vec<Value>, Option<ReplaySkip>) {
    let (prev_orig, prev_fwd) = match (previous_original_messages, previous_forwarded_messages) {
        (Some(o), Some(f)) if !o.is_empty() && !f.is_empty() => (o, f),
        _ => return (optimized_messages, Some(ReplaySkip::NoPreviousTurn)),
    };
    let n = prev_orig.len();
    // The stored pair covers one span of the conversation, and the frozen
    // prefix must fit within both the current originals and this turn's
    // optimized output.
    //
    // A floor, not an equality. The two slices held equal counts only while the
    // overlay never added a message; it replays scaffolding the client withdrew,
    // so a forwarded slice legitimately runs LONGER than its originals by the
    // number of messages stepped over. Fewer is still a desync — no pass in
    // front of this one deletes a message — and declining is the safe answer.
    if prev_fwd.len() < n {
        return (optimized_messages, Some(ReplaySkip::ForwardedCountMismatch));
    }
    // The index correspondence the splice rests on: `optimized[i]` is this
    // turn's message `i`. The passes in front rewrite in place and append at
    // the tail, so this is a floor and not an equality — a list that came back
    // SHORTER than the client's means something was dropped and the tail has
    // shifted under us.
    //
    // Held on `n` so a pipeline that collapsed the stored prefix away keeps its
    // own name; this covers what that check cannot see, a drop past the prefix.
    if optimized_messages.len() >= n && optimized_messages.len() < current_original_messages.len() {
        return (
            optimized_messages,
            Some(ReplaySkip::OptimizedShorterThanOriginals),
        );
    }
    // Match the stored prefix to this turn before measuring either against the
    // other by index. With no withdrawal in the way this walks the two in step
    // and lands on `n`, which is the compare the guard below used to do on its
    // own; with one it lands short, and the tail is taken from there. The two
    // length checks under this stay where they are: they read `n` against this
    // turn, and a withdrawal makes that comparison the wrong one to fail on.
    let Some(consumed) = align_over_withdrawn_scaffolding(prev_orig, current_original_messages)
    else {
        if current_original_messages.len() < n {
            return (
                optimized_messages,
                Some(ReplaySkip::ShorterThanStoredPrefix),
            );
        }
        if optimized_messages.len() < n {
            return (
                optimized_messages,
                Some(ReplaySkip::OptimizedShorterThanPrefix),
            );
        }
        // Append-only guard on CONTENT ONLY (#1852): compare with the shared
        // canonicalizer so the guard is robust to ALL per-turn transport /
        // annotation churn — cache_control movement, litellm `caller`, streaming
        // `index`, string↔block content shape, etc.
        //
        // Deliberately blind to `<system-reminder>` churn: the canonicalizer
        // filters those spans out, so a reminder the client attached or withdrew
        // inside this region does not count as divergence and the turn still
        // replays. That is the whole point. Claude Code withdraws a reminder from
        // the message it decorated a turn earlier, which lands in the prefix TAIL
        // where the breakpoints are, so treating it as divergence rebuilds nearly
        // the whole prefix — measured 2026-08-16 at 151k creation against 18k read
        // on a single turn, eleven turns of savings for one withdrawn span.
        //
        // The cost of the blindness is that replay forwards the stored copy, so a
        // withdrawn reminder stays on the wire and history keeps one per decorated
        // message. Those bytes sit INSIDE the cached prefix and bill at 0.1x. The
        // 382%-of-client-body growth recorded here on 2026-08-11 was read as the
        // price of this and it was not: it was the relocation pass parking its
        // block past the last breakpoint at 1.0x, 64% of all billed weight when it
        // was finally measured on 2026-08-16. Relocation is gone. Watch
        // `outbound_body_bytes` against `client_request_bytes` — a ratio climbing
        // past ~1.2 means the accumulation is real after all and this is wrong.
        // Locate the first disagreement. The alignment walk above already told
        // us there is one — it accepts everything an index-aligned compare
        // accepts, and more — so this only runs on the decline path and never
        // touches a turn that replays cleanly.
        let first_diff_index = (0..n)
            .find(|&i| {
                canonicalize_for_prefix_compare(&current_original_messages[i])
                    != canonicalize_for_prefix_compare(&prev_orig[i])
            })
            .unwrap_or(n);
        // Replay the leading run that still agrees; take this turn's own bytes
        // from the divergence on.
        //
        // This was tried on 2026-08-09, measured worse, and reverted. The
        // premise recorded then was that a declined replay is not a bust
        // because compression is deterministic, so this turn's own bytes for an
        // unchanged prefix reproduce what the provider already cached. Capture
        // refutes it. Early messages carry `<system-reminder>` spans that the
        // client attaches and withdraws over the life of a conversation, and
        // replay freezes whichever form was current when the chain started. The
        // stored copy and a freshly computed one therefore disagree at message
        // 0, so a decline does not reproduce the cached bytes — it misses at
        // the very first message and rebuilds the entire conversation.
        //
        // Measured on the 2026-08-17 capture: 5 declined turns, every one of
        // them diverging in the last 1% of the prefix (median 99.3% depth),
        // between them stranding 443,541 tokens — 55% of all recorded waste.
        // Splicing recovers cache creation by 25.6% and the bill by 16.3% at
        // the fitted subscription weights. The 2026-08-09 measurement was taken
        // while the relocation pass was live, which lifted blocks out of
        // history and re-appended them every turn; a spliced prefix could not
        // match under it. Relocation is gone.
        //
        // Only the run that agrees is replayed, so this stays correct when the
        // divergence is a mid-history deletion: the removed message sits at or
        // after `first_diff_index` and never comes from the stored copy.
        let replay_upto = if continues_chain {
            first_diff_index
                .min(prev_fwd.len())
                .min(optimized_messages.len())
        } else {
            0
        };
        let skip = ReplaySkip::PrefixContentDiverged {
            first_diff_index,
            replayed_prefix_msgs: replay_upto,
        };
        if replay_upto == 0 {
            return (optimized_messages, Some(skip));
        }
        let mut out = prev_fwd[..replay_upto].to_vec();
        out.extend_from_slice(&optimized_messages[replay_upto..]);
        // The floor arbitrates the bound, never the guards: a divergence still
        // declines when nothing agrees, and a split only moves where the fresh
        // tail resumes — which can only forward more of this turn's own bytes.
        // The reported count follows the split down when one happens.
        let mut replayed_prefix_msgs = replay_upto;
        out = match confirmed_frozen_count {
            None => out,
            Some(confirmed) => match split_inflated_replay_at_floor(
                out,
                prev_fwd,
                &optimized_messages,
                replay_upto,
                confirmed.min(replay_upto),
                prev_fwd.len() > n,
            ) {
                InflatedReplay::Whole(out) => out,
                InflatedReplay::Split(out) => {
                    replayed_prefix_msgs = confirmed.min(replay_upto).min(optimized_messages.len());
                    out
                }
                InflatedReplay::Fresh => {
                    return (
                        optimized_messages,
                        Some(ReplaySkip::InflatedWithoutConfirmedFloor),
                    );
                }
            },
        };
        if first_illegal_system_position(&out).is_some() {
            return (optimized_messages, Some(ReplaySkip::SystemAdjacencyBroken));
        }
        let skip = ReplaySkip::PrefixContentDiverged {
            first_diff_index,
            replayed_prefix_msgs,
        };
        return (out, Some(skip));
    };
    if optimized_messages.len() < consumed {
        return (
            optimized_messages,
            Some(ReplaySkip::OptimizedShorterThanPrefix),
        );
    }
    // Replay the cached (compressed) prefix verbatim; keep this turn's tail.
    //
    // Verbatim is what makes the guard above safe. It ignores a
    // `<system-reminder>` the client attached or withdrew inside this region,
    // and these bytes are the ones the provider cached last turn, so the churn
    // it ignored never reaches the wire. That agreement used to come from the
    // relocation pass, which lifted every span onto the newest message before
    // the request got here; it cost 64% of all billed weight (measured
    // 2026-08-16) and is gone.
    //
    // So nothing is stripped on the way out. Stripping the stored copies made
    // sense only while relocation guaranteed this turn's tail carried the same
    // spans; doing it now would rewrite history on every turn and throw away
    // the read. The `ReminderInsidePrefix` decline that stood here went with
    // it — it guarded the strip, and with history holding its own spans it
    // would fire on every turn.
    //
    // `prev_fwd` goes out whole even when the walk stepped over a withdrawn
    // scaffolding message, so that message is still on the wire. That is the
    // point: those are the bytes the provider cached, and the client dropping
    // the message does not change what is sitting in the cache. `consumed` is
    // where this turn's own messages resume, which is `n` unless something was
    // withdrawn.
    let mut out = prev_fwd.to_vec();
    out.extend_from_slice(&optimized_messages[consumed..]);
    // The confirmed floor (port of upstream `aebe9895`): positions the
    // provider confirmed cached replay unconditionally — the replay source is
    // exactly what the provider hashed — while the non-inflation bound keeps
    // arbitrating beyond the floor, so a fresh compression improvement still
    // reaches the wire. `None` keeps this overlay's historical posture (no
    // bound); production always passes the tracker's count. The alignment span
    // from the withdrawal work stays atomic under a partial floor — see
    // [`split_inflated_replay_at_floor`].
    out = match confirmed_frozen_count {
        None => out,
        Some(confirmed) => match split_inflated_replay_at_floor(
            out,
            prev_fwd,
            &optimized_messages,
            prev_fwd.len(),
            confirmed.min(prev_fwd.len()),
            prev_fwd.len() > n,
        ) {
            InflatedReplay::Whole(out) | InflatedReplay::Split(out) => out,
            InflatedReplay::Fresh => {
                return (
                    optimized_messages,
                    Some(ReplaySkip::InflatedWithoutConfirmedFloor),
                );
            }
        },
    };
    if first_illegal_system_position(&out).is_some() {
        return (optimized_messages, Some(ReplaySkip::SystemAdjacencyBroken));
    }
    (out, None)
}
