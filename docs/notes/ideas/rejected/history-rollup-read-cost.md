# Idea: carry exact cache-read cost in history rollups

- **Status:** rejected 2026-09-28 — our rollups have no read leg to
  misprice, and every model in our traffic reads at 0.1× or is free.
- **Source:** upstream `1cb779e1` (Sept 2026), folded into the
  `89a58fd1` cache-mix pricing port. Upstream's savings-tracker
  rollups now carry `cache_read_cost_usd_delta` per bucket, priced per
  checkpoint with the same function that built `total_input_cost_usd`
  — because read discount is not a fixed multiple of read cost
  (0.1x on most models, 0.05x/0.025x on others), so "discount / 9"
  misprices exactly the steepest-discount models. Unknown-model
  checkpoints report the bucket cost as unknown rather than guessed.
- **Value:** lets consumers take reads out of the input bill without
  re-deriving a cost that isn't derivable. Matters most on
  steep-discount models.
- **Next:** check whether our Rust rollup path (`savings_tracker.rs`,
  per-checkpoint cost via `compression_savings_cost_usd`) can carry a
  read-cost leg, or whether the mixed-rate pricing already subsumes
  it. Measure on a steep-discount model before building.

## Findings 2026-09-28

`build_rollup` (`savings_tracker.rs:1565`) carries compression savings,
input tokens and input cost per bucket, and nothing about cache reads, so no
consumer here derives a read cost from a fixed ratio. Nothing in `crates/`
or `contrib/` reads a per-bucket read cost either. The models that billed
over 09-18→28 were Claude (opus-5, opus-5-5, sonnet-5 at 0.1× input),
gpt-6-luna (0.1×) and muse-spark (free). None of the steep-discount models
upstream fixed this for shows up. Re-open if a consumer needs the input bill
without reads, or a model off 0.1× starts carrying traffic.
