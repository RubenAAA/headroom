# Idea: settle whether prior-turn thinking in the prefix is real money

- **Status:** open question (blocked on measurement)
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
  also `gate-prior-thinking-drop.md` for the gate fix this unblocks.


## Detail

*moved from `docs/notes/savings-ideas-1.md`*

**Prior-turn thinking in the prefix: 31-35% of bytes.** `compression/prior_thinking.rs`
already drops it, but only at rebuild boundaries to avoid busting the cache
(205 events, 8.2MB removed over four days). Whether Anthropic bills those
blocks at all is open; plan 2 supplies the data.
