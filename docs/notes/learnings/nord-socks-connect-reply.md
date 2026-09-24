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
- **Status:** all 14 local helper tests pass, and the updated helper builds.
  A live startup after the first normalization change found only 7 distinct
  exits and stopped before the candidate-probe path was refactored. The final
  code has not had a live shadow check; keep the Python pool active until it
  passes after a provider cooldown.
