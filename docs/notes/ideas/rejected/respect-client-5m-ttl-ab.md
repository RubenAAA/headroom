# Idea: honor explicit client 5m TTLs (`--respect-client-5m-ttl`)

- **Status:** rejected 2026-09-28 — the flag has nothing to act on. Claude
  Code no longer sends all-5m bodies: 0 of 30,647 turns over 09-18→28.
- **Source:** `contrib/headroom-flags.sh` (comment block above the commented
  `--respect-client-5m-ttl true`); `config.rs:962-967`
- **Value:** ~12.7% of creation tokens (~6.7M input-equivs at stake) sit in
  all-5m bodies we currently re-pin to 1h. When true, all-explicitly-5m
  bodies skip the pin; main-loop (1h) and mixed bodies still pin, so the tail
  hedge from the +511% split-TTL lesson (`rejected/split-cache-ttl.md`) is
  untouched — this is explicitly NOT a return to split TTL.
- **Next (as prescribed in flags.sh):** first
  `upstream-python/bench/_ttlsubagent.py` in mapping mode — confirm all-5m ⟺
  short conversations over days of `client_ttl` lines. Then the same script
  in A/B mode across the flip epoch. TTL wire changes burn once
  (`config.rs` doc), so no reasoning-only enable.
- **Exit:** enable if mapping holds and A/B shows net win depth-binned;
  reject with the number otherwise.

## Findings 2026-09-28 — no all-5m bodies left

`_ttlsubagent.py` in mapping mode over all five logs (09-18→28), shape of
the client's own markers on `vs_stock_turn`:

| client_ttl | turns | conversations | share in <5 min conversations |
|---|---|---|---|
| AllOneHour | 15,317 | 171 | 2% |
| Unmarked | 15,038 | 272 | 11% |
| unknown (older lines) | 292 | 13 | 13% |

No `AllFiveMinutes` and no `Mixed`. `pin_1h_applies`
(`cache_ttl.rs:401`) only skips the pin for `AllFiveMinutes`, so turning the
flag on would change no request. The 12.7% figure above came from a client
version that marked subagent turns 5m; the current one marks them 1h or not
at all. Re-open only if `AllFiveMinutes` shows up in that table again.
