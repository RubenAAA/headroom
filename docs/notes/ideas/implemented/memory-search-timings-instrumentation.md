# Implemented: memory_search_timings instrumentation (plan 0)

- **Status:** implemented and live (2026-09-04)
- **Source:** `docs/speed-ideas.md` plan 0
- **Summary:** `memory_search_timings` event (narrow/wide pass, porter/trigram,
  rerank, record loads, `term_count`, hits) + `inflight` on `stage_timings` +
  `benches/memory_search.rs`. Cost: a few `Instant::now` per request.
- **Note:** first live samples inverted the §2 assumptions (porter dominates,
  16 terms, wide pass 79%) — see `../memory-search-wide-pass-lever.md`.


## Detail

*moved from `docs/notes/speed-ideas.md`*

**Plan 0 + Plan 3: confirmed shipped.** `memory_search_timings` with the
full breakdown (`ctx_backend.rs:584-606`, timings struct in
`store.rs:178-184`), `inflight` on `stage_timings` (`stage_timer.rs:97,142`,
passed at `proxy.rs:7227`), bench at
`crates/headroom-core/benches/memory_search.rs` (20/50/150 terms,
narrow 20 / wide 2000). Footprint spawns off-path and times only the
spawn (`proxy.rs:5653-5663`), with a spawned-vs-inline parity test
(`:12020`).


## Detail

*moved from `docs/notes/speed-ideas.md`*

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
