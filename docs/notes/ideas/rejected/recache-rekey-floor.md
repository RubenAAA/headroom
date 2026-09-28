# Idea: size the hidden re-key / first-turn floor

- **Status:** rejected 2026-09-28 — the 3.37M "floor" below was the
  criterion, not re-keys. 152 of the 186 `fresh_session` contradictions read
  a prefix size that another session also read (3,470 tokens 21 times, 7,236
  13 times): new sessions hitting the shared system and tools prefix, which is
  the cache working. What is left over ten days is about 0.5M tokens. See the
  2026-09-28 findings at the bottom.
- **Source:** a continuation under a fresh key (compaction, model switch,
  system rewrite) files as `FirstTurn`, never reaches attribution, and its
  write is not waste-counted. `first_turn_contradictions_total` and
  `forgotten_conversations_total` bound this from below but nobody has sized
  it against recache waste. The 2026-09-17 completion log now parks the
  session hash beside every key, which is the missing join key.
- **Value:** the recache waste figure has an unmeasured floor. If large, the
  fix is key stability, not cache stability — a different project.
- **Next:** join `first_turn_write_observed` /
  `first_turn_prefix_diagnostic` to recently-completed sibling keys on
  `session_key_hash`; total `arrived_with_history`-with-no-read plus forgotten
  returns over the same window as recache waste. Small ⇒ close this; material
  ⇒ promote to a key-stability proposal.
- **Update 2026-09-17:** measured over the live 08:04–13:24Z window —
  drift waste 69,650t (8 events) vs 14 contradictions at 223,927t (3.2×),
  arrived-with-history-no-read 0t on thin D0 coverage; 60s session-hash join
  found zero siblings for all 14 (join works — 3 multi-key sessions linked —
  but blind to session-rotating rekeys by design). Join gaps closed in code:
  `turn_cost_ledger` now carries `session_key_hash`,
  `cache_conversation_forgotten` logs `evicted_footprint_tokens` + current
  write/read, `cache_stream_unmatched` carries `session_key_hash`
  (`usage_observer.rs`). Still open: re-run the join once new fields land,
  then promote-or-close on the benign-scaffold share.

## Findings 2026-09-21 — the floor is material; promote

Re-ran the join over the full 6-day window (2026-09-15 to 09-21), Anthropic
models only: 364 `first_turn_write_observed` events, 349 conversations.

| reason | contradicts | turns | creation tokens |
|---|---|---|---|
| fresh_session | **yes** | 176 | 3,021,964 |
| compaction_restart | no | 88 | 2,765,846 |
| fresh_session | no | 59 | 1,625,549 |
| arrived_with_history | no | 21 | 472,283 |
| session_key_drift | no | 16 | 591,969 |
| arrived_with_history | **yes** | 4 | 347,726 |

**Contradictions: 180 turns, 3,369,690 creation tokens.** Against
drift-attributed recache waste over the same window (166 events, 505,961
tokens) that is **6.7×** — up from the 3.2× the 5-hour window showed. It is
also 0.89× of *all* recache waste (3,776,205), so the unmeasured floor is the
same order of magnitude as the thing the recache work has been chasing, and
none of it is counted.

The 2026-09-17 reading's "small ⇒ close this" does not survive. Promote to a
key-stability proposal. 176 of the 180 contradictions file as
`fresh_session`, meaning the key claims a cold start while the write says the
prefix was already there.

Two sub-answers the join settles:

- `arrived_with_history` with zero read: 4 turns, 347,726 tokens. Small.
- **Forgotten conversations: zero.** `cache_conversation_forgotten` exists in
  `usage_observer.rs:3550` and never fired once in six days, so
  `evicted_footprint_tokens` bounds that half of the floor at 0. The new
  fields landed; only one of them had anything to say.

First turns are 19.6% of all cache creation in the window (9,158,609 tokens
over 349 turns), which is the ceiling this floor sits under.


## Findings 2026-09-28 — the contradictions are shared-prefix reads; close

Same join over 2026-09-18 to 09-28, Anthropic models only. The criterion at
`usage_observer.rs:2499` calls a `fresh_session` turn a contradiction when it
reads anything. But a new session sends the same system prompt and tools as
every other session of its kind, and the provider serves that prefix from any
of them. A read there proves nothing about a lost key.

| reason | contradicts | msgs | turns | creation tokens | read p50 |
|---|---|---|---|---|---|
| fresh_session | yes | ≤2 | 186 | 2,949,537 | 17,426 |
| compaction_restart | no | 3–10 | 70 | 2,137,801 | 11,941 |
| compaction_restart | no | >10 | 41 | 1,826,387 | 105,085 |
| fresh_session | no | ≤2 | 63 | 1,778,734 | 0 |
| session_key_drift | no | >10 | 27 | 955,855 | 88,042 |
| arrived_with_history | no | >10 | 15 | 642,086 | — |
| arrived_with_history | yes | >10 | 3 | 319,354 | 0 |

The 186 fall across 185 distinct sessions, and none had replay or adoption.
152 read a token count that at least one other session read too, the mark of
a fixed shared prefix. Only 7 read more than 40k, which would be too much for
scaffolding alone; they wrote 205,194 tokens.

What could be a hidden re-key:

- `arrived_with_history` with no read: 3 turns, 319,354 tokens.
- `fresh_session` reads over 40k: 7 turns, 205,194 tokens.

That is about 0.52M over ten days. `session_key_drift` (956k) is not hidden:
prefix adoption found the donor, so replay already handles it, and what it
still writes belongs to the drift work. `compaction_restart` is a real new
prefix by design.

A key-stability project would chase half a million tokens in ten days. Not
worth it. If this comes back, tighten the criterion first so the counter stops
reporting shared-prefix reads as contradictions, for example by only counting
a `fresh_session` read above the largest prefix seen shared across sessions.
