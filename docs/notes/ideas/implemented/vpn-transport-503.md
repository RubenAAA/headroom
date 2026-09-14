# Implemented: transport exhaustion answers 503 + Retry-After

- **Status:** shipped (proxy-transient signal, not provider 5xx)
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (wifi/host flaps)
- **Summary:** `503 + Retry-After: 2` with `x-headroom-retryable:
  transport-exhausted` so stock client retry fires instead of stalling the
  session. Non-transport failures stay 502 without `Retry-After`.
