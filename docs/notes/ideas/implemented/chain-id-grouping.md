# Implemented: chain-id continuity grouping

- **Status:** shipped `e22c1152` (2026-08-09)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §26


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 26 — Chain id: a grouping key that is not a guess

Added 2026-08-09, live in `e22c1152`.

Item 25's three failures all turned on the same missing thing: no way to tell
which turns belong to one unbroken run. `session_key` is per-client,
`conversation_key` hashes `system` plus the first message, and message counts
are ambiguous — a compaction, a retry and a genuine second stream all make the
count stop rising.

The replay store already computes the answer, because it has to decide what to
replay. A *chain* is a run of turns that each continue the previous one. Each
stored prefix now carries an id, assigned when a chain is born and inherited by
every turn that continues it. `previous_turn_for` returns it and both
`prefix_replay_applied` and `prefix_replay_not_replayed` log it as `chain_id`.

`chain_id = 0` means this turn continues nothing held — a first turn, a TTL
eviction, or a branch. It is deliberately not the id of the prefix the fallback
hands back, because naming two unrelated runs the same thing is the bug this
exists to remove.

Pinned by `interleaved_streams_get_distinct_chain_ids` (two streams get two ids,
each surviving growth) and `a_turn_continuing_nothing_reports_no_chain`.

### What it can now answer

- Of the 84% of re-cache waste sitting on multi-sequence keys, how much is
  concurrent streams and how much is one stream compacting or branching. Two
  live chains on one key means concurrency; a chain ending and a new one
  starting on the same key means a branch or compaction.
- Whether the gaps used to rule out TTL expiry (items 21, 22) were measured
  between turns of the same run, or across two runs — the earlier figures
  grouped by `conversation_key` and cannot tell.

Nothing consumes it yet. It is a measurement first; migrating the drift
detector, savings attribution or ctx-inject onto it is a separate decision and
should wait until the data says the id behaves.
