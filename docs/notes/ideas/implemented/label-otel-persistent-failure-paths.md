# Implemented: ledger failure paths labelled by provider

- **Status:** shipped 2026-09-24 (uncommitted working tree)
- **Source:** port of upstream `5ff4ea1e` (metrics labels, Sept 2026
  session). Only the Prometheus counters were ported first
  (`headroom_requests_rate_limited_total{source}`,
  `headroom_requests_failed_total{provider}` in `proxy_counters.rs`).
- **Summary:** `RequestsState` (`crates/headroom-core/src/persistent_metrics.rs`)
  gains `failed_by_provider` / `rate_limited_by_provider` CountMaps, same
  32-entry cap as `by_provider`, persisted and normalized on load.
  Provider threaded through `SavingsTracker::record_failed_work` (new
  `FailedWorkRecord.provider`) and new `SavingsTracker::record_rate_limited`,
  called from `ProxyOutcomeSink` (`proxy.rs`) and `CodexWsOutcomeSink`
  (`websocket_codex.rs`) with `outcome.provider`. Shape tests updated,
  breakdown assertions added; core lib (2201) + proxy lib (2488) green,
  clippy/fmt clean.
- **Not ported:** dashboard (lives elsewhere, skipped), OTel mirrors
  (unused — debugging goes through logs), per the operator decision
  2026-09-24.
