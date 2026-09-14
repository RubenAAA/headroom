# Proxy latency: what costs, what to do, how to know it worked

Measured on the live log (`~/headroom-proxy.log` + `.log.1`, 2026-09-03
11:20Z–17:35Z, 2,875 `stage_timings` lines on `/v1/messages`, all joined to
`outbound_body_bytes`). Scripts: `/tmp/lat_profile.py`, `/tmp/final.py`,
`/tmp/mem_trace.py`, `/tmp/gaps.py`, `/tmp/fts_time.py`.

> Note (2026-09-10): §0 is current; §§1–2 are the 2026-09-03 baseline
> and their `proxy.rs` line numbers predate growth to ~14k lines.
> Re-resolve cites before costing work.
> Status 2026-09-11 (§0b below): Plan 0 + Plan 3 confirmed shipped;
> Plan 1 unshipped and its §0 inversion table partly stale — read §0b
> before acting; thinking-drop gate decided (drop is free, no change);
> shutdown fix live with 2 clean drains + 1 bounded overrun, deploy-script
> half still unchecked; Plan 2 step 1 unshipped.

## 0. Pick up here (2026-09-04)

> **Moved to [`ideas/memory-search-wide-pass-lever.md`](ideas/memory-search-wide-pass-lever.md)** — §0 pick-up context (shared by the three points below).

## 0b. Status 2026-09-11 — what changed since §0

Checked against code and `~/headroom-proxy.log` (2026-09-10).

> **Moved to [`ideas/implemented/memory-search-timings-instrumentation.md`](ideas/implemented/memory-search-timings-instrumentation.md)** — §0b confirmations (plan 0 half; plan 3 half also confirmed — see footprint file).

> **Moved to [`learnings/speed-profile-baseline.md`](learnings/speed-profile-baseline.md)** — full 2026-09-03 profile (the baseline the plans measure against).

## 2. Plans

> **Moved to [`learnings/speed-profile-baseline.md`](learnings/speed-profile-baseline.md)** — plan 4 (no change) + measured-vs-inferred ledger stay with the baseline.

