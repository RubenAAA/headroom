# Learning: the "hang under load" was the spinner sidecar, not a lock

- **Source:** proxy logs 2026-09-14/15 (`~/headroom-proxy.log`, `.log.1`),
  audited 2026-09-15 with the scripts in `/tmp/hr-logaudit/`.
- **Claim:** no lock, blocking call, or runtime starvation on the request
  path explains the stalls. Every one of the 612 `parse`/`pre_forward`
  stage samples over 5 s (median 12.0 s, p95 57.6 s) was a spinner sidecar
  whose direct attempt sent `claude-muse-spark-1.3` to api.anthropic.com
  and got `404 model: ...`. The routed sidecar declines under
  `--redact-sensitive` (deliberate, privacy), and the direct fallback then
  sent the Zen alias to Anthropic. 2,487 such fallbacks; detect-to-fallback
  median 3.9 s, p95 35 s, plus 494 retries. Fixed: the direct sidecar skips
  any model with a route entry and holds off ten minutes after a 404
  (`sidecar::direct_sidecar_model`, `sidecar_direct_skipped`).
- **Second stall:** `zen_concurrency_cap_exceeded` 1,152 times, every one
  waiting the full 30 s and then proceeding without a slot. The Zen slot
  was held through the 429 hold loop (up to 187 s), so four held turns
  pinned all four slots. Fixed: the slot is released when a hold begins
  (`routed/retry.rs`).
- **Latency vs concurrency, for the record:** median total 1.9 s at 2-5
  in flight, 3.1 s at 6-10, 14.5 s at 11-20, 19.6 s at 101+; upstream time
  dominates the median at every level. Proxy-only p95 went 291 ms to 35.8 s
  only through the sidecar stall above.
- **Ruled out (read, not measured):** tokio runtime config, shared reqwest
  client and pool, SSE loop, sqlite `spawn_blocking` discipline, the
  `trackers` mutex in `prefix_replay` (short holds, no `.await` inside).
  `ccr.put` runs inline on the worker thread in the offload path; the
  `compression` stage it sits in never exceeded 129 ms, so it is not the
  cause today.
