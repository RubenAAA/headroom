# Learning: the residue was provider-side granularity

- **Source:** `docs/notes/recache-classification.md` (2026-08-26 → 09-02)
- **Claim:** 536 events / 527k tokens with byte-stable forwarded prefixes and
  no tail (worst single turn 9,161 vs 132k+ for real divergences) = the
  provider's read landing a block short of a divergence, or a 5m block ageing
  under a 1h one — provider accounting granularity, not a broken prefix.
  Closed as five named `provider_*` reasons; re-open bar is a five-figure
  single turn.
