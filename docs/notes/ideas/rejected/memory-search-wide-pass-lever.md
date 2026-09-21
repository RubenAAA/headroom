# Idea: rewrite the memory-search plan from live numbers

- **Status:** REJECTED 2026-09-21. The gate passes by 6.6× with nothing done:
  memory-on `pre_forward` p50 is **60.3 ms** against the 400 ms bar, and the
  `memory` stage itself is **0.17 ms** — 0.29% of pre-forward. Measured over
  23,779 `stage_timings` rows, Anthropic models only, 2026-09-15 to 09-21.
  Plan 1 has nothing to win. See Verdict at the bottom.
- **Source:** `docs/speed-ideas.md` §0.1 + §1–2 (2026-09-03 profile; line numbers stale)
- **Summary:** memory search is 93% of pre-forward (p50 1.5 s vs 65 ms tool-mode).
  But live `memory_search_timings` invert the §2 assumptions: porter 70 ms
  dominates trigram 28 ms (not 85% trigram), real queries run 16 terms (not the
  150-term cap), and the wide pass is 79% of the search while narrow already
  returned 40 hits. So 1b (trigram-as-fallback) is backwards, 1a (term cap 32)
  aims at nothing, **1c (fewer wide passes — raise narrow to `top_k*16`) is the
  lever**. Caveat: 3 samples on a quiet box; `total_ms` 108 vs section-1 p50
  1,539 suggests load matters — read a real sample joined on `inflight` first.
- **Next:** read live timings joined on `inflight`; implement 1c; gate for the
  whole plan stays: memory-on pre_forward p50 < 400 ms over a day, revert any
  single change moving p50 < 100 ms. 1d (off-critical-path) only if 1a–1c fail.


## Detail

*moved from `docs/notes/speed-ideas.md`*

Plan 0 and plan 3 shipped and are live. Three things to know before doing
anything else.


## Detail

*moved from `docs/notes/speed-ideas.md`*

**1. Rewrite plan 1 before you act on it. The live numbers invert it.**
Plan 0's `memory_search_timings` event contradicts three of the assumptions
section 2 rests on. First samples off the deployed build:

| | what section 2 assumed | first live sample |
|---|---|---|
| trigram share | ~85% of query time | 28.3 ms of 98.5 ms — porter is 70.2 ms |
| query length | runs to the 150-term cap | `term_count: 16` |
| where the time goes | the FTS queries | `wide_ms` 85.8 vs `narrow_ms` 22.2 |

So **1b is backwards** — making trigram a fallback leaves the larger half
alone, because porter dominates live. **1a is aimed at nothing** — at 16
terms a cap of 32 changes nothing. **1c is the lever**: the wide pass is 79%
of the search and it ran even though narrow returned 40 hits. The trigram
figure in section 2 came from an offline Python harness against a copy of
the index; live it inverts.

Two caveats. That is three samples on a quiet proxy (`inflight` p50 2), and
`total_ms` reads 108 ms against the 1,539 ms p50 in section 1 — so either
load explains the difference or the memory stage counts work outside the
search. `queue_ms` and `reader_ms` both read ~0 here, so the blocking-pool
wait and connection acquisition are not the gap on an idle box. Read a real
sample, joined on `inflight`, before touching the search.


## §0b status

*moved from `docs/notes/speed-ideas.md`*

**Plan 1: unshipped, and the §0 inversion table is now stale — do not
act on §2 or on the §0 table without re-reading this.** Fresh
`memory_search_timings`, n=24: trigram is the majority again (mean 76.5
ms vs porter 55.4, ~58% of the pair — §2's 85% is closer than §0's
porter-dominates), but `term_count` maxes at 11 (the 150 cap still never
binds, so **1a is dead**), and narrow now dominates wide (mean 111.4 vs
41.6 ms). Wide still runs 24/24 despite full narrow pages — expected:
the gate reads the post-filter count (`ctx_backend.rs:546`) while
`hits_narrow` records pre-filter index hits (`:332`), and partition
filtering drops most hits after ranking (`:549-554`). So the lever is
the wide-always-runs gate and narrow cost, not 1b/1c as written:
**1b needs re-costing** (trigram 58% — a fallback still saves the
majority but keeps the Russian/substring recall risk) and **1c is
reshaped** (gate on post-filter sufficiency, not index hits). n=24 is
thin; re-run this table over a busier day before rewriting Plan 1 for
real. Markers of unshipped: cap still 150 (`store.rs:1203`), trigram
unconditional (`store.rs:795-801`), narrow still `top_k*4`
(`ctx_backend.rs:544`).


## §0c status (2026-09-11)

Busy-day re-read, n=869 `stage_timings` (inflight p50 2, max 8 — still
light concurrency): `memory` stage p50 **0.11 ms**, p90 0.38 ms against
`pre_forward` p50 44.9 / p90 193 ms. Memory is 0.2% of the residual —
Plan 1 (all of 1a–1d, including the reshaped wide-gate) is gated OFF by
its own economics until `memory` p50 matters again. Do not implement;
re-check this number (not the inversion table) if load returns. The
residual to chase is elsewhere — see Plan 2 step-1 gate read.


