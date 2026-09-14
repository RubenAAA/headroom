# Rejected: offload-gap levers disproved

- **Status:** closed by measurement (2026-08-18/21)
- **Source:** `docs/notes/proxy-experiments-2026-08.md`


## Detail

*moved from `docs/notes/proxy-experiments-2026-08.md`*

Disproved:

- **The spare fourth breakpoint is worth nothing.** Swept at seven fractions from
  2% to 50% back through history, on both corpora and under both weightings,
  every arm came out byte-identical to the untouched proxy. Every turn writes a
  cache entry at its own tail, so the conversation already carries a ladder of
  readable prefixes and an extra marker lands on a rung that exists. Spending it
  as a 1h anchor deep in history was worth 0.2pp at best, inside the noise
  between fractions, because idle gaps past 5 minutes are 1.0–1.5% of turns.
- **The third tail breakpoint does not survive the marker budget.**
  `tail-breakpoints-3` scored −16.9% subscription on windowgap and looked like
  the best untried idea in the file, but it asked for five markers on 1,133 of
  1,159 requests and the simulator was quietly dropping the earliest to fit. Its
  score was never "three tail markers". Named honestly as `rebalance-1sys-3tail`,
  which asks for exactly four, it scores −16.7% on windowgap against the live
  proxy's −15.1%, and −1.3% on blindguard against −1.3%. The control
  `rebalance-1sys-2tail` shows the split: giving up a system marker costs 0.4pp
  and the third tail marker buys 2.0pp. `cachesim.py` now counts requests over
  `MAX_BREAKPOINTS` and prints `OVER BUDGET`; the comment there had claimed for a
  while that such requests were flagged, and they were not.
- **`pair-back-05` was measuring three levers at once.** It cleared every message
  marker and re-placed two with `ttl: "1h"`. Separated, the tail move carries all
  of it: `shipped-tail` alone scored −3.5% subscription on blindguard against
  `pair-back-05`'s −3.3%.
- **The marker-spreading levers do not replicate.** Backtested on the three older
  two-marker corpora, `spread-wire-02` and `spread-wire-05` are flat to a shade
  worse on capture-beta (1,869 turns) and toolblocks (374), with the 5pp gain
  showing only on msg0 (231 turns). `rebalance-1sys-3tail` is bad everywhere
  there, by 17 to 74 points, and its uncached share goes to 0.0%, which is the
  tell: it rewrites the cache every turn and writes cost more than reads.
- **markercheck settled both, on 446 turns of fresh traffic on windowgap's own
  build.** Subscription weights, `--base forwarded`:

  ```
  claude code                 8,529,901    +0.0%   uncached 0.5%
  live proxy                  7,410,026   -13.1%   uncached 0.4%
  spread-wire-02              7,426,398   -12.9%
  spread-wire-05              7,429,335   -12.9%
  spread-wire-10              7,433,292   -12.9%
  rebalance-1sys-3tail        7,422,759   -13.0%   uncached 0.0%
  ```

  Every arm is worse than the live proxy, on the one corpus captured
  specifically to test them. Four corpora out of five say no gain; windowgap is
  the outlier, not the signal. Both levers are closed. Do not re-open without a
  reason that explains why windowgap differed. Two caveats on the baseline: the
  `live proxy` row is the binary running since 2026-08-18 20:20, which predates
  `e796ee3c`, so rebaseline before re-running these arms; and at 0.4% uncached
  there is no marker slack left to win, which is the likely reason every arm
  costs a little. Superseded in any case by the scaffold breakpoint now placed in
  message 0 (`cache_stabilization/prefix_replay.rs`, `opening_scaffolding_target`),
  which changes where the markers sit.
