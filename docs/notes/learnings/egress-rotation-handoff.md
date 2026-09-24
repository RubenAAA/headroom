# Learning: gate the egress handoff, not candidate probing

- **Source:** implementation review of the Rust `nord-socks-egress` helper and
  `contrib/zen-rotate-watch.sh` (2026-09-23).
- **Claim:** the watcher's first drain cannot prevent a request from starting
  while a replacement Nord exit is being probed. Do not close old lane tunnels
  immediately after probing: mark that opaque egress unavailable in Headroom,
  fail new selections fast, then confirm that egress's own counter
  (`egress_in_flight` on `/debug/inflight`) is drained before switching and
  closing old tunnels. This covers reused SOCKS tunnels too, where no new
  local SOCKS handshake occurs. If the second drain fails, keep the old exit
  and reopen the lane.
- **Claim:** drain one egress, not the proxy. With several sessions active the
  global count rarely reaches 0, so a whole-proxy drain timed rotations out
  while the gated lane answered 503 for nothing. The per-egress count is taken
  under the maintenance lock and rides in the response body to its last byte.
  A turn parked in the 429 hold gives it back, and each hold probe re-takes it
  or, while the egress rotates, is skipped. A failed drain poll counts as
  "not drained yet", not as a failed rotation.
- **Claim:** a reactive 429 rotates one pool lane; it must not move the clock for
  the next timed all-lane rotation, or a sustained stream of 429s can starve
  scheduled rotation indefinitely.

These safeguards are code-reviewed and covered by local race/contract tests;
they do not establish a live Nord timing bound. See `contrib/README.md` for the
operator behavior.
