# Learning: Zen's free-tier limit follows the exit IP, resets at 00:00 UTC, and a fresh exit carries little

- **Source:** `~/headroom-proxy.log` 2026-09-29 08Z to 2026-10-01 21Z,
  `~/.local/state/headroom/egress-relay/relay.log`, `~/zen-rotate-watch.log`,
  and probes through the live lanes on 2026-09-30 21:09–21:25Z.
- **Claim:** `429 FreeUsageLimitError` ("Rate limit exceeded") is keyed on the
  exit IP. `Retry-After` counts down to the next 00:00 UTC, the same second on
  every limited exit, which is a shared reset time and not a shared account.
- **Evidence:**
  - An unauthenticated probe carries no credentials or IDs. Through used Nord
    exits (lanes 0–7) it got 429. Through lane 8's Proton exit and through two
    Nord servers nothing had used that day (`us50`, `us62`) it got 403
    "free tier can only be used from within OpenCode": not limited.
  - Six rotations of lane 5 onto three different Nord servers all answered 429
    with the same countdown.
  - Every day has the same shape: 429s climb from midday (725 in the 23Z hour
    on 09-29) and fall to 3 in the first hour after 00Z.
  - Lane 8 moved to a fresh Proton server and answered 153 requests from
    21:11:59Z to 21:15:20Z, then 429 from 21:15:21Z. One sample of a fresh
    exit's allowance: about 150 requests.
- **Not the reasoning-blob change:** `--zen-reasoning-replay` went on at 16:19Z
  on 09-30, after the 429s had already climbed since 09Z, and 2 of the last 60
  forwarded bodies carry `encrypted_content`.
- **Nord rejects most logins from our one account.** All eight Nord lanes share
  `nord-socks-credentials.json`. Lane 0 logged 573 `PermissionDenied` (SOCKS
  auth rejected) across 68 servers. Acceptance now: 2 of 12 in a relay sweep,
  2 of 20 and 3 of 14 in probes. Closing every session did not restore logins
  within 42 s, so it reads as a throttle on login attempts, not a cap on open
  sessions. The rotator log's "Too many connection attempts" (106 times) fits.
  Cause not shown.
- **The watcher could not rotate a limited lane at all.** It read `egress_id`
  from the top level of the log line; the proxy nests it under `fields`. "Zen
  rate-limit event had no egress id" was logged 5,087 times since 09-24.
- **Rotation, measured:** after four 429s in a row a lane answered next 7 times
  in 4,978 without a rotation and 21 times in 102 with one. The 21% is what a
  rotation gives a lane whose exit still has allowance; it does nothing while
  the whole pool is spent.
- **Change:** the watcher reads `fields.egress_id`, and each egress gets at
  most 6 rotations an hour, a backoff after a failed one (240 s doubling to
  1 h), and a probe of Zen through the new exit: a 429 or an unreachable Zen
  counts as a failed rotation. The relay's heal wait doubles per heal in a row
  (300 s to 1 h). Manual rotation skips the budget. Tests:
  `scripts/test-zen-rotate-watch.sh`, `heal_cooldown` in `egress_relay.rs`.
- **A new server frees a lane (01:25–01:35 local, 437 answered).** The relay's
  own heal moved lanes 0, 1, 6 and 7 onto new Nord servers (`se13`, `se8`,
  `us59`, `us61`) and those four answered 82, 43, 298 and 14 requests with no
  429. Lanes 2–5 and 8 kept their servers and stayed limited. So rotation
  cures a lane when it lands on an exit with allowance left, and Nord's
  throttle still lets some logins through.
- **Not established:** the size of a fresh exit's daily allowance (one
  sample), whether Zen counts requests or tokens, why Nord throttles logins,
  and whether rotating onto fresh Nord servers works once logins are accepted.
  The probe itself spends one request of the new exit's allowance.