## Plan framing

*moved from `docs/notes/speed-ideas.md`*

Each plan has a check that says whether it worked and a gate that says
whether it was worth doing. Baselines are the table above.

Order: 0, 3, 1a/1b/1c, 2 step 1, 2 step 2 only if step 1 says so, 1d last.


## Detail

*moved from `docs/notes/speed-ideas.md`*

### Plan 1 — memory search: 1,539 ms p50 → under 300 ms

**1a. Term cap 150 → 32, rarest first.** In `sanitize_query` /
`sanitize_trigram_query`, keep the N terms with the lowest document
frequency after stopword removal. Add a `df` column to `vocabulary`,
filled in `index_content`, or derive it once per search from
`chunks_docsize`. Basis (measured): trigram time is linear in terms, so 32
terms should take the 8 queries from ~980 ms to ~250 ms.
Check: bench before/after; and a recall replay — dump 200 real queries for
an hour (a debug event carrying the query text), run top-10 under 150 and
under 32 terms, accept if the median Jaccard of the top-10 id sets is
≥ 0.8. If it is not, try 64.

**1b. Trigram only as a fallback.** In `rrf_search`, run porter first; run
trigram only when porter returned fewer than `limit` hits or the query
holds non-ASCII letters. Basis: trigram is ~85% of query time (172 vs
19 ms at 100 terms). Same checks as 1a. Trigram is what finds inflected
Russian and substrings; the non-ASCII guard keeps that.

**1c. Fewer wide passes.** `search_memories_sync` already returns early when
narrow fills `top_k`. Use plan 0 to measure how often the wide pass runs and
what it costs. If it runs on more than 30% of requests, raise `narrow` from
`top_k*4` to `top_k*16`: a bigger `LIMIT` on one query is nearly free
(porter/4000 cost the same as porter/20 in the timing run) and a second
query is not.
Check: wide-pass rate and ms from `memory_search_timings`, before/after.

**1d. Search off the critical path.** Memory only appends to the last user
text block (`append_to_latest_user_tail`) and reads nothing compression
writes. In `proxy.rs`, spawn the search as a tokio task once `value` is
parsed (before the CCR/inject block near 3520), await the `JoinHandle` at
the current memory site (3881) and apply the append there. The memory
stage then measures the wait, not the search.
Check: memory stage (the wait) against the search's own duration from
plan 0. On its own it hides the search only behind compression, which is
15 ms p50, so it saves about 15 ms. It matters only if 1a–1c fail to land.
Do those first.

Gate for plan 1 as a whole: live pre_forward p50 for memory-on requests
under 400 ms over a day. Any single change that moves memory p50 by less
than 100 ms gets reverted.

## Verdict 2026-09-21 — rejected, with the killing number

The gate asked for memory-on `pre_forward` p50 under 400 ms over a day. Read
over six days instead: 23,779 `stage_timings` rows joined to Anthropic-model
requests by `request_id` (2026-09-15 to 09-21).

| stage | p50 | p90 | p99 |
|---|---|---|---|
| upstream | 1,480.79 | 2,575.74 | 5,068.05 |
| **pre_forward** | **60.29** | 166.11 | 447.75 |
| parse | 17.06 | 38.19 | 299.35 |
| compression | 16.89 | 68.23 | 160.66 |
| post | 8.79 | 19.03 | 42.96 |
| replay | 4.57 | 12.78 | 33.39 |
| rewrite | 3.78 | 8.26 | 20.87 |
| **memory** | **0.17** | 0.43 | 1.13 |

Memory is **0.29% of pre-forward at p50**. The 1,539 ms figure in the Summary
above, and the 93%-of-pre-forward claim built on it, are artifacts of the
2026-09-03 profile. Nothing in plan 1 — 1a, 1b, 1c or 1d — can move a number
that is already a sixth of a millisecond.

The load caveat is settled too, which is what the file was waiting on. Split
by `inflight` (p50 2, p90 6, max 29):

| inflight | n | pre_forward p50 | memory p50 |
|---|---|---|---|
| 0-2 | 13,023 | 62.0 | 0.175 |
| 3-5 | 7,279 | 59.9 | 0.176 |
| 6-50 | 3,477 | 56.4 | 0.166 |

Flat, and slightly *faster* under load. The quiet-box worry does not hold.
This confirms the busy-day re-read recorded above (n=869, memory p50 0.11 ms)
at 27× the sample and with real concurrency.

If `memory` p50 ever reaches single-digit milliseconds, reopen from the
numbers here, not from `docs/speed-ideas.md` §1-2. Until then the pre-forward
budget is `parse` and `compression`, and all of it is noise beside a 1.48 s
upstream.
