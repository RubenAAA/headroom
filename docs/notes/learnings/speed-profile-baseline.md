# Learning: proxy latency profile baseline (2026-09-03)

- **Source:** `docs/notes/speed-ideas.md` §1 (2,875 stage_timings, /v1/messages; `proxy.rs` numbers predate ~14k growth)
- **Claim:** memory search 93% of pre-forward (p50 1.5 s vs 65 ms tool-mode); compression 14.6 ms p50; footprint 14 ms median with 63 s tail; residual 42/123 ms; pre_forward-minus-memory ≈25 ms/100 kB.


## Detail

*moved from `docs/notes/speed-ideas.md`*

## 1. The profile

The 11:26Z build (n=290) ran the pre-`ee3e07a8` memory search (memory p50
91 s, 278/290 requests over 30 s). It is excluded below; the numbers are for
builds from 12:09Z on.

Memory on (n=2,014):

| stage        |    p50 |    p90 |     p99 |      max | ms per 1000 req | share of pre_forward |
|--------------|-------:|-------:|--------:|---------:|----------------:|---------------------:|
| buffer       |    0.0 |    0.0 |     0.0 |      3.5 |               8 |                 0.0% |
| memory       | 1538.5 | 3754.6 |  9689.0 | 230702.4 |       2,285,006 |                93.0% |
| compression  |   14.6 |   64.8 |   166.4 |    343.5 |          26,713 |                 1.1% |
| replay       |    4.9 |   17.4 |    45.9 |    305.9 |           8,110 |                 0.3% |
| footprint    |   14.4 |   33.2 |    92.2 |  63091.0 |          50,829 |                 2.1% |
| residual     |   42.4 |  123.2 |   437.4 |  19500.2 |          86,042 |                 3.5% |
| pre_forward  | 1615.4 | 3902.0 | 10426.3 | 299575.3 |       2,456,708 |                      |
| upstream     | 1434.8 | 2674.6 |  9978.0 |  51888.6 |       1,915,314 |                      |

`residual` = pre_forward minus the five timed stages: proxy work no stage
covers.

Memory in tool mode (n=808): pre_forward p50 65.5 ms, p90 207 ms.

So: memory search turns a 65 ms proxy into a 1.6 s one, and the proxy then
takes longer than Anthropic does (upstream p50 1.43 s). Buffer and replay are
noise. Compression is real but small: 14.6 ms p50, 14 ms per 100 kB
(R² 0.38). Footprint is 14 ms at the median with a tail in the tens of
seconds.

### Why memory costs 1.5 s

Measured against the live index
(`~/.claude-personal/context-mode/memory/memories_index.db`: 4,125 chunks,
28k vocabulary words, 552 sources), read-only, from Python:

- The query is the whole last user text block (`extract_user_query`), cut to
  150 OR-joined terms by `sanitize_query` / `sanitize_trigram_query`
  (`store.rs`, `MAX_QUERY_TERMS`).
- One search runs porter + trigram FTS (`rrf_search`), narrow
  (`top_k*4`) and then wide (`WIDE_SEARCH_LIMIT = 2000`) when narrow comes
  up short, and both again on the English rendering when the query holds
  Cyrillic. That is up to 8 FTS queries per request.
- 8 queries at 150 terms: 983 ms in one process; 1.7 s each with 12 in
  parallel, so the pager no longer serialises.
- Trigram is ~85% of it and linear in term count: per query 7 ms at 5 terms,
  39 ms at 20, 79 at 50, 172 at 100, 276 at 200. Porter: 2, 5, 10, 19, 37 ms.

The rest of the 1.5 s is inferred, not measured: per-hit `records.get()`
under the `MemoryRecordStore` mutex, the 300-hit proximity rerank,
`related_to`.

### The tail

All 20 slowest post-fix requests by pre_forward are memory. Two clusters:

