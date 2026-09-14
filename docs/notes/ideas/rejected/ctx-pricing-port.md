# Rejected: porting the pricing catalog

- **Status:** do not port (heavy overlap)
- **Source:** `docs/context-mode-integration-analysis.md` §2 (tier 2.6)
- **Summary:** 61 models × 4 buckets overlaps `headroom/pricing/*` heavily
  (which resolves via litellm, and nulls unknown models instead of mispricing).
  Do not port.
