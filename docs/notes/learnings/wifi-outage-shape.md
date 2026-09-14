# Learning: wifi outage has its own shape (no drain possible)

- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (wifi/host flaps)
- **Claim:** unplanned total outage (DNS + connect + in-flight fail) — no
  watcher trigger, retry budgets burn against a dead link. Coverage: pre-commit
  turns retry transparently while budget lasts; pure-passthrough has no loop
  (client re-issues); transport-exhaustion 503+Retry-After fires stock client
  retry; post-commit drops end marked via finisher + Stop-hook auto-continue;
  corpses age out on the 25 s TTL.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Wifi switches / host flaps (2026-09-10)

Same machinery as rotations, minus the drain: a wifi switch is an unplanned
total outage (DNS + connect + in-flight all fail), so no watcher trigger
fires and retry budgets burn against a dead link. What covers it:

- Pre-commit turns (buffered POST, SSE inside the hold window) retry
  transparently while budget lasts; pure-passthrough has no retry loop
  (streamed body, nothing to resend) — the client re-issues from its copy.
- Transport exhaustion now answers **503 + `Retry-After: 2`** with
  `x-headroom-retryable: transport-exhausted` (proxy-transient, not provider
  5xx), so stock client retry fires instead of stalling the session.
  Non-transport failures stay 502 with no `Retry-After`.
- Post-commit drops end marked-but-well-formed via the finisher; the Stop
  hook (`retry-dropped-turn.sh`) auto-continues both tool-discarded AND
  plain-text truncations (max 3/session, never fails closed).
- Corpses on every destination age out via the 25 s pool TTL; first turns
  after reconnect spend one attempt each failing fast onto fresh conns.