- 16:50–16:54Z: four requests at 73–231 s with 0–2 in flight. The worst
  (`e11b716c`) also had footprint 63 s and residual 5.6 s, and heartbeats
  kept logging, so the process was starved of disk or CPU, not stuck in the
  query. A cargo build was likely running (binary rebuilt 16:10Z, commits
  15:42–17:30Z). Inferred; the log cannot prove it.
- Bursts at 12:53Z and 15:42:22Z (five requests in one second) at 10–34 s.
  Concurrency: memory p50 is 1.7 s at ≤2 in flight, 2.0 s at 4–5, 5.6 s at
  6–7.

No image, offload, or replay-miss pattern. One unexplained residual outlier:
`8a04cdce` 12:55Z, residual 14.7 s, memory 0.5 s, `no_previous_turn`.

### Size

pre_forward is not linear in body size because memory dominates and memory
is not size-driven (memory p50: 1.8 s at 100–200 kB, 0.7 s at 500–600 kB;
the big bodies here are tool-mode sessions). pre_forward minus memory is
roughly linear, about 25 ms per 100 kB:

| body kB | n    | pre_forward − memory p50 / p90 |
|--------:|-----:|-------------------------------:|
| 0–100   |   91 |  26 / 118 |
| 100–200 | 1269 |  50 / 136 |
| 200–300 |  720 |  78 / 221 |
| 300–400 |  273 | 104 / 288 |
| 400–500 |  251 | 130 / 275 |
| 500–600 |  140 | 152 / 482 |

Fits: compression 14 ms/100 kB (R² 0.38), replay 2.6, residual 6–10 by bin
(OLS R² ≈ 0 because of outliers).

### Where the residual lives

`crates/headroom-proxy/src/proxy.rs`, `forward_http`. Gaps between timed
stages, with what sits in them:

| gap | lines | contents | log evidence |
|-----|-------|----------|--------------|
| buffer → memory | 2925–3881 | `serde_json::from_slice` on the body at 2964, 3086, 3275, 3413, 3487, 3520; inbound drift hash; capture; sticky betas; prior-thinking drop; CCR proactive expansion; ctx_inject; memory tool injection | `memory_tools_injected` lands ~1 ms after the first log line |
| memory → compression | 3976–4065 | `to_vec` of the body when memory changed it (3979) | — |
| compression → replay | 4338–4517 | outcome bookkeeping, `hold_working_directory` rewrite (4479), `drop_unsigned_reasoning_blocks` (4505) | — |
| replay → footprint | 4539–4684 | `model_router::apply_to_anthropic_body` (4595), `maybe_prune_tools` (4608), `maybe_optimize_images` (4611); each parses and re-serialises | live-zone dispatch → pruned tools: 10 ms p50, 31 p90 |
| footprint → pre_forward | 4700–4947 | `maybe_capture_outbound`, `apply_request_hooks` (4737), signed-reasoning check, outbound drift `from_slice` (4833), `cache_key_fingerprint` (4863), another `from_slice` (4898) | pruned tools → outbound_body_bytes: 20 ms p50, 45 p90 |
| pre_forward → upstream | 4947– | reqwest build | total − (pre_forward + upstream) = 1.8 ms p50 |

The last-but-one row is the largest and holds most of the residual.


## Plan 4 + method

*moved from `docs/notes/speed-ideas.md`*

### Plan 4 — compression: no change

14.6 ms p50, 14 ms per 100 kB, and it pays for itself. Revisit only if
plan 2's `rewrite` stage shows the post-compression re-serialisations
dominate.

## 3. Measured vs inferred

Measured: every stage percentile above; FTS per-query cost by term count;
the 8-queries-per-search count; the concurrency table; the milestone
deltas between log lines.

Inferred: the savings from 32 terms and trigram-as-fallback (extrapolated
from the linear FTS timings); the recall cost of either (unmeasured until
1a's replay); the residual's attribution to parsing (code reading plus the
size slope, hence plan 2 step 1); the fsync attribution for the footprint
tail; the cargo-build explanation for the 16:50Z stall.
