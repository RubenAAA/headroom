# Idea: volatile setup relocation after the breakpoint (§2/§4)

- **Status:** open — generalizes two shipped holds, one span at a time, opt-in per span
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §2 + §4. Proxy: `volatile_detector.rs:1-31` (read-only scan, 10/req cap, never mutates), `drift_detector.rs:1-34` (canonical system/tools/early-message hashes), working-dir hold (one `cd`: 65,051 vs 4,637 peer avg) and role-sentence hold (788k tokens/day of flips) — both opt-in because they add/rewrite client text.
- **Signal constraint from learnings:** `volatile-shape-needs-change.md` (shape flagging was 86% noise; detector now warns only on change). Mine change-based volatile/drift events by recache waste — never raw shape hits. Depth-binned, first turn dropped, per `recache-counting-rules.md`.
- **Marker/TTL constraints from rejections:** `rejected/strip-system-cache-breakpoints.md` (removing system markers: 11,118 → 39,208 mean creation — system markers are the floor under tail misses; 4-marker cap binds). Relocation must PRESERVE marker layout and budget, never strip to make room. `rejected/split-cache-ttl.md` (simulator −38% but live +511% creation — simulator blind to the interaction): validate live A/B only, never the simulator.
- **Ordering constraint:** `absorbed-rebuild-boundary.md` (open, parked: `rebuild_boundary` is set from inbound drift BEFORE outbound stabilizers absorb the edit — a held-back edit still reads as a boundary). Any relocation prototype must account for inbound-vs-outbound ordering or it re-opens that hole; coordinate, don't duplicate.
- **Next:** rank spans by waste, prototype one span per window (stable bytes forward, live value at the tail — the working-dir pattern). Per-span keep/remove with numbers.
- **Traps:** rewriting text the model depends on verbatim; coordination that becomes a bottleneck. Stays opt-in per span.
- **Tool:** `scripts/volatile_relocation_mine.py` (stdlib-only; joins `volatile_content_detected` locations to `cache_recache_observed` waste on conversation_key; co-occurrence, not causation).

## Findings 2026-09-24 — no candidates in a week of live logs (miner run)

2,001,375 lines over `headroom-proxy.log` + 4 rotated archives (09-18→24): 2,057 recache events, 6,233,836 wasted tokens — and **0 `volatile_content_detected`** rows. Same window: 128,031 `volatile_content_suspected` (first sightings, INFO) and 0 `unchanged` (DEBUG, below the configured level). Consistent with the 2026-08-31 finding (81 multi-sample locations, every sample from a single request — blocks holding several dates, never churning).

- **Parked for lack of candidates, not rejected:** the miner is the ranking tool for when a `detected` fires; until then there is nothing to relocate beyond the two shipped holds. The detector itself is the alert — no new instrumentation needed. Re-run the miner on any window before proposing a span.
