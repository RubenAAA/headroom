# Idea: port the upstream output-shaper policy split

- **Status:** open (large; re-diff before starting)
- **Source:** `docs/notes/upstream-port-backlog.md` group A (range
  `42ebbc6c..904bc675`)
- **Summary:** upstream split output-shaping into single-purpose policy modules
  (`output_savings_policy`, `output_turn_policy`, `output_steering`,
  `request_log_redaction_policy`, `memory_query_policy`, `auth_policy`,
  `forwarded_policy`; ~1000 lines). None exist in Rust; `output_shaper.rs`
  predates the split.
- **Next:** `git log --oneline 9af63499..HEAD -- <path>` per file, then port
  policy by policy against the Rust shaper.
