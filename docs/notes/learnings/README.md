# Learnings

One file per durable finding, extracted from the working notes. Each names
its evidence window and the source doc — refuted hypotheses stay (they stop
retests), with status in the file.

Measurement-led files (`recache-*`, `savings-*`, `proxy-*`) scope every
number to its window; do not quote across windows.

- [`offload-loses-at-2000-bytes.md`](offload-loses-at-2000-bytes.md): at a
  2,000-byte floor, retrievals and re-reads cost more than offload saves;
  two thirds of retrievals repeat one from an earlier turn.
- [`device-wide-rotation-resets-routed-streams.md`](device-wide-rotation-resets-routed-streams.md):
  Spark/Codex "dropped mid-response" turns were VPN rotations resetting a
  pool-less proxy's streams, not provider load shedding.
- [`zen-retry-after-guidance.md`](zen-retry-after-guidance.md): use bounded
  provider guidance, but preserve the measured guard against Zen's stale,
  oversized Retry-After value.
- [`egress-rotation-handoff.md`](egress-rotation-handoff.md): close the
  per-lane admission race before dropping old SOCKS tunnels; reactive lane
  rotations must not postpone the pool-wide timed pass.
- [`nord-socks-connect-reply.md`](nord-socks-connect-reply.md): normalize the
  upstream SOCKS CONNECT bound address for strict local clients; the live
  trigger still needs post-fix verification.
