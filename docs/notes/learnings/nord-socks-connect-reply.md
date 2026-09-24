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
  The Rust relay is installed on separate production ports `19300`–`19307`; the
  Python relay remains on `18600`–`18607` for rollback. Proxy cutover is still
  pending: the first restart selected the main checkout binary because the
  restart script resolved `NEW_BIN` before loading the installed worktree path.
  That path-order bug is fixed; wait for zero in-flight requests, restart onto
  the worktree binary, and confirm `/debug/zen-egresses` before approving the
  live proxy behavior.
