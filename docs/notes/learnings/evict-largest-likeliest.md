# Learning: evict the largest streams, not the stalest

- **Source:** `docs/notes/recache-classification.md` (eviction order, 2026-08-23)
- **Claim:** return probability rises monotonically with cached size across
  every bucket (0–10k: 0% ≥2 turns; 120k+: 92%, median 13 turns; log-log
  r = 0.54 over 1,080 streams). Budgets are counted in messages and tokens
  track messages, so value-per-budget belongs to large streams; dropping a
  small one costs ~nothing. Recency is actively wrong (blocked parent ages
  like a finished one).
