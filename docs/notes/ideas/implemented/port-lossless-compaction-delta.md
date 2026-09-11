# Idea: port the upstream lossless-compaction delta

- **Status:** done 2026-09-11 (superseded — bulk port already in tree, only kill-switch remained)
- **Close-out:** the +180 folds (`737b3321` config fold, `7dc9a978`
  dir-prefix fold) were already in `lossless_compaction.rs` with byte-parity
  tests green. Ported the one remaining delta: `HEADROOM_LOSSLESS_COMPACTION`
  kill-switch (`LOSSLESS_COMPACTION_ENV` + per-call read in `compact_lossless`,
  3 predicate/premise tests, all 30 module tests green).
- **Source:** `docs/notes/upstream-port-backlog.md` group A
- **Summary:** `transforms/lossless_compaction.py` (+180/-5, shared-prefix
  folding in grep search etc.) is Python-only; the Rust module exists but was
  untouched in range.
- **Next:** re-diff `lossless_compaction.py` vs
  `crates/headroom-core/src/transforms/lossless_compaction.rs`, port the delta.
