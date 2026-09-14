# Learning: the concurrency flag earned its name

- **Source:** `docs/notes/recache-classification.md` (taken on trust, then timed)
- **Claim:** zero of 208 flagged events stands >60 s from its nearest sibling
  (control bucket has a 5% tail to ten minutes) — no stale-pending-entry
  failure mode in practice. Overlap raises re-cache rate 7.7× (17.16% vs
  2.23% over 29,432 turns). Small bucket (150k tokens), honest label, nothing
  to do.
