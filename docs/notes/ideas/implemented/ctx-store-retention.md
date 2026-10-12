# Idea: bound the ctx store on disk

- **Status:** done 2026-10-12 (`0a84bf3ee`). A `ctx-retention` thread sweeps
  `sessions/*.db` and `content/*.db` at proxy start and every 24h: rows older
  than 30 days go, then each file is trimmed to 90% of a 1 GiB cap, then
  VACUUM runs when half its pages are free. A quiet conversation keeps its
  chain and injection until the conversation itself is 30 days old. Log
  events: `ctx_retention_pruned`, `ctx_retention_failed`.
- **Source:** disk-full cleanup, 2026-10-12.
- **Summary:** nothing pruned the store; `~/.claude-work/context-mode` had
  reached 34G. After the first live sweep it held 4.3G, with no failures.
- **Measured on copies:** sessions DB 1.4GB → 20MB in 7s; content DB
  2.0GB → 805MB in 136s. Without an FTS `optimize` before sizing, the
  content pass overshot to 0.4GB, so it runs one when the file is over the
  cap or a tenth of its sources went.
- **Code:** `crates/headroom-core/src/ctx/retention.rs` (pruning),
  `crates/headroom-proxy/src/ctx/retention.rs` (thread), wired at the top
  of `serve()`.
