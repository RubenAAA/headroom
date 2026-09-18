# Idea: lossless-only mode A/B (`--lossless true`)

- **Status:** open (needs a quiet-moment A/B, not a blind flip)
- **Source:** 2026-09-18 lossless audit. `--lossless` is `false` live
  (`config.rs:1742-1744`, `contrib/headroom-flags.sh:601`), so lossy
  SmartCrusher/Log/Search/Diff arms run. Every other lossless stabilizer is
  already ON (json_minifier, compact_lossless family, boundary-trim + 512B
  floor, holds, tool_order, tail, prefix replay, beta-sticky, ttl_order).
- **Value:** content-lossless, marker-free, deterministic output should be
  *stabler* than lossy over time — fewer byte shapes competing for the same
  prefix — at the cost of smaller per-turn savings.
- **Next:** flip `--lossless true` in a quiet window and settle with a
  depth-binned ledger A/B (first turn dropped, `drift`-only counting per
  `recache-counting-rules.md`): per-turn savings vs recache waste vs
  rework turns. Cost of the experiment itself is a one-time prefix re-key on
  flip. Do NOT combine with any other flag change in the same window.
