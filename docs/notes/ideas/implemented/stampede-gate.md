# Implemented: same-head stampede gate

- **Status:** shipped 2026-09-15, `--cache-stampede-gate` (default off,
  on in `contrib/headroom-flags.sh` to measure),
  `--cache-stampede-wait-cap` (10 s). Module
  `cache_stabilization/prefix_stampede.rs`; three integration tests in
  `tests/integration_stampede_gate.rs`.
- **Mechanism:** Anthropic makes a cache entry readable only once the
  response that wrote it begins. A fan-out of subagents of one type, or
  parallel sessions on one repo, share model + `system` + `tools` and are
  sent in the same instant, so each pays write price for the same head.
  The first request under a cold head leads; followers park until the
  leader's response headers arrive or the cap passes. Nothing about the
  request changes, only when it goes.
- **Prior evidence:** overlap raised the re-cache rate 7.7x (17.2% vs 2.2%
  over 29,432 turns, `concurrent-flag-honest.md`), filed then as "nothing
  to do". First-turn writes were 2.7M tokens on 2026-09-07 with no counter.
- **Measure it by:** `stampede_follower_released` log lines
  (`waited_ms`, `release`) and the `stampede_wait` stage in
  `stage_timings`. A follower released `leader_warm` should show
  `cache_read_input_tokens` covering the head; `timeout` releases mean the
  cap is short for that head size.
- **Cost:** a cold fan-out starts at most `wait_cap` later. Warm heads and
  distinct heads never wait.
- **Measured 2026-09-15 (9/14-15 logs, `/tmp/hr-logaudit/g3_stampede.py`):**
  31 cold writes (730,390 tokens), zero of them behind a same-head leader
  still before first byte. Fingerprints cover 46% of completed turns, so
  this is not a refutation, but it is not a win either. Move this file to
  `rejected/` if a week of `stampede_follower_released` shows nothing.
