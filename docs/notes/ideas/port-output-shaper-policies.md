# Idea: port the upstream output-shaper policy split

- **Status:** open, narrowed 2026-09-30 — the 9 pure file splits need no Rust
  move; steering for Responses and chat shipped; only `/stats output_reduction`
  is left.
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

The module split itself needs no port: the nine file-split commits move
Python code between Python files. Rust has no counterpart files to move, and
`output_shaper.rs` (about 1,000 lines) already holds the same functions, so a
split would add files without changing behaviour. Skipped. Already in Rust: `3e976712`,
`1390d897` (`47666ac8`), `c0292984` (`6cb19b93`), `75105e23`, and
`f542b704` in part. N/A: rollout-gated enable (`3077ac81`), Grok Build
(`420dc907`).

Left:

1. Done 2026-09-28: `4e5a67a3` + rest of `f542b704`. `extract_user_query`
   (`memory/handler.rs`) now skips `<system-reminder>` blocks and joins the
   rest with `\n`; before, it returned the first text block, usually a
   reminder, so memory search ran on boilerplate. `latest_user_query`
   (`ccr_expansion.rs`), which picks proactive CCR expansions, had the same
   flaw and got the same filter. Neither saves anything on the live flags:
   `HEADROOM_MEMORY_MODE=tool` returns before the query is built (30,412 of
   30,412 searches in the current log skipped as `mode_is_tool`), and
   `--ccr-proactive-expansion false` gates out the only reader of the CCR
   query. They matter only if `auto_tail` or expansion come back.
2. Done 2026-09-30: `71cbb6aa` + `63f74aa3` (Responses `instructions`
   tail, HTTP and WS) and `1b8c11eb` (chat: last system/developer message,
   or a new system message when none exists). `shape_openai_with_holdout`
   (`output_shaper.rs`) gives both formats the same arm assignment, stratum
   and conversation labels as the Anthropic path. `run_openai_output_shaper`
   (`proxy/forward/stages.rs`) runs after the ctx gate for
   `/v1/chat/completions` and `/v1/responses`; `shape_create_frame`
   (`websocket_codex.rs`) runs on the first frame and every later
   `response.create`, before compression, in both envelope shapes. Unlike
   the Anthropic path, the OpenAI injectors replace a block whose level
   changed instead of stacking a second one, as upstream does. Same gates as
   the Anthropic shaper: `--output-shaper` on, and compression on (with it
   off the body streams through unbuffered). Idle on the live flags.
   Differences from upstream:
   - The Responses conversation key prefers a stable client id
     (`prompt_cache_key`, which Codex sets to its session id, then
     `conversation`, `session_id`, `thread_id` and their `metadata` forms)
     and falls back to upstream's model plus first user text. Upstream's key
     collides across sessions that open with the same text, the flaw the
     Claude path already avoids.
   - WS frames are steered and logged (`output_shaper_arm`), but their labels
     do not reach the savings ledger; there is no per-frame outcome to carry
     them on.
   - Chat turns classify as new ask (trailing user), mechanical (trailing
     tool message) or unknown; chat has no error flag to read.

   Tests: `suites/cache/integration_output_shaper_openai.rs` (byte stability
   across turns, no system message or instructions, holdout split by
   `prompt_cache_key`, shaper off) and
   `response_create_frames_get_verbosity_steering` in
   `integration_codex_ws.rs` (both envelopes, later frames, control arm
   byte-equal).
3. Dropped 2026-09-28: `apply_verbosity_steering` appends a second block
   on a level change (`output_shaper.rs`) where Python replaces in place.
   The branch never runs: the proxy adds the block to the forwarded body
   and the client never sends it back (0 of 1,600 captured inbound bodies
   carry `<headroom_output_shaping>`). A level change rewrites the system
   prompt, and so costs a recache, with or without the fix. The shaper is
   also off live (`--output-shaper` defaults false and the flag file does
   not set it; 0 of 1,600 outbound bodies carry the block).
4. Done 2026-09-28: `53631adb` (holdout counts conversations) plus the
   wiring it lacked. `--output-holdout` / `HEADROOM_OUTPUT_HOLDOUT` sets
   the control share; `shape_with_holdout` (`output_shaper.rs`) assigns
   the arm per conversation, steers only treatment, and labels every
   request with arm, stratum and conversation for the savings ledger, on
   both the Claude and routed paths. The conversation is Claude Code's
   session id, not upstream's first-text-block key: that block is a
   shared `<system-reminder>`, and with upstream's key all ten sessions
   in `integration_output_holdout.rs` land in one arm. Idle until
   `--output-shaper` is on; the trial is to run it with
   `--output-holdout 0.5` and read `headroom` output savings plus
   `text_chars` per `end_turn` by arm (`output_shaper_arm` log event).

Also belongs here from the server re-diff: `/stats output_reduction`
(`074f0ae9`), which needs a shaper ledger first.
