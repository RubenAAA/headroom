# Learning: device-wide VPN rotation resets routed streams

- **Source:** `~/headroom-proxy.log`, `~/zen-rotate-watch.log` and the nordvpnd
  journal, 2026-09-25 08:59–12:05Z.
- **Claim:** the `[truncated: the connection to the API dropped mid-response]`
  turns on Spark and Codex came from `zen-rotate-watch.sh` rotating the
  device-wide VPN on Zen 429s, not from the providers. The proxy had started
  without `HEADROOM_ZEN_HTTP_PROXY_POOL`, so Zen shared the route that Codex
  and Claude use.
- **Status:** fixed in scripts and logging. The pool now loads from
  `~/.headroom-zen-pool.env`, `restart-headroom.sh` warns when starting
  pool-less while the relay is up, and the translator passes the upstream
  error through to `stream_finisher`.

## Evidence

- 108 truncated turns: 92 Spark, 14 Codex, 2 Opus. 106 were logged as
  `"body ended early"` with `cause: None`.
- Each one had a matching `routed_stream_aborted` carrying `error decoding
  response body`, whose source is `hyper … ConnectionReset`. That event had no
  `request_id`, so following one request never showed it.
- Streams that started minutes apart died within 0–1 ms of each other, 17
  at once at 11:24:44Z. One shared event kills many streams; per-request load
  shedding does not.
- Each burst followed a watcher rotation by 13–76 s (for example: 429 at
  11:24:20Z, nordvpnd `CONNECTING` 11:24:23Z, 17 resets 11:24:44Z). The lag is
  the time an idle stream takes to send its next packet into the rebuilt
  tunnel and get reset.
- The watcher's drain could not reach zero while new turns kept arriving. It
  logged `timed out with in_flight=N; rotating anyway` and rotated.
- The routed early retry fired twice all day. Most streams had sent their
  first 8 KB to the client well before the reset.

## Misreading to avoid

`early_stream_retry.rs` once said Zen "sheds load by closing streams cleanly".
It was the same log shape: the translator ended each aborted stream without
an error, so a reset looked like a clean EOF. Before blaming the provider for a
routed drop, check `routed_stream_aborted` and the watcher log for a rotation
in the preceding minute.
