# Idea: settle whether prior-turn thinking in the prefix is real money

- **Status:** rejected 2026-09-17 — measured and declined. Removing 4.5MB
  of thinking across 67 disciplined turns moved billed reads by ~0 (63
  exactly 0). The wider drop saves nothing; numbers that killed it: 67/67
  invariant through α=20.
- **Source:** `docs/notes/savings-ideas-1.md` ("measured but not recommended"),
  `docs/notes/savings-ideas-2.md` §4.2
- **Summary:** prior-turn thinking is 31–35% of prefix bytes.
  `compression/prior_thinking.rs` already drops it, but only at rebuild
  boundaries (205 events, 8.2 MB over four days) — and whether Anthropic bills
  those blocks at all is unknown. The output-split instrumentation
  (`output-block-type-instrumentation.md`) supplies the data: join the new
  fields with `prefix_composition` to test if prior thinking bills as cache read.
- **Value:** decides whether a wider thinking-drop is worth its cache risk.
- **Next:** run the join once the instrumentation has 2–3 days of data; see
  also `gate-prior-thinking-drop.md` for the gate fix this unblocks (already
  shipped, in `implemented/`).

## Findings 2026-09-17 (5 log days: 09-11, 09-14/15/16/17; 517 drops, ~29.6k ledgers)

Test (not the planned output-split join — a stronger instrument): the drop
itself removes measured thinking bytes from the wire
(`prior_thinking_dropped.bytes_removed`). On read-dominant drop turns with a
previous turn on the same conversation, compare billed `cache_read` against
the previous boundary. If thinking bills at face value, removing 30–88Kt of
it must show up as shortfall or a bust line.

- **67 disciplined hits: removing thinking moves billed reads by ~0.**
  Filters: read-dominant, prev turn kept thinking (prev not a drop),
  removal necessarily reached the cached region (`bytes_removed/4 >
  input_tokens`; tails are ~2 tokens), same-stream discipline (candidate
  msgs ≥ prev msgs, no unmatched/forgotten/recache/TTL lines). 63/67
  shortfall exactly 0, all 67 < 1000 tokens, while removed spans
  8KB–353KB (median 25KB; 4.5MB / 2,623 blocks total). Holds at every
  bytes-per-token assumption through α=20 (67/67).
- **First cut looked contradictory — both halves explained.** Unfiltered,
  98/195 clean-subset turns showed huge shortfalls at read/exp ≈ 1/3, 1/2:
  all carry independent bust/branch attributions (`prefix_head_changed`,
  `prefix_content_diverged`, `inbound_tail_replaced`) — the drop co-fired,
  it didn't cause. The single remaining 137Kt case is a join artifact:
  two interleaved streams sharing one key (reads alternating ~60Kt/~120Kt;
  observer matched correctly, no line filed).
- **Verdict: the 31–35% share is $0 at read rates.** Removing thinking from
  the wire never reduces the billed read, so widening the drop cannot save
  read money — while bust risk outside the gate's contexts is unproven
  (these 68 removals filed zero busts, but all fired under gate-approved
  contexts). Current boundary-only behavior is correct; do not widen.
- **Caveats.** (1) Bundle: 480/517 drops co-fire with history offload, so
  strictly the invariance is to the removal bundle; no
  rebuild-free/offload-free read data exists to isolate thinking's marginal
  share (only 37 offload-free drops, all rebuilds). Nothing in the data
  motivates a trial to split them — no prize either way. (2) Whether
  thinking is unbilled vs billed-but-skipped-in-matching is moot for the
  decision: both imply $0 savings from wider dropping. (3) A paired-α
  corroboration (drop vs non-drop rebuilds, n=10) came back 1.01 (equal)
  but content variance (α 0.55–2.3) leaves it underpowered; the conclusion
  rests on the removal test, not this.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

**Prior-turn thinking in the prefix: 31-35% of bytes.** `compression/prior_thinking.rs`
already drops it, but only at rebuild boundaries to avoid busting the cache
(205 events, 8.2MB removed over four days). Whether Anthropic bills those
blocks at all is open; plan 2 supplies the data.
