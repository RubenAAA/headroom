# Idea: close the savings-tracker remainder gap

- **Status:** shipped 2026-09-14 — slice #2 per-bucket output rollup ported
  (`RollupEntry.output_tokens_saved_delta` /
  `output_savings_usd_delta`, series CSV columns, rollup↔lifetime
  reconciliation test green). Nothing remains.
- **Source:** `docs/notes/upstream-port-backlog.md` group A
- **Summary:** Python landed savings-sink aggregation + double-count fixes
  (`savings_tracker.py` +394/-26); six local commits through `e920e5a8`
  reworked the Rust tracker since. Unknown overlap.
- **Next:** diff the two evolutions; port only the missing aggregation
  semantics, with the ledger↔tracker reconciliation row green.
- **Update 2026-09-11:** scope correction — the +394/-26 was NOT the audit
  trio (those touched handlers/outcome/analyzer; Rust never had the funnel,
  nothing to port). Shipped slice #1: cache-only/output-only history gating
  per Python `d1258055` (widened `record_request` gate, lifetime
  `cache_read_tokens`/`cache_savings_usd` cumulatives, 4 history fields with
  load arms, `cache_only_turn_appends_history_point` green, ledger lock test
  green). Remaining: slice #2 per-bucket output rollup (dashboard-only value),
  slice #3 tool-schema aggregation (no Rust producer exists — feature build,
  conflicts with keep-layers-separate; likely reject).
- **Update 2026-09-11 (2):** slice #3 premise changed — a Rust tool-schema
  producer now exists (`tool_schema_savings.rs`: `TOOL_SCHEMA_SAVINGS_TAGS`,
  headline totals through tracker / cost / ledger / `/stats`, ledger `$`
  priced on the headline count at the cache-aware rate). Slice #3 counts as
  shipped. Remaining: slice #2 per-bucket output rollup (dashboard-only
  value).
