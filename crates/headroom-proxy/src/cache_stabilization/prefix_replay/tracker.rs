//! Per-request accounting for replay decisions.
//!
//! Moved out of `prefix_replay.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Per-session freeze-replay state across turns (Python `PrefixCacheTracker`).
#[derive(Clone, Debug)]
pub struct PrefixReplayTracker {
    pub(super) cached_token_count: u64,
    pub(super) cached_message_count: usize,
    pub(super) turn_number: u64,
    pub(super) last_activity: Instant,
    pub(super) last_original_messages: Vec<Value>,
    pub(super) last_forwarded_messages: Vec<Value>,
    /// [`forwarded_system_digest`] of the system block `last_forwarded_messages`
    /// went out under. The adoption gate compares it against the adopting
    /// turn's post-hold system: an adopted prefix only replays under a system
    /// the provider already holds. Updated on every served turn alongside the
    /// messages, so it always describes exactly what `last_forwarded_messages`
    /// describes — including content installed by adoption, whose digest the
    /// gate verified before installing.
    pub(super) last_forwarded_system_hash: String,
    /// Prefixes belonging to OTHER streams interleaved on this session.
    ///
    /// One session key carries several streams — a subagent inheriting its
    /// parent's context, a resumed transcript — which item 11 proved by message
    /// counts that run backwards under a single key. With one slot per session,
    /// stream B's turn is tested against stream A's stored prefix, fails the
    /// append-only guard, and forwards freshly compressed bytes for content the
    /// provider already had cached. That is a bust the proxy causes itself.
    ///
    /// Bounded and ordered most-recent-first. Each carries the id of the chain
    /// it belongs to, and the [`forwarded_system_digest`] of the system its
    /// messages went out under — same gate input as the primary, per entry,
    /// because a re-latched hold can move the system mid-lane.
    pub(super) alternates: Vec<(u64, Vec<Value>, Vec<Value>, String)>,
    /// Which chain the primary prefix belongs to. 0 before the first turn.
    ///
    /// A *chain* is a run of turns that each continue the previous one. It is
    /// the identity that every other key in this codebase only approximates:
    /// `session_key` is per-client, `conversation_key` hashes `system` plus the
    /// first message, and neither can tell two branches of one conversation
    /// apart. Message counts cannot either — compaction, a retry and a genuine
    /// second stream all make the count stop rising, and on 2026-08-09 three
    /// separate conclusions were drawn from that ambiguity and all three were
    /// wrong (item 25).
    ///
    /// The store already computes the answer to decide what to replay. This
    /// only gives it a name so it can be logged and grouped by.
    pub(super) primary_chain_id: u64,
    pub(super) next_chain_id: u64,
}

impl Default for PrefixReplayTracker {
    fn default() -> Self {
        Self {
            cached_token_count: 0,
            cached_message_count: 0,
            turn_number: 0,
            last_activity: Instant::now(),
            last_original_messages: Vec::new(),
            last_forwarded_messages: Vec::new(),
            last_forwarded_system_hash: String::new(),
            alternates: Vec::new(),
            primary_chain_id: 0,
            next_chain_id: 1,
        }
    }
}

impl PrefixReplayTracker {
    /// How many leading messages to skip compression on the next turn. Returns 0
    /// on the cold-start turn or when the cached prefix is below the min-token
    /// threshold.
    pub fn frozen_message_count(&self) -> usize {
        if self.turn_number == 0 {
            return 0;
        }
        if self.cached_token_count < MIN_CACHED_TOKENS {
            return 0;
        }
        self.cached_message_count
    }

    /// The previous turn's original (pre-compression) messages.
    pub fn last_original_messages(&self) -> &[Value] {
        &self.last_original_messages
    }

    /// The previous turn's forwarded (post-compression, byte-exact) messages.
    pub fn last_forwarded_messages(&self) -> &[Value] {
        &self.last_forwarded_messages
    }

