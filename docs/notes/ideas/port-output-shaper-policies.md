# Idea: port the upstream output-shaper policy split

- **Status:** open, narrowed 2026-09-28 — 9 of 20 commits are pure file
  splits; 3 behaviour items left, 1 blocked.
- **Source:** `docs/notes/upstream-port-backlog.md` group A (range
  `42ebbc6c..904bc675`)
- **Summary:** upstream split output-shaping into single-purpose policy modules
  (`output_savings_policy`, `output_turn_policy`, `output_steering`,
  `request_log_redaction_policy`, `memory_query_policy`, `auth_policy`,
  `forwarded_policy`; ~1000 lines). None exist in Rust; `output_shaper.rs`
  predates the split.
- **Next:** `git log --oneline 9af63499..HEAD -- <path>` per file, then port
  policy by policy against the Rust shaper.

## Re-diff 2026-09-28 (`42ebbc6c..964671d8`, 20 commits)

The module split itself needs no port. Already in Rust: `3e976712`,
`1390d897` (`47666ac8`), `c0292984` (`6cb19b93`), `75105e23`, and
`f542b704` in part. N/A: rollout-gated enable (`3077ac81`), Grok Build
(`420dc907`).

Left:

1. Done 2026-09-28: `4e5a67a3` + rest of `f542b704`. `extract_user_query`
   (`memory/handler.rs`) now skips `<system-reminder>` blocks and joins the
   rest with `\n`; before, it returned the first text block, usually a
   reminder, so memory search ran on boilerplate. `latest_user_query`
   (`ccr_expansion.rs`), which picks proactive CCR expansions, had the same
   flaw and got the same filter.
2. `71cbb6aa` + `63f74aa3`: steering for Responses (`instructions` tail,
   HTTP and WS); `1b8c11eb`: steering for chat (last system/developer
   message). The shaper runs on Anthropic bodies only
   (`forward/ctx.rs:619-622`). New bytes in cached prefixes, so ship with
   byte-stability tests.
3. Not from the range: `apply_verbosity_steering` appends a second block
   when the level changes (`output_shaper.rs:197-206`) where Python
   replaces in place. The level is fixed at startup, so it bites only after
   a restart with a new level, and then rewrites every open conversation's
   system prefix. Verified.
4. Blocked: `53631adb` (holdout counts conversations). `assign_arm` has no
   production caller, so there is no holdout to fix.

Also belongs here from the server re-diff: `/stats output_reduction`
(`074f0ae9`), which needs a shaper ledger first.
