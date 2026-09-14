# Implemented: footprint recording off the request path (plan 3)

- **Status:** implemented and live (2026-09-04)
- **Source:** `docs/speed-ideas.md` plan 3
- **Summary:** `record_request_footprint` (two ~200 kB parses + fsync'd tracker
  write; 14 ms p50, 63 s max) moved to `spawn_blocking` after the footprint
  stage — `Bytes` clones are refcount bumps, tracker mutex orders writes.
- **Verify:** footprint stage p50/p99/max ≈ 0 over a day; tracker totals
  identical on a 50-request replay.


## Detail

*moved from `docs/notes/speed-ideas.md`*

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
