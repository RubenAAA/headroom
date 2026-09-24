# Learning: normalize the SOCKS CONNECT bound address

- **Source:** controlled Rust Nord SOCKS shadow comparison and local Reqwest
  regression test (2026-09-24).
- **Confirmed:** `curl` completed through the Rust relay, while the helper's
  Reqwest probe failed on all eight lanes with
  `SocksConnect(Parsing(Other))`. A local mock reproduces that parser error
  when a SOCKS5 success reply contains an empty domain `BND.ADDR`.
- **Inference:** the live upstream's CONNECT reply contains a bound-address
  field that curl tolerates and Hyper's Reqwest connector rejects. The
  pre-fix trace did not capture successful reply fields, so the exact live
  trigger is unconfirmed.
- **Fix:** return a canonical unspecified IPv4 `BND.ADDR` to local SOCKS
  clients, preserving the upstream reply code. SOCKS CONNECT clients do not
  use the bound address. Startup candidate verification now goes through the
  same local lane code, so it gets the same normalization. Trace mode records
  only reply metadata, not the bound address or credentials.
- **Status:** all 14 local helper tests pass, `cargo fmt --check` and helper
  Clippy pass, and the release build succeeds. After a cooldown, the final code
  passed an isolated live shadow run: 8 exits started, sequential `curl`
  succeeded, and Reqwest's helper probe passed on all 8 with 8 unique
  fingerprints. Safe trace metadata confirmed successful normalized replies.
  The Rust relay is installed on separate candidate ports `19300`–`19307`; the
  Python relay remains on `18600`–`18607` for rollback. The worktree release
  binary's isolated smoke check served `/healthz` and reported all 8 configured
  egress IDs. The first restart selected the main checkout binary because the
  restart script resolved `NEW_BIN` before loading the installed worktree path;
  that path-order bug is fixed in the worktree and installed script. The
  corrected live cutover is active: the installed binary matches the worktree
  release, `/healthz` succeeds, and `/debug/zen-egresses` reports all 8 lanes.
  The helper reports 8 distinct live exit fingerprints, while the Python relay
  remains available on its original ports. Two old device-wide rotation
  watchers were replaced by one `mode=per-egress` watcher; the VPN connection
  was left up. The first post-restart request was assigned to lane 0 and
  received HTTP 200 response headers from `opencode.ai` on attempt 1 after
  13.59 seconds. The relay had recorded 2 SOCKS connects. Its stream was still
  active and cache-health response samples remained zero, so this is not a
  before/after latency comparison; use a completed turn for cache counters and
  broader timing.
