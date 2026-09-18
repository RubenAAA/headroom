# Rejected: split cache TTL (5m tail + 1h prefix)

- **Status:** rejected 2026-08-17, reverted (`b63030bc` "it cost 5x in
  production") — live `--split-cache-ttl false`
- **Source:** `contrib/headroom-flags.sh` ("TRIED AND REVERTED, 2026-08-17")
- **Summary:** 1h on tools/system prefix + 5m tier on the message tail (long
  TTL re-taken every 10th turn as anchor) priced at −38% in
  `bench/cachesim.py` and cost **+511% depth-standardised creation** on live
  traffic, flat across 489 minutes (1,009 ledger-joined turns via
  `bench/_ttlverdict.py`: e.g. depth 20–50: 772 → 13,092). Mechanism: the 5m
  hedge entry is dead before reuse, so a newest-entry miss falls back to the
  system boundary and rewrites the conversation. Simulator failure is itself
  a finding — it modeled expiry and renewal correctly in isolation while the
  interaction was invisible to it (demonstrated 5× error).
- **Revisit only** with a live-traffic A/B, never the simulator.
