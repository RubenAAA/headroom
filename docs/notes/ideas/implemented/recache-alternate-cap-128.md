# Implemented: replay alternates cap 16 → 128

- **Status:** fixed 2026-08-23 (pinned by
  `a_parent_conversation_outlives_a_large_fan_out`; fails at 16, passes at 64)
- **Source:** `docs/notes/recache-classification.md` (root cause 2026-08-23)
- **Summary:** `MAX_ALTERNATE_PREFIXES = 16` evicted the parent after 17
  subagent turns while holding 12% of the message budget — the count ceiling
  bit before the 4,000-message budget in every shape tested, and the evicted
  parent (most expensive prefix) re-cached everything next turn (51 turns /
  2.59M tokens, ~50k each). 128 is 4.4× the busiest session ever seen (max 29
  streams); memory still bounded by `MAX_ALTERNATE_MESSAGES`.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Root cause found — 2026-08-23

**`MAX_ALTERNATE_PREFIXES = 16` was the only bound that ever bit, and it threw
away the most expensive prefix in the store.**

Reproduced without traffic, in a unit test. A 300-message conversation, then
subagents at 30 messages each:

```
main=300 sub= 30: LOST after 17 subagent turns (held  480/4000, cap 16)
main=300 sub=100: LOST after 17 subagent turns (held 1600/4000, cap 16)
main=300 sub=250: LOST after 16 subagent turns (held 3750/4000, cap 16)
```

The parent is dropped after 17 subagent turns while the store holds 480
messages against a 4,000 budget — 12% of the bound the code calls "the bound
that actually matters". The count ceiling bit first in every shape tested.

Two things combine. Eviction takes the least-recently-*displaced* entry, on the
reasoning that it "has actually gone quiet"; a parent waiting on its fan-out
looks exactly like that. And the parent is the largest entry in the store, so
the cheapest thing to keep by count is the most expensive thing to lose by
tokens. Its next turn then finds no stream leading it, takes the `chain_id == 0`
path, and re-caches everything — the 51 turns and 2.59M tokens above, ~50k
each.

**Fix: raise the ceiling to 128** so the message budget governs, which is what
the design intended. Memory is unchanged: it is bounded by
`MAX_ALTERNATE_MESSAGES`, not by the count.

The number is sized against the data, not picked round. Distinct streams per
session across the same logs — `chain_id` increments once per new stream, so
its max per session measures exactly this:

| | streams |
|---|---|
| median | 0 |
| p90 / p95 / p99 | 4 / 6 / 11 |
| max observed | 29 |
| sessions over 16 | 2 of 344 (0.58%) |
| sessions over 32 | 0 |

128 is 4.4x the busiest session ever seen. Note that `alternates_held` in the
logs maxes at exactly 16 — the old ceiling clipping its own distribution, which
is why the count had to be measured through `chain_id` instead.

Subagent *turns* are not the quantity that matters: a stream taking many turns
is promoted back to primary on each one and consumes no extra slot. Only
distinct streams do.

Pinned by `a_parent_conversation_outlives_a_large_fan_out`. It fails at 16 and
passes at 64, so the defect cannot come back quietly.
