# Proxy latency: what costs, what to do, how to know it worked

Measured on the live log (`~/headroom-proxy.log` + `.log.1`, 2026-09-03
11:20Z–17:35Z, 2,875 `stage_timings` lines on `/v1/messages`, all joined to
`outbound_body_bytes`). Scripts: `/tmp/lat_profile.py`, `/tmp/final.py`,
`/tmp/mem_trace.py`, `/tmp/gaps.py`, `/tmp/fts_time.py`.

> Note (2026-09-10): §0 is current; §§1–2 are the 2026-09-03 baseline
> and their `proxy.rs` line numbers predate growth to ~14k lines.
> Re-resolve cites before costing work.

## 0. Pick up here (2026-09-04)

Plan 0 and plan 3 shipped and are live. Three things to know before doing
anything else.

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

**2. The thinking-drop gate is one read away from being decidable.** See
`savings-ideas-2.md` section 4.2. `prior_thinking_dropped` now carries
`agreed_prefix_len` and `forwarded_agreement_len`. The first says 47 of 49
`history_rewritten` drops hit a head the client still agrees on, median 101
messages. That alone does not settle it: `drop_prior_thinking` is
deterministic, so re-stripping a head that was already forwarded stripped
reproduces the same bytes and costs nothing. `forwarded_agreement_len`
separates the two — high agreement means the head went out verbatim and
stripping now busts it; low means we are reproducing what is already cached.
Read that field before changing the gate.

**3. The next deploy tests the shutdown fix.** `main.rs` used to
`sleep(grace)` inside the shutdown future, so the timeout delayed the drain
instead of bounding it and then waited on in-flight connections forever.
Three deploys on 09-03 needed SIGKILL. It now returns on the signal and
bounds the drain with a `select!`. Watch whether the deploy script still
prints "still up after 40s, forcing"; `shutdown_drained` with a small
`drain_ms` is the pass, `shutdown_drain_timed_out` says it overran and
`inflight` says whether anything was actually still streaming.

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

## 2. Plans

Each plan has a check that says whether it worked and a gate that says
whether it was worth doing. Baselines are the table above.

Order: 0, 3, 1a/1b/1c, 2 step 1, 2 step 2 only if step 1 says so, 1d last.

### Plan 0 — instrument before optimising

Code: `crates/headroom-proxy/src/memory/ctx_backend.rs`
(`search_memories_sync`, `ranked_for_user`);
`crates/headroom-core/src/ctx/store.rs` (`search`, `rrf_search`).

1. Time inside `search_memories_sync`: narrow pass, wide pass, `related_to`,
   record loads (count and ms). Inside `store.search`: porter ms, trigram
   ms, rerank ms, fuzzy ms. Return them next to the results (a small struct,
   or a thread-local the handler drains) and log one
   `memory_search_timings` event with `request_id`, `term_count`,
   `query_chars`, `hits_narrow`, `hits_wide`.
2. Add `inflight` to the `stage_timings` line: a counter bumped at
   `forward_http` entry and dropped at exit.
3. Add `crates/headroom-core/benches/memory_search.rs` (criterion, next to
   `ccr_store.rs`). It copies the live `memories_index.db` into `target/`
   at start and runs `CtxStore::search` with 20-, 50- and 150-term queries
   drawn from the `vocabulary` table. Plans 1a–1c are judged on it.

Check: over one day of log the sub-stages sum to within 10% of the memory
stage. Cost is a handful of `Instant::now` calls per request; worth it by
definition.

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

### Plan 2 — parse the body once: residual 42 ms p50 / 123 p90 → ~15 ms

The 42 ms attribution to parsing is inferred from the code and the size
slope, so this plan starts by measuring.

**Step 1 (measure).** Add three stages to the `StageTimer` in
`forward_http`: `parse` from buffer end (2925) to memory start (3881);
`rewrite` around `apply_to_anthropic_body`, `maybe_prune_tools`,
`maybe_optimize_images` (4595–4611); `post` from footprint end (4700) to
pre_forward (4947). One day of log says which gap holds the 42 ms.
Gate: continue only if one gap is ≥ 20 ms p50.

**Step 2 (fix, per gap that passed the gate).**
- parse gap: parse `buffered` once into a `serde_json::Value` right after
  the buffer stage and hand `&Value` to every consumer; re-serialise only
  when a consumer mutates (prior thinking, CCR expansion, ctx_inject,
  memory).
- rewrite gap: chain router, prune and image optimisation on one `Value`
  with one serialisation at the end.
- post gap: keep the `Value` the last mutating stage produced and pass it
  to `observe_outbound_drift` (4833) and the msgs extraction (4898). The
  fingerprint keeps hashing bytes.

Check: the three new stages before/after; existing tests unchanged;
`bytes_out` in `outbound_body_bytes` byte-identical on a replayed request
(capture one with `cache_stabilization::capture`, diff the outbound body).
Gate: residual p50 drops ≥ 20 ms, and the pre_forward-minus-memory slope
(size-bin table in `/tmp/final.py`) falls. One to two days; skip if step 1
finds nothing.

### Plan 3 — `record_request_footprint` off the request path: 14 ms p50, 63 s max → ~0

`proxy.rs:2363`. It parses `original` and `on_the_wire` in full (two
~200 kB parses), runs `audit_tool_pairing` and `tool_inventory_of`, then
the tracker persists with temp-file + fsync + rename
(`savings_tracker.rs:7`) under a std mutex. The 63 s outlier and the 0.8 s
p99 in tool mode are that fsync on a busy disk (inferred from the write
mode and the 16:50Z disk stall).

Change: clone the two `Bytes` (refcounted, cheap) and
`tokio::task::spawn_blocking` the whole function after the footprint
stage; time the spawn only. If plan 2 lands, pass the parsed `Value`s
instead. The tracker's mutex already orders the writes.

Check: footprint stage p50/p99/max over a day, expect under 1 ms; tracker
`history_response` totals (proxy overhead, tool counts) identical
before/after on a fixed replay of 50 requests.
Gate: 14 ms p50 and the tail for ~20 lines and no behaviour change. Do it
right after plan 0.

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
