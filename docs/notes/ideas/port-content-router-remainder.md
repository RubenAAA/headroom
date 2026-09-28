# Idea: close the content-router remainder gap

- **Status:** open, narrowed 2026-09-28 — 6 of 54 commits left, most small.
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

- `aceff2ea`: keep the caller's own prompt text verbatim. The live zone
  treats user `text` blocks as compressible (`live_zone/planner.rs:359`).
  Measured on netvalue: 2 of 288 turns with typed text were rewritten, both
  15–18KB messages relayed from peer sessions, none a typed prompt. Small but
  wrong; port first.
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
