# Idea: let unused Nord lanes free their VPN device

- **Status:** done 2026-10-12. `0f65ee49f` (relay, watcher): a Nord lane
  with no connection for 15 minutes is idle. The 5-minute health check,
  proactive rotation (relay and `zen-rotate-watch.sh`) and the hourly
  sweep's login probes leave it alone; the first real connect wakes it.
  `egress-relay status` shows `idle` per lane. `0caa50319` (proxy): a new
  stream lane takes an egress used in the last 10 minutes that is quiet
  (nothing in flight, no turn for 60s), else the lowest cold one, else
  cycles as before.
- **Source:** user request, 2026-10-12. The Nord account allows ten devices,
  shared with the user's own machines.
- **Summary:** with no traffic, maintenance still logged in on every lane,
  so Headroom held all ten devices. Right after the restart 6 of 9 lanes
  were idle.
- **Unverified:** that Nord frees a device once nothing is logged in
  through it. Check the account's device count after 15 quiet minutes.
- **Tradeoff:** at low load traffic leaves through fewer exit IPs, so Zen
  429s and the rotations they trigger may come more often. Watch the 429
  rate in `zen-rotate-watch.log` against the week before.
- **Proton:** unchanged; its WireGuard tunnel stays up.
