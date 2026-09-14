# Learning: saved dollars need cache placement

- **Source:** `docs/notes/proxy-experiments-closures.md` (item 10, 2026-08-12)
- **Claim:** pricing every saved token at fresh-input rates overstated dollars
  1.30× (333 events: $13.88 vs $10.64; post-restart sample 1.63–1.78×). Both
  durable dollar paths now use per-turn placement (`cost_basis` on ledger
  rows); legacy rows without it keep the old assumption and can't be repriced.
  Token counts and denominators unchanged.


## Pricing exclusion

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **Pricing was ruled out as a source of the gap.** `pricing.rs` had Opus 5 at
  the retired Opus 4.1 rates, $15/$1.50 per MTok against the real $5/$0.50, a 3x
  overstatement corrected in `85c32900`. It inflated the savings ledger but
  cannot touch a cachesim comparison: both sides are weighted token counts
  relative to fresh input = 1.0, and `bench/cachesim.py` contains no dollar
  arithmetic at all.
