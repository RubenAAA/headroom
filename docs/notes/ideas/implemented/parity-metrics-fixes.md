# Implemented: metrics that measured the wrong thing

- **Status:** done 2026-08-07
- **Source:** `docs/notes/rust-parity-gaps.md` §7
- **Summary:** three fixes from one live `/stats` read: `token_savings_percent`
  30,891% → divide by `input` (pre-compression sum) with `attempted` fed from
  `OutcomeContext` at all five sites (68%); in-band SSE rate-limit/overload
  errors now retried (peek first event, `reason="in_band_sse"`); dispatch
  size-gate returns `NoOp` (99.6% of search rejections dropped nothing) with
  `declined_by` counted, not absorbed. Plus recurrence guard for the capture
  false alarm + log rotation in the launcher.
- **Not done deliberately:** whole-prompt denominator (balloons wrong),
  dropped-usage questions answered by test, not merge.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 7. Metrics that were measuring the wrong thing — DONE (2026-08-07)

Found by reading a live proxy's `/stats` and `/metrics` after ~9.5 hours.

**`token_savings_percent` read 30,891%.** It divided `saved` by
`attempted_input`, and every outcome site filled that field from the provider's
`usage.input_tokens`. On Anthropic that excludes cache reads and writes, so on
a warm session it collapsed to the uncached remainder — 8,059 against 2,489,559
saved — and held a value byte-identical to `uncached_input_tokens`, which was
the tell. Two fixes: the percentage now divides by `input` (the sum of
pre-compression sizes, matching `RequestOutcome::savings_pct`), and
`attempted_input_tokens` is fed from the compressible baseline via
`OutcomeContext::attempted` at all five outcome sites. The session reads 68%.

Note what was NOT done and why: making the field the *whole prompt*
(input + cache_read + cache_write) is the obvious reading and is wrong. It
would feed `original_tokens = attempted + saved` on non-compressed turns and
balloon `input` to cache-re-read scale, collapsing the figure to ~1.6% — a
denominator dominated by the same prefix counted once per turn.

**Retries could not see in-band SSE errors.** Anthropic reports rate limits and
overload inside a 200 body on streaming requests. Both retry loops branched on
HTTP status alone, so those turns looked like success. `forward_http` now peeks
the first SSE event and re-sends on a leading `overloaded_error`,
`rate_limit_error` or `api_error`, counted as
`proxy_upstream_retries_total{reason="in_band_sse"}`. Peeked bytes lead the
client's stream, so a clean turn is unchanged.

**Compressors ran and threw the result away.** Every arm of
`dispatch_compressor_uncached` asked "did this help?" as `compressed ==
original` — byte identity, which misses a compressor that *rewrote* a block
without removing anything. `SearchCompressor` does this whenever its caps do
not bite. Measured over 862 captured requests: 99.6% of its token-check
rejections had dropped zero content, and no accepted compression anywhere in
the corpus grew in bytes. A size gate at the end of dispatch now returns
`NoOp`, which also lands in the memo's skip tier so repeat blocks
short-circuit. `BlockAction::NoCompressionApplied` carries `declined_by` so
these are counted in `proxy_compression_declined_no_shrink_total` rather than
silently absorbed — otherwise the rejection counter would fall whether the
waste went away or the gate started declining work that pays.

**A capture-worker alarm that was not real.** `ctx_observe_worker_gone` looked
like 60,312 drops; scoped to the running process it was zero. `proxy.log` is
not rotated, so raw counts span months and restarts. Root cause (a panic in
`extract_constraint` slicing mid-character, killing the worker thread and with
it the channel receiver) was already fixed by `0eaf0ce1`. Kept a recurrence
guard: drops counted in an atomic, reported at 1/10/100/… at ERROR, exposed via
`dropped_captures()`. `claude-launcher` now rotates the log on proxy start,
keeping four generations.
