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
