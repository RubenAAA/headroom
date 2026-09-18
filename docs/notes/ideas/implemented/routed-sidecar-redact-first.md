# Implemented: routed sidecar runs under redaction (redact-first)

- **Status:** landed in tree 2026-09-18, uncommitted (needs rebuild+restart)
- **Source:** operator question; the redact-first machinery was committed but
  unreachable behind the `redact_sensitive` early return in
  `try_routed_sidecar` (`sidecar_routed_skipped_redacted`, 431 skips in log).
- **Change:** removed the gate (`routed/sidecar.rs`). The path below it was
  already correct: session key derived from the unshrunk body (matches the
  next real turn's key), shrunk body redacted in place, placeholders
  restored at the edge for both stream and buffered arms. The old fear —
  raw client text reaching a third-party upstream past the redaction stage
  — is handled by redacting before the route instead of skipping it.
- **Test:** `redaction_on_still_routes_the_sidecar` rewritten from a
  shape assertion (which passed for both skip and fallback) to a wire
  capture: local listener records the routed POST, asserts the AWS-key
  secret absent + `__HR_` placeholder present. Negative control verified:
  with the gate temporarily restored the test fails (nothing captured).
- **Watch live:** `sidecar_routed_attempt` lines should appear where
  `sidecar_routed_skipped_redacted` did; any `sidecar_routed_fallback`
  with a secret-shaped complaint means the shrink admitted something new.
