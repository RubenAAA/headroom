# Learning: Proton's free plan adds one Zen lane, through wireproxy

- **Window:** measured 2026-09-30 00:25-00:40 local (+04), WSL2 in mirrored
  networking, one free Proton account, `wireproxy` 1.1.3.
- **Claim:** Proton's free plan is worth one extra egress lane, and only one.
  It has no SOCKS5 service and allows one VPN connection per account, so more
  SOCKS ports over the same tunnel all exit from one address and share one
  Zen rate limit. Nord gets eight to ten lanes because its SOCKS service
  accepts many sessions per account at once; Proton's does not exist.
- **Evidence:** five WireGuard configs from the Proton dashboard, one per free
  server (NL-FREE-119, CH-FREE-7, NO-FREE-5, PL-FREE-13, RO-FREE-23), each run
  alone through `wireproxy` and probed through its SOCKS port: five distinct
  IPv4 exits, `opencode.ai/zen/v1/models` HTTP 200 in 0.84-0.94 s on every
  one, about 4.3 MB/s on a 20 MB download. `ifconfig.me` saw an IPv6 exit on
  the four configs that carry an IPv6 address; the RO config has none and
  exited over IPv4.
- **Tool choice:** tun2socks and tun2proxy turn a SOCKS proxy into a TUN
  device, the opposite of what the relay needs. `wireproxy` runs WireGuard in
  userspace and serves it as a loopback SOCKS5 port: no root, no TUN, no route
  change, so Claude and Codex traffic never touch Proton. Proton's own Linux
  apps only route the whole device.
- **Consequences:** `egress-relay` runs one Proton lane after the Nord lanes.
  Rotation must stop the old tunnel before starting the next config, so a
  candidate cannot be probed beside the live lane the way a Nord server can:
  the lane drains first. Free-server exits are shared by many free users, so
  the lane should be expected to hit Zen's limit sooner than a Nord lane; not
  yet measured.
- **Status:** shipped with the `egress-relay` rename. Rate-limit behaviour of
  the Proton lane under real Spark load is still open.
- **Source:** the setup session of 2026-09-30; configs in
  `~/.config/headroom/proton-wg/`, relay code in
  `crates/headroom-proxy/src/bin/egress_relay/proton.rs`.
