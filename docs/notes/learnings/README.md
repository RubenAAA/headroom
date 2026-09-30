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
- [`nord-socks-one-way-idle-timeout.md`](nord-socks-one-way-idle-timeout.md):
  a per-direction read timeout can close an otherwise active SSE tunnel; use
  a shared bidirectional idle deadline instead.
- [`nord-socks-acceptance-flaps.md`](nord-socks-acceptance-flaps.md): the
  SOCKS servers that accept the account change every ~10 minutes and Nord
  says nothing about why; keep the list live and re-rotate lanes by probe.
- [`proton-free-wireproxy-lane.md`](proton-free-wireproxy-lane.md): Proton's
  free plan adds one lane, not a pool (one connection, no SOCKS service);
  `wireproxy` serves it as loopback SOCKS without touching routes.
- [`spark-side-requests-look-like-cache-breaks.md`](spark-side-requests-look-like-cache-breaks.md):
  Claude Code's spinner request shares a session key with real turns, so
  comparing with the last turn alone logged false prefix breaks; compare with
  the last four.
- [`spark-stream-drops-and-503s-2026-09-30.md`](spark-stream-drops-and-503s-2026-09-30.md):
  TapBuy fleet kills sorted by cause: 503s fixed by lane failover, mid-stream
  drops are peer closes (10 of 19 near proxy restarts), malformed CCR hashes
  now fall back to keyword search.
