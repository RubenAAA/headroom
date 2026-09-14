# Learning: pair turns by stream, never by time order

- **Source:** `docs/notes/recache-classification.md` (H-race, refuted as defect)
- **Claim:** 106 of 143 "previous write never read" pairs overlapped in time
  (53.7% vs 0.8% base rate, 67× enrichment) — parallel subagents under one
  conversation key, where time-order pairing is invalid and most of the
  apparent 304k loss is artifact. Any log query pairing by
  (conversation, time) is wrong wherever subagents run.
