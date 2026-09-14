# Learning: check invocation telemetry before porting

- **Source:** `docs/notes/rust-dev.md` ("picking the next port")
- **Claim:** `/stats` `compressions_by_strategy` / `pipeline_timing` /
  `tokens_saved_by_strategy` prioritize ports: zero-invocation strategies
  defer, hot-path strategies port regardless of LOC. The standing practice
  behind the audit-cleanup sequencing.
