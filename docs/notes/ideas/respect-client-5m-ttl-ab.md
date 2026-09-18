# Idea: honor explicit client 5m TTLs (`--respect-client-5m-ttl`)

- **Status:** open (live `false`; prescribed test path exists but unrun)
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
