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
  use the bound address. Trace mode records only the reply code, reserved byte,
  address type, and address length.
- **Status:** all 14 local helper tests pass, and the updated helper builds.
  The post-fix Rust binary has not yet passed a live Nord shadow check. Keep
  the Python pool active until that check passes after a provider cooldown.
