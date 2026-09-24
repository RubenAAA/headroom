# Zen Retry-After guidance

- **Source:** OpenCode Zen support reply received 2026-09-23; historical
  observation in [`retry-after-cap-fix.md`](../ideas/implemented/retry-after-cap-fix.md)
  and the 2026-09-14 notes in `zen_hold.rs`.
- **Claim:** OpenCode recommends following `Retry-After` when returned;
  otherwise use exponential backoff with jitter. A usable value should guide
  the next hold probe on that egress.
- **Guard:** Zen previously sent a constant `Retry-After` near 53,568 seconds
  for hours while the same route recovered much sooner. Ignore values beyond
  the configured probe cap and fall back to jittered backoff; do not turn this
  observed stale value into a multi-hour in-request sleep.
- **Scope:** routed Zen 429 hold only. Other retry paths retain their existing
  cap and response behavior.
