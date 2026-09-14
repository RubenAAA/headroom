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
