# Idea: parse the body once (measure first)

- **Status:** open, gated on measurement
- **Source:** `docs/speed-ideas.md` plan 2 (2026-09-03; line numbers stale)
- **Summary:** residual proxy work is 42 ms p50 / 123 p90; attribution to
  repeated full-body parses is inferred from code + size slope, not measured.
  Step 1 (measure): add `parse` / `rewrite` / `post` stages to `StageTimer`
  around buffer→memory, router/prune/image, and footprint→pre_forward gaps.
  Gate: continue only if one gap is ≥ 20 ms p50. Step 2 (fix per gap): hand
  one parsed `Value` through consumers, single serialisation at stage ends;
  byte-identity check via capture replay. Skip the whole thing if step 1
  finds nothing. 1–2 days.
- **Next:** land step 1, read one day of log.


## §0b status

*moved from `docs/notes/speed-ideas.md`*

**Plan 2: step 1 shipped 2026-09-11** (`parse`/`rewrite`/`post` records in
`forward_http` + expected-stages list; `wiki/ARCHITECTURE.md` stage chain
updated). Emission validated by smoke test, then instrumented release built
(`make build-proxy`) and deployed via `restart-headroom.sh` ~14:48 UTC —
first production `stage_timings` already carries all three stages numeric.
Gate readable after one day of log (~2026-09-12): apply the ≥20 ms p50 gate
per gap, then rewrite or retire step 2. Residual p50 is 49.4 ms / p90 105.8 ms — above
the 20 ms aggregate — but unattributable to any gap, so no gap passes the
gate as specified. Read one day of log with the new stages, then apply the
≥20 ms p50 gate before any rewrite work. Note
the window moved: memory p50 is 0.30 ms now, compression (22.8 ms) is
the largest timed stage.


## Detail

*moved from `docs/notes/speed-ideas.md`*

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


## Latency split

*moved from `docs/notes/proxy-experiments-2026-08.md`*

Where the proxy's own latency goes:

- **About 61 ms fixed plus 0.155 ms/KB, and 89% of it is proxy CPU.** Measured
  with captured bodies replayed against an instant local upstream, so the number
  is the proxy's own cost and not upstream inference: 156 ms at 198 KB and at
  445 KB, 171 ms at 674 KB, 239 ms at 858 KB, and 184 ms of CPU against 207 ms of
  wall over 20 requests. On the median 410 KB body that is roughly 125 ms, about
  6% of a 2 s turn. Phase timers on a 743 KB body, no profiler being available
  here: ident 1.8 ms, semantic cache 1.0 ms, compression decision 2.5 ms,
  compression 39.8 ms, outcome-context 7.5 ms, replay 2.0 ms, and 108.6 ms total
  to the send point. Compression is the proxy earning its keep.
- **The 54.3 ms once attributed to the tool stages was the savings tracker.**
  That timer window spanned `record_request_footprint`, which runs at the end of
  it; split, the four tool stages account for 3.9 ms and the footprint call for
  49.2 ms. `record_request_footprint` calls `record_proxy_overhead` and
  `record_tools`, each of which calls `SavingsTracker::save`, and `save` rebuilt
  every `history` entry into a `Value` before serialising — 1.14 MB of a 1.4 MB
  payload, several times per request, since a typical request reaches four or
  five recorders. Measured in release against the real savings file,
  `record_proxy_overhead` went 12.40 ms to 1.16 ms, `record_tools` 11.96 ms to
  1.33 ms, and `record_request` 16.13 ms to 2.22 ms. History was the whole of it:
  emptied, the same pair of calls costs 0.62 ms instead of 24.36 ms. Fixed in
  `crates/headroom-core/src/savings_tracker.rs`, where `State` now carries
  `history_rendered` kept in step by `push_history` and `trim_history`, `save`
  serialises from a borrowed struct, and `trim_history` retains and drains in
  place instead of cloning every surviving entry on every push. The file written
  is unchanged.
