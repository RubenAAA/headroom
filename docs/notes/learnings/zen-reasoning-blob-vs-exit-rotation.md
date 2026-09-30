# Learning: Zen's reasoning blob dies with the VPN exit that fetched it

- **Source:** `~/headroom-proxy.log` 2026-09-14 09:10–09:12Z, four
  `local_model_upstream_error` 400s on `muse-spark-1.3-contributor-free`;
  `~/.zen-rotate.last` = 13:07:02 +04, i.e. a rotation three minutes earlier.
- **Claim:** OpenCode Zen binds `reasoning.encrypted_content` to the caller it
  was issued to, and the exit IP counts as the caller. `zen-rotate-watch.sh`
  rotates that exit on every 429, so a rate limit followed by a rotation makes
  every blob in the transcript unusable:
  `[invalid_request_error] reasoning encrypted_content was not issued to this
  caller`.
- **Why it dead-ends:** the stale envelope lives in the client's transcript,
  not in the proxy, so it is resent on every later turn. One rotation kills the
  conversation for good — the user sees the same 400 on each `continue`.
- **Rule:** the free tier and the rotation are one system. Do not replay
  encrypted reasoning to Zen at all; `UpstreamKind::strip_unreplayable_reasoning`
  drops the items and the `include` ask. Codex and Cursor keep the replay —
  they are reached from one stable identity.

## Re-measured 2026-09-30

A scratch proxy with the strip off, no egress pool, the Spark route to Zen:

| Replayed blob | Zen answered |
|---|---|
| The caller's own, from the previous turn | 200 |
| One issued through the live proxy (a relay lane) | 400 "not issued to this caller" |
| The caller's own with 40 characters overwritten mid-blob | 200 |

So the 400 still exists and a foreign blob triggers it. A mangled blob passes,
so Zen reads who the blob was issued to, not its contents. The audit note's
earlier "200 for every arm" came from probes through the live proxy, which
strips the blob first; it said nothing about Zen.

`--zen-reasoning-replay` (default off) sends the blobs and lets Zen's answer
pick which to keep: `routed/reasoning_blobs.rs`. On that 400, `send_with_retry`
drops every blob in the body, remembers their fingerprints and resends once;
later turns drop remembered blobs before sending. Scratch run: a turn carrying a
stale blob got the 400 once and then a 200; the next turn carrying the same
stale blob plus a fresh own blob got a 200 with no 400. The rule does not
depend on what "caller" means (exit IP, session id or both), which this run did
not separate. Not measured: whether a replayed blob changes what the model does.

**Through the live lane pool (flag on, 2026-09-30).** A blob issued on one
lane, replayed with four different lane keys (different lanes): 200 every time,
no refusal. So lanes alone do not invalidate a blob, which fits a binding to the
Zen session rather than the exit IP; the 2026-09-14 rotation may have coincided
with a session change. Not separated. The one refusal seen in the first hour
(12:36:01Z) came on a turn that had failed over from slot 0 to slot 1 after a
429. The refusal took about 8 s and the resend answered 13 s later, so a
refused blob costs the turn about 20 s, not one round trip. The first hour:
343 upstream 200s, 1 refusal, 219 429s.
