# Idea: verify /stats serves the ledger failure-provider splits

- **Status:** open (verify on next read, no code expected)
- **Source:** split 2026-09-24 from `label-otel-persistent-failure-paths.md`
  (port of upstream `5ff4ea1e`). Dashboard skipped, OTel skipped (unused —
  debugging goes through logs), persistent ledger ported (see
  `implemented/label-otel-persistent-failure-paths.md`).
- **Value:** confirm the labelled splits are visible where operators read
  them. Cost is one `/stats` read, not a migration.
- **Next:** on the next `/stats` read, check `failed_by_provider` and
  `rate_limited_by_provider` appear with per-provider counts. If yes,
  close this file; if the handler drops unknown keys, fix it there
  (`handlers/stats.rs`) and close.
