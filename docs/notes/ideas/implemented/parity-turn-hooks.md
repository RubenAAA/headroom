# Implemented: turn-hook extension point (Rust port)

- **Status:** done 2026-07-10 (`turn_hooks.rs` + registry + runners + 9 tests;
  proxy suite green)
- **Source:** `docs/notes/rust-parity-gaps.md` §1 (Python `ec950f7e` + #1903)
- **Summary:** `TurnContext`/`TurnHook`/`register`/`run_request|response_hooks`,
  wired at Anthropic pre-send + post-response seams (OpenAI chat covered by the
  generic post-response wiring); no-op when registry empty, failing hooks
  logged-and-skipped. Dashboard surfacing N/A (no Rust dashboard).
- **Note:** an earlier doc pass wrongly claimed this already existed — verified
  by `git show --stat`, the merged commit was Python-only. Check file lists,
  not log presence.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 1. Turn hooks extension — DONE (2026-07-10)

Ported: `crates/headroom-proxy/src/turn_hooks.rs` (core module + registry +
runners + 9 tests), wired at Anthropic/OpenAI pre-send and post-response
seams in `proxy.rs` (byte-identical no-op when no hook registered). OpenAI
chat-completions seam covered by the generic post-response wiring rather
than a dedicated seam (no separate OpenAI CCR path existed to hang a
dedicated seam off). Dashboard/tool-schema-savings aggregate from #1896
still N/A (no Rust dashboard). Full proxy suite green (977+ tests).

<details><summary>Original scoping notes</summary>

**CORRECTION (2026-07-10): an earlier pass of this doc wrongly claimed the
extension point already existed in Rust. It does not — verified via
`grep -rln "turn_hook\|TurnHook\|TurnContext" crates/` returning nothing at
all.** `ec950f7e` (#1891, "add turn-hook extension point for buffered model
turns") — which IS merged into `local/npu-integrated` — is **Python-only**
(`git show ec950f7e --stat`: 4 files, all under `headroom/`:
`headroom/proxy/turn_hooks.py`, `headroom/proxy/handlers/anthropic.py`,
`headroom/proxy/handlers/openai.py`, `tests/test_turn_hooks.py`). The commit
being in our git history just means the *Python* commit is merged, not that
anything was ported to Rust. This whole feature is unstarted in Rust. Always
verify file lists with `git show --stat`, not just commit presence in `git log`.

- Python source of truth, two commits:
  - `ec950f7e` (#1891): `headroom/proxy/turn_hooks.py` — `TurnContext`, the
    `TurnHook` protocol (`on_request(ctx)` / `on_response(ctx, response,
    call_model)`), a module-level registry (`register_turn_hook` /
    `registered_turn_hooks` / `clear_turn_hooks`), and runners
    (`run_request_hooks` / `run_response_hooks`) — inert/no-op when the
    registry is empty, never raises (a failing hook is logged and skipped).
    Wired at two seams: Anthropic pre-send + CCR response seam
    (`handlers/anthropic.py`), and OpenAI Responses tool-shaping point + CCR
    response seam (`handlers/openai.py`).
  - `c9217856`/`281d8802` (#1903/#1896): extends wiring to a THIRD seam — the
    OpenAI `/v1/chat/completions` direct path — plus dashboard surfacing
    (`savings.by_layer.tool_search`, a "Tool-Schema Deferral" card). No
    Rust dashboard template exists today, so that part doesn't apply yet.
- Scope: full net-new Rust port — the hook trait/registry module itself
  (probably `crates/headroom-proxy/src/turn_hooks.rs` or similar), the
  re-drive `call_model` capability (reuses whatever internal re-call path
  the CCR handler already uses — check `crate::compression::ctx_offload` /
  the CCR response-handling call sites for a reusable primitive), and wiring
  at 3 seams: Anthropic pre-send, OpenAI Responses tool-shaping, OpenAI
  chat-completions direct path. This is meaningfully bigger than the other
  3 items below — treat as its own multi-step task, not a quick port.

</details>
