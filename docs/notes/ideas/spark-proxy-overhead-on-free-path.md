# Idea: cut the proxy's own time on Spark turns

- **Status:** open. Measured, not changed.
- **Source:** `PERF` lines, `opt_ms` field, 2026-09-29 21:22-22:10Z (after the
  cache fix), 1,193 Spark turns.
- **Claim:** `opt_ms` p50 1,124, p90 4,219, max 8,636. On the 419 turns above
  500k tokens, p50 2,973 ms of a 11,404 ms TTFB (about a quarter). Median share
  of TTFB across all turns: 12%.
- **Claim:** what that time buys is small. `tok_saved` p50 11,732, 2.6% of
  `tok_after`, on a route where tokens are free.
- **Unknown:** which stage takes the time (live-zone compression, the redaction
  scan, replay, the continuity hashing added 2026-09-30). `stage_timings` events
  exist (3,801) but were not split by route.
- **Question the status quo:** compression exists to save paid tokens and
  window space. For Spark it saves 2.6% and delays the answer. It may also break
  cache continuity by rewriting old bytes.
- **Next:** split `stage_timings` by route for Spark; try
  compression off for `OpenCodeZen` on one lane and compare TTFB and cached
  fraction over a few hundred turns. Keep the CCR recovery working if it stays
  on.

## Re-measured 2026-09-30 (00:00-11:00Z, 4,242 Spark turns)

`opt_ms` is far below the 09-29 figures. By context size (`tok_after`):

| Context | Turns | `opt_ms` p50 / p90 | TTFB p50 | Share of TTFB, p50 |
|---|---:|---:|---:|---:|
| under 50k | 741 | 27 / 79 ms | 1,992 ms | 1% |
| 50-200k | 3,356 | 142 / 390 ms | 3,079 ms | 4% |
| over 200k | 145 | 512 / 794 ms | 4,798 ms | 11% |

By transform set: `ctx_inject` only, 3,558 turns, p50 101 ms; with
`memory_tools`, 684 turns, p50 300 ms. The two windows are not like for like:
today's sessions are smaller (145 turns over 200k, against 419 over 500k on
09-29), so part of the drop is size, not a fix. On today's mix the proxy's own
time is 4% of TTFB at the median, so this note is low value now. Reopen if long
sessions return and the over-200k share climbs past about 20%. Still unknown:
which stage the time goes to (`stage_timings` covers only the Anthropic
forward path, not routed turns).
