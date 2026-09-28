# Idea: close the content-router remainder gap

- **Status:** open, narrowed 2026-09-28 — 5 of 54 commits left, most small.
- **Source:** `docs/notes/upstream-port-backlog.md` group A
- **Summary:** Python rewrote router dispatch (`content_router.py` +1518/-313);
  local `7dd551ac` + `e539a3b0` closed part (router/gemini fixes, PHP, tool
  exclusion). What remains is the decision-logic delta vs Rust
  `content_router.rs`.
- **Next:** re-diff and port only what's left.

## Re-diff 2026-09-28 (`42ebbc6c..964671d8`, 54 commits)

Most of Rust `content_router.rs` is off the serving path: requests go
through `live_zone/` (planner + dispatch), and `apply_strategy`, the
compressor registry, the mixed-content split and the net-cost gate have no
production caller. Judged against the serving path: 25 already there, 23
N/A, 6 left, all changing forwarded bytes.

- Done 2026-09-28: `aceff2ea`. Text in a user message stays verbatim in
  both Anthropic dispatchers (`ExclusionReason::PromptText`), unless the
  message answers a fenced shell command in the assistant turn before it.
  Upstream protects only the opening and newest user turns and relies on
  prefix replay to keep older ones as sent; protecting every user turn
  gives the same result without depending on replay, and stays
  deterministic under `all_messages`. The real target was not typed
  prompts: session continuation summaries were cut from 20-28KB to 3-4KB,
  task statement included. On 90 captured bodies opening with a summary,
  old code rewrote message 0 in 4 and the new code in none; the bodies
  lose 41,676 bytes of saving between them.
- `ad56dd38`: the Kompress size gate uses chars/4
  (`content_router.rs:2212`, called from `live_zone/dispatch.rs:342`);
  upstream uses the token estimator. A few lines.
- `a97b8241`: unwrap Hermes `tool_call` wrappers before tool-exclusion
  checks (`live_zone/planner.rs:138-145`). Hermes clients only.
- `e9000863`: fail-open deadline on one cache-miss compression. Do it with
  `7f2766ca` from `upstream-triage-0.39.md`, not separately.
- `10e48292`: widen cross-turn dedup to list-content tool results and
  `function` role (`cross_turn_dedup.rs:463-482`). Only if dedup is ever on
  by default.
- `57bf720d`: compress embedded JSON spans. A new feature that changes
  bytes widely; a design decision, not parity.
