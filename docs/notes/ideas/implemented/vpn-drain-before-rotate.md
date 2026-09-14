# Implemented: drain-before-rotate VPN rotation watcher

- **Status:** shipped (`contrib/zen-rotate-watch.sh`, committed; drain pieces
  need the one-time restart — see `../vpn-one-time-upgrade-restart.md`)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md`
- **Summary:** tails the log for routed-429s, drains in-flight via
  `/debug/inflight` (bounded 90 s, then rotates anyway), rotates country until
  egress moves, 120 s cooldown + flock. Proactive hourly ±10 min cycling into
  quiet moments; reactive rotations reset the clock; countries recycled.
  Rotation notices via per-session files + `rotation-notice.sh` (no billed
  `claude --resume` wakes).


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Making rotations safe (2026-09-10)

A rotation kills in-flight TCP by design (exit IP changes, server RSTs the
old 4-tuple) and strands pooled keepalive sockets as corpses — but the
proxy must NOT restart (in-memory session state loss = fleet-wide recaches
plus an error burst). Safety is drain-then-shrink instead:

1. **Drain before rotating.** The watcher polls `GET /debug/inflight`
   (loopback-only; counts `forward_http` AND routed `handle_messages`
   turns until response dispatch — headers-wait, buffered bodies, CCR,
   fallback — but not streamed body bytes, which flow after the guard
   drops) and only runs `nordvpn connect` at zero, bounded by a 90 s timeout — then rotates
   anyway and the stragglers die truncated-but-marked. New turns starting
   mid-drain are a residual race; the window shrinks from "whenever the
   429 lands" to seconds.
2. **Shrink the corpse window.** `--pool-idle-timeout 25s` (was hardcoded
   90 s; now a flag, default unchanged). Corpses age out in ~25 s instead
   of ~90 s at the price of more TLS handshakes. Corpse turns fail fast
   (RST on first write) and self-heal through the pre-commit retry onto a
   fresh connection.
3. **What already covers the rest:** stream hold + attempts (pre-commit),
   finisher + marker (post-commit), resume prompt on wake.


## Proactive cycling

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

## Proactive cycling (2026-09-10)

The watcher also rotates on a schedule — every hour ±10 min jitter, so no
cron-shaped pattern — instead of only after a refusal. A scheduled rotation
fires only into a quiet moment (`in_flight==0`; busy box defers 5 min and
rechecks), drains first, and wakes nobody when the drain is clean — only
stragglers that died truncated get the resume prompt. Reactive rotations
reset the schedule clock (a fresh exit needs no cycle on top of it).
Countries are recycled, not one-passed: the loop reshuffles until the
egress moves or 10 min elapse, then fails loudly. Manual rotations use
`zen-rotate-watch.sh --rotate-now [country]` (same drain + wake).
