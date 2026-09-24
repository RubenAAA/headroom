# Idea: carry exact cache-read cost in history rollups

- **Status:** open (follow-up to a port, needs design)
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
