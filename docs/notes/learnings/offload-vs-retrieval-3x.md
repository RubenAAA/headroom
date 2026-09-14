# Learning: offload beats retrieval ~3x; TTL healthy

- **Source:** `docs/notes/recache-classification.md` (2026-09-02 audit)
- **Claim:** offload books 154M saved/day (≈15.4M read-equivalents) vs 441 continuations costing ~5.6M — net ~3x, so `--ctx-offload-min-bytes` stays. Zero recaches past 60 min gaps: `--force-1h-cache-ttl` works.


## Detail

*moved from `docs/notes/recache-classification.md`*

### TTL, offload, and two things no longer worth watching

- **TTL.** Zero recaches after a gap longer than 60 minutes; 77 turns recached
  after gaps of 5 to 60 minutes. `--force-1h-cache-ttl` is doing its job.
- **Offload against retrieval.** Offload books 154M `tokens_saved` a day, worth
  15.4M read-equivalents at 0.10. Retrieval costs 441 CCR continuation requests
  — 29.1M cache reads, 0.9M writes, 0.14M output, about 5.6M equivalents. Net
  positive by roughly 3x. Lowering `--ctx-offload-min-bytes` would offload more
  and retrieve more often, so it stays where it is.
- **`<total_tokens>N tokens left</total_tokens>`.** This block drove 45% of
  invalidated bytes in the August captures. It appears in 1 of the 100 newest
  stored prefixes. Not a live problem.
- **Utilization sampler.** `bench/fit_weights.py sample` last wrote
  `~/hr-usage-samples.jsonl` on 2026-08-18. The fitted weights — read 0.10,
  write_1h 1.45 with a 1.0–2.0 band, R² 0.33 — rest on that single 28-hour
  window and have been quoted since as if they were settled. Sampler restarted
  2026-09-02.
