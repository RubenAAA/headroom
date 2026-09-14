# Learning: token flow audit 2026-08-20/22

- **Source:** `docs/notes/recache-classification.md` (82 MB logs, 14,290 turns, 45.9M creation tokens)
- **Claim:** 91.4% of turn-pairs get full reuse; loss concentrates in 51 no-lead turns (2.59M, ~50k each) — the alternates-cap root cause below.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Where the tokens actually go — 2026-08-23

Measured over 82MB of logs, 2026-08-20 to 08-22: 14,290 turns, 45.9M tokens of
cache creation. Attribution by cause:

| cause | turns | creation | share |
|---|---|---|---|
| replay applied (healthy) | 13,604 | 31.7M | 68.9% |
| cold start, no stored prefix — legitimate | 363 | 11.0M | 23.9% |
| **no held stream leads the turn** | **51** | **2.59M** | **5.6%** |
| declined, real chain, content diverged | 271 | 0.52M | 1.1% |
| shorter than stored prefix | 7 | 0.15M | 0.3% |

91.4% of consecutive turn-pairs get full reuse. The system is mostly working,
and the loss is concentrated: 51 turns carry 2.59M tokens, ~50k each. That is
the one number worth chasing.

### Refuted, with the numbers that killed each

**H-marker: the tail breakpoint moving orphans the previous block.** Both
markers do slide forward every turn and no path re-declares an old position
(`place_tail_cache_breakpoints`, `prefix_replay.rs:1726`). But `cache_control`
is in `NON_SEMANTIC_KEYS` (`prefix_replay.rs:391`) and is stripped before any
prefix comparison, and the provider matches on content, not marker parity.
Dead.

**H-slack: `TAIL_EDIT_SLACK = 2` is off by one.** The tail-edit rescue needs
the divergence within 2 messages of the stored prefix's end. Gap distribution
across 288 divergences: 219 at gap 1, 56 at gap 2, **4 at gap 3**, worth 8,476
tokens. Raising the slack recovers nothing. Dead.

**H-race: concurrent turns lose each other's writes.** Real, but not a defect
and not ours. Of 143 pairs where the previous turn's write was never read, 106
were requests that started before the previous one finished — 53.7% overlap
against a 0.8% base rate, a 67x enrichment. Those are parallel subagents under
one `conversation_key`; pairing them by time order is invalid, so most of that
apparent 304k loss is an artifact of the offline pairing, not a real loss.
**Any log query that pairs turns by conversation key and time order is wrong
wherever subagents run.** Pair by stream.

**H-retry: the losses follow dropped streams.** Of 12 equal-length in-place
divergences, 0 had any retry, overload, error or stream-drop event on the
previous turn. Dead.

### What the divergences actually are

`prefix_replay_not_replayed` already carries `diff_shape_stored` /
`diff_shape_current`, which went unread until now. Across 319 diverged turns:

- 116 — blocks **appended** to an existing message, mostly
  `tool_result` -> `tool_result,text`. The client attaching a
  `<system-reminder>` to a message it already sent. Client behaviour.
- 76 — same shape, content edited inside.
- 59 — stored says `string`, current says `thinking,tool_use`: **different
  speakers at the same index**, so the comparison was against another stream
  entirely. 44 of these are `chain_id == 0`.
- 8 — blocks removed.

60.7% of all divergences sit at exactly `len-2`, and 100% of the equal-length
ones do. `len-2` is the assistant message; `len-1` is the user tool_result.

### The open question, and the instrument for it

`chain_id == 0` from `previous_turn_for` is deliberate and correct: it means no
held stream leads this turn, so the store refuses to splice rather than merge
two unrelated runs (`prefix_replay.rs:2544-2552`). The question is why a
*continuing* stream finds nothing held. Three causes need opposite answers — a
genuinely new stream (nothing to do), an entry evicted under
`MAX_ALTERNATE_PREFIXES` (16) or `MAX_ALTERNATE_MESSAGES` (4,000), or a real
divergence after a long agreement.

Eviction is counted only in `proxy_cache_replay_alternates_evicted_total`,
which is lazily registered and absent from `/metrics` on a fresh process, so it
could not answer this retroactively.

Added `prefix_replay_no_stream_leads_turn` on that arm: `alternates_held`,
`held_messages`, `primary_prefix_msgs`, `current_msgs`, `best_agreement_msgs`
and both caps. `best_agreement_msgs` is the discriminator — 0 means eviction or
a brand-new stream, a long run means identity was nearly there. This needs the
new binary running; the 08-22 restart predates it.
