# Zen free tier refuses the proxy; the 429 hold turned that into hangs

Window: proxy logs 2026-10-30 to 2026-11-06, plus direct probes on 11-06.

- Zen answered all 7,453 routed requests in the window with 429. None
  returned 200, on any egress lane.
- The body names the cause: `FreeUsageLimitError`, "Free usage limit
  reached. Free models work in OpenCode". A direct `curl` without the proxy
  got the same answer for `glm-5.3-flash-free` and
  `deepseek-v4.1-flash-free`.
- Retry-After is not a constant. 50525s at 09:57 UTC and 38547s at 13:17
  UTC both count down to 00:00 UTC, so it marks a daily quota. The comments
  in `routed/zen_hold.rs` and `routed/retry.rs` that call it a constant
  (~53568) are wrong.
- The unbounded 429 hold (`retry_zen_hold_budget_ms = 0`) parked 881 turns
  in the window. None recovered or completed. Each one hung until the client
  gave up. Most were tool-less turns the `no-tools->glm` rule had moved,
  WebFetch's page summary among them.

Changes on 11-06: commit `71e20d647` stops holding router-moved turns, so
they fall back to the client's model. `contrib/headroom-flags.sh` turns off
the router and the hold. With the hold off, an explicit `glm` agent turn
fails in about 2s with Zen's message instead of hanging.

Before re-enabling either, get a 200 from the target through the proxy.
