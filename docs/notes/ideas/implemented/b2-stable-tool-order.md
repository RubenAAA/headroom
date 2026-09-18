# Implemented: B2 — stable tool order replay

- **Status:** shipped (commit `b4bee28c`; flag `--cache-stable-tool-order`,
  default `true`, live `true`)
- **Source:** `crates/headroom-proxy/src/cache_stabilization/tool_order.rs:19-29,113-122,170-172`
- **Value:** replays the last-forwarded tool order, appending new tools at the
  tail, so a late MCP tool arriving mid-array doesn't re-key the whole tools
  prefix. Lossless and steady-state no-op (`false` when already ordered).
  Measured: recovered 20.5k of 104k tokens (19.7%) on an MCP-mid-array event.
  Declines (byte-for-byte) on customer markers — hands PAYG bodies to E1/E3 —
  and on roster removal (subset guard); re-anchors every turn including
  declines. Runs last so the recorded order is the cached order
  (`proxy.rs:2699-2713,6183-6213`).
- **Next:** none open; the residual question is roster flap (B3 pin), not order.
