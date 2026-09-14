# Learning: ranking tests miss threshold bugs

- **Source:** `docs/notes/proxy-followups.md` §6 (RRF bug, `3cee05d1`)
- **Claim:** presence + ordering assertions passed a fused score capped at
  0.032 against a 0.3 floor — two days of zero injection, all tests green.
  Fixed by `ideas/threshold-tests-absolute-scores.md`: assert the number.


## RRF root cause

*moved from `docs/notes/proxy-experiments-2026-08.md`*

- **The memory layer was initialised and retrieving nothing, because the score
  could never reach the floor.** Across 1,929 captured forwarded bodies there
  were zero `## Relevant Memories` blocks, while 795 `<session_recall>` blocks
  went out over the same plumbing. `CtxStore::search` fuses two ranked lists and
  overwrites `SearchHit::rank` with the negated RRF score, and with `RRF_K = 60`
  the best obtainable hit is `2/(RRF_K + 1)` = 0.033, which through
  `|rank|/(1+|rank|)` caps the output at 0.032 against a `min_similarity` floor
  of 0.3. No result could pass, for any query, at any threshold above 0.032. The
  live log carried 7,265 consecutive `all_below_min_similarity` events, each
  reporting ten results found and not one success. Fixed in `3cee05d1` by scaling
  the fused score by `RRF_K + 1` before the squash: a single-list leader now
  scores 0.5, a both-list leader 0.67, and roughly the first fifty fused
  positions clear 0.3. The trap worth keeping: `SearchHit::rank` is named for
  BM25 and carries a fused score, so testing the FTS index directly returns
  ranks of −3.6 to −9.7 and tells you nothing about what the search path
  produces. That is exactly what hid this for two days.
