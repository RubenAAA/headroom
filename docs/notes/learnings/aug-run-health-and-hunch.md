# Learning: Aug-08 run health snapshot + dropped hunch

- **Source:** `docs/notes/proxy-experiments-2026-08.md`
- **Claim:** proxy adds no measurable latency (opt_ms p50 11); idle-gap colds expected; plus the dropped injection-failure hunch below (recorded so nobody re-runs it).


## Healthy baseline

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## Healthy — checked, no action

- `opt_ms` median 11ms, p90 29ms, max 211ms. Proxy adds no measurable latency.
- `ttfb_ms` median 1571, p90 3129. The 77.8s max traces to upstream retries.
- `cache_hit_pct` 99 median, 9 cold starts out of 311 turns.
- Cold cache on a large conversation after an idle gap is expected, not a
  fault. The run went quiet 12:00Z–20:00Z; the first turns back (156 and 104
  messages) wrote 155K and 88K tokens with zero cache read. Upstream cache TTL
  is minutes, so nothing survives an eight-hour pause. Same process
  throughout — no restart, so the scoping baseline above still holds.
- `tok_inflated` is 0 across every turn.

---


## Dropped hunch

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## Hunches tested and dropped

- **"Injection failure causes the `early_messages` busts."** Prompted by
  11:49:07 (injection missing) sitting 3 seconds before 11:49:10 (22,997
  tokens wasted, `early_messages`). It does not generalise: the other
  injection failure at 10:33:11 has no recache within 120s, and 11 of the 12
  genuine-drift events have no injection failure anywhere near them. Recorded
  so nobody re-runs it. The single pairing may still be real for that one
  turn — it is just not the general cause.

---
