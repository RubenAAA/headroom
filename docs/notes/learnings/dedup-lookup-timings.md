# Learning: unindexed probe 23.4 ms, indexed 0.003 ms, index build 73 s

- **Source:** `docs/ctx-sessions-dedup.md` (measured on a 9.7 GB copy)
- **Claim:** the unindexed lookup isn't a full scan (`idx_session_events_type`
  narrows first) but grows with the conversation's own history — the wrong
  shape, hence skip-until-indexed. DELETE-first is what makes the index worth
  building.