    /// Record what we forwarded this turn and how many tokens the provider
    /// cached, computing the frozen-message boundary for the next turn (Python
    /// `update_from_response`).
    ///
    /// `original_messages` is this turn's pre-compression input; `forwarded` is
    /// exactly the bytes we sent upstream. When `original_messages` is `None`,
    /// `forwarded` is used for both (parity with Python).
    pub fn update_from_response(
        &mut self,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        forwarded: &[Value],
        original_messages: Option<&[Value]>,
        forwarded_system_hash: String,
    ) {
        self.last_activity = Instant::now();
        self.turn_number += 1;
        let mut incoming_original = original_messages.unwrap_or(forwarded).to_vec();
        // The last message can consist entirely of client-ephemeral blocks.
        // `place_tail_cache_breakpoints` seals before that message, so the
        // provider never cached it. Keeping it here would nevertheless make a
        // replacement at the same index fail the append-only guard next turn.
        //
        // Derive the boundary from the ORIGINAL slice. Relocation can attach a
        // reminder to a forwarded message whose original counterpart had none,
        // so inspecting the two tails independently can produce different
        // lengths and trip `ForwardedCountMismatch`. Derive the one bound from
        // the originals, then apply it to each slice from ITS OWN tail.
        //
        // Counting from the tail rather than reusing the index is what keeps
        // the two spans equal. The overlay replays a withdrawn scaffolding
        // message rather than dropping it, so a forwarded body can hold MORE
        // messages than the originals it came from, and from the insertion
        // point on the two no longer share an index. Truncating the forwarded
        // slice AT an originals index then cut real messages off its tail, and
        // next turn the splice read the short slice as covering the full stored
        // span: `prev_fwd` ended early, `optimized[consumed..]` resumed past the
        // gap, and the messages in between never reached the wire. Measured on
        // the 195 persisted prefixes of 2026-09-02, 15 had lost content this
        // way — 27 messages inserted, 27 client messages dropped, one for one.
        // Two of those sessions had also drawn a 400 from the API, because the
        // gap left a `role: "system"` reminder sitting behind an assistant
        // message.
        let stored_prefix_len = replayable_stored_prefix_len(&incoming_original);
        let trailing_dropped = incoming_original.len() - stored_prefix_len;
        incoming_original.truncate(stored_prefix_len);
        let mut incoming_forwarded = forwarded.to_vec();
        let forwarded_prefix_len = incoming_forwarded.len().saturating_sub(trailing_dropped);
        incoming_forwarded.truncate(forwarded_prefix_len);
        // One projection of this turn's messages for all three branch tests
        // below — see [`matches_canonical_prefix`].
        let canonical_incoming = canonicalize_slice(&incoming_original);
        // If this turn does not continue the prefix we are currently holding,
        // the two belong to different streams sharing this session. Keep the
        // displaced one instead of dropping it, so the stream it belongs to can
        // still replay on its next turn rather than busting.
        if !self.last_forwarded_messages.is_empty()
            && !matches_canonical_prefix(&self.last_original_messages, &canonical_incoming)
        {
            let displaced = (
                self.primary_chain_id,
                std::mem::take(&mut self.last_original_messages),
                std::mem::take(&mut self.last_forwarded_messages),
                std::mem::take(&mut self.last_forwarded_system_hash),
            );
            self.primary_chain_id = 0;
            self.alternates.retain(|(_, o, _, _)| o != &displaced.1);
            self.alternates.insert(0, displaced);
            let held_before_caps = self.alternates.len();
            // Spend the budget on the streams most likely to come back for it.
            //
            // Recency alone used to decide this, on the reasoning that what
            // falls off the end has gone quiet. It does not: an entry's
            // position records how many *other* streams have taken a turn
            // since, so a parent waiting on a fan-out ages exactly as fast as
            // one that has finished, and it is the largest and costliest entry
            // held. That is how a 300-message conversation came to be evicted
            // by 30-message subagents.
            //
            // Size is the better predictor, and measurably so. Over the
            // 2026-08-20/22 logs, the chance a stream ever takes another turn
            // rises monotonically with how much it has cached: 0% under 10k
            // tokens, 16% at 10-30k, 51% at 30-60k, 76% at 60-120k, 92% above
            // that (n=1,080, log-log r=0.54). Since the budget is counted in
            // messages and tokens track messages, value per unit of budget is
            // just that probability — so the budget belongs to the big streams,
            // and dropping a small one costs almost nothing.
            //
            // Recency still breaks ties, so among equals the freshest wins.
            let mut order: Vec<usize> = (0..self.alternates.len()).collect();
            order.sort_by_key(|&i| (std::cmp::Reverse(self.alternates[i].1.len()), i));
            let mut held = 0usize;
            let mut keep = vec![false; self.alternates.len()];
            let mut kept = 0usize;
            for &i in &order {
                let size = self.alternates[i].1.len();
                if kept >= MAX_ALTERNATE_PREFIXES {
                    break;
                }
                let next = held.saturating_add(size);
                // Skip rather than stop: a smaller stream can still fit in the
                // room a rejected larger one left behind. Always keep one, even
                // if it alone is over budget, so a session is never left with
                // nothing to replay against.
                if kept > 0 && next > MAX_ALTERNATE_MESSAGES {
                    continue;
                }
                held = next;
                keep[i] = true;
                kept += 1;
            }
            let mut seen = 0usize;
            self.alternates.retain(|_| {
                let keeping = keep[seen];
                seen += 1;
                keeping
            });
            // An evicted stream busts on its next turn and cannot say why — it
            // reports a miss with no stored prefix to name. The drop is the only
            // place it can be counted, and whether either cap is worth raising
            // is a question nothing else answers.
            let evicted = held_before_caps - self.alternates.len();
            if evicted > 0 {
                crate::observability::replay_alternates::observe_alternates_evicted(evicted as u64);
            }
        }
        // Whichever chain this turn continues, it inherits that chain's id;
        // continuing nothing starts a new one. This is the only place a chain
        // is born, so an id names one unbroken run of turns for as long as the
        // tracker lives.
        if self.primary_chain_id == 0 {
            self.primary_chain_id = self
                .alternates
                .iter()
                .find(|(_, o, _, _)| matches_canonical_prefix(o, &canonical_incoming))
                .map(|(id, _, _, _)| *id)
                .unwrap_or_else(|| {
                    let id = self.next_chain_id;
                    self.next_chain_id += 1;
                    id
                });
        }
        // This turn continues one of the alternates? Then it is no longer an
        // alternate — it is the live prefix, and holding it twice would let a
        // stale copy win a later match.
        self.alternates
            .retain(|(_, o, _, _)| !matches_canonical_prefix(o, &canonical_incoming));
        self.last_original_messages = incoming_original;
        self.last_forwarded_messages = incoming_forwarded;
        self.last_forwarded_system_hash = forwarded_system_hash;

        let total_cached = cache_read_tokens + cache_write_tokens;
        if total_cached == 0 {
            self.cached_token_count = 0;
            self.cached_message_count = 0;
            return;
        }

        // Estimate positions over the complete upstream request, not the
        // replay-state slice. The reported cached total stops accumulation
        // before the uncached ephemeral tail; only storage is truncated above.
        let counts = estimate_message_tokens(forwarded);
        let mut accumulated: u64 = 0;
        let mut frozen_count = 0usize;
        for (i, tok) in counts.iter().enumerate() {
            accumulated += *tok;
            if accumulated <= total_cached {
                frozen_count = i + 1;
            } else {
                break;
            }
        }
        self.cached_token_count = total_cached;
        self.cached_message_count = frozen_count;
    }

    /// Drop the stored prefix. Called on a drift/rebuild boundary: the bytes the
    /// provider cached are gone, so replaying the stored prefix would be stale.
    /// The next turn starts a fresh replay chain.
    pub fn invalidate(&mut self) {
        self.cached_token_count = 0;
        self.cached_message_count = 0;
        self.last_original_messages.clear();
        self.last_forwarded_messages.clear();
        // Alternates are prefixes for the same dead cache; leaving them would
        // let an invalidated stream replay after the boundary that killed it.
        self.alternates.clear();
        // The chain is broken here by definition, so the next turn starts a new
        // one rather than silently extending the id across the boundary.
        self.primary_chain_id = 0;
    }

    pub fn turn_number(&self) -> u64 {
        self.turn_number
    }
}
