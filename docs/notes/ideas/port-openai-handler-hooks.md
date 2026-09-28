# Idea: port the OpenAI handler + cold-prefix hooks delta

- **Status:** open, narrowed 2026-09-28 — 2 items left of 110 commits, neither on traffic seen here; the
  rest is in Rust or Python-only.
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `proxy/handlers/openai.py` (+2451/-687, 7 commits) —
  model-aware cold-prefix hooks (Kimi/GLM reasoning compaction), streaming
  fixes. Rust has `handlers/` but not the hook logic.
- **Next:** re-diff; port hook-by-hook with parity fixtures.

## Re-diff 2026-09-28 (`42ebbc6c..964671d8`)

The range holds 110 commits touching `openai.py` (+4750/-952), not 7; the
old figure matched `42ebbc6c..904bc675`. Plain `git log` hides `cb8f4b64`,
the cold-prefix commit, through history simplification.

Already in Rust: 17, including CCR on Responses, turn hooks, read
protection, `additional_tools` lift, chat tool-description compaction.
N/A: 17 (`/v1/compress` sidecar, `route_advice`, TTL learner, Python prefix
tracker, response cache). About 72 more (Codex WS, memory tools, telemetry)
were grouped rather than checked one by one; spot checks found them in Rust
or Python-only.

Left to port:

1. `cb8f4b64` (a), `HEADROOM_THINKING_COMPACT`: shrink Kimi
   `reasoning_content` and GLM/DeepSeek `<think>` in older chat turns. The
   core function exists (`thinking_compactor.rs:366`,
   `compact_reasoning_openai_chat`) and nothing calls it. The warm path is
   deterministic and cache-safe; the cold drop needs a per-session idle
   clock chat does not have. Ship warm only, default off.
2. `cb8f4b64` (b), `HEADROOM_COLD_RECOMPACT` on chat: Anthropic-only today
   (`forward/stages.rs:198-243`). Needs the same idle clock.
3. Dropped 2026-09-28: `09c66ac2` + `dbe2558c` (apply the 512-byte floor
   to the total of small Codex Responses tool outputs, not each one).
   Measured on 3,861 captured Responses bodies: in the current frame, the
   only outputs Rust compresses, under-floor items are 238KB of 15.9MB
   (1.49%), and only 28 of 3,410 requests have small outputs that sum past
   512 bytes. The gain is a fraction of that 1.49% after compression and
   CCR markers, against a batching envelope with its own parse and split
   paths. Upstream wrote it for sessions of hundreds of tiny outputs; Codex
   here sends few, and large.

Chat/Responses output steering (`1b8c11eb`, `71cbb6aa`) moves to
`port-output-shaper-policies.md`. Items 1 and 2 serve Kimi/GLM/DeepSeek on
`/v1/chat/completions`; none of those families is in `pricing.rs`, and the
maintainer's traffic has none.
