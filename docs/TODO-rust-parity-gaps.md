# TODO: Rust/Python parity gaps (larger feature ports)

Tracking doc for parity work identified while auditing `42ebbc6c..origin/main`
(40 commits, 23 Python-only with no corresponding Rust change). The small
correctness-bug batch (status_code/5xx accounting, x-headroom-base-url,
non-streaming cache metrics) is being ported directly. The items below are
larger, standalone features — deliberately scoped out of that batch and
tracked here so the thread isn't lost.

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

## 2. CCR retrieve-tool interception for OpenAI (chat + Responses) — DONE (2026-07-10)

Turned out bigger than the audit item: Rust's `handle_ccr_response` in
`proxy.rs` was hardcoded to `"anthropic"` only — OpenAI chat-completions AND
Responses both got zero CCR interception at the HTTP layer, despite
`response_handler.rs` already having a working (but never-invoked)
`"openai"` provider branch. Ported: added `"openai_responses"` provider
branch to `headroom-core/src/ccr/response_handler.rs` +
`tool_injection.rs` (mirroring Python `62cd3072` exactly), generalized
`handle_ccr_response` to take a `provider: &str` param with an
`extend_or_push` helper covering all 3 providers' sentinel conventions, and
replaced the Anthropic-only gate with a 3-way path-based dispatch covering
`/v1/messages`, `/v1/chat/completions`, and `/v1/responses`. New wiremock
integration test proves OpenAI Responses `headroom_retrieve` interception +
continuation. Full suite green (core ccr: 182 tests, proxy: 1248 tests).

<details><summary>Original scoping notes</summary>

- Python: `62cd3072` (#1898), ~243 lines in `headroom/proxy/handlers/openai.py`.
  Wires CCR (context-compression-retrieval) `headroom_retrieve` tool-call
  interception into the `/v1/responses` handler, so a retrieval marker the
  model emits gets resolved server-side instead of leaking to the client.
- Rust: `crates/headroom-proxy/src/handlers/responses.rs` has zero CCR/
  retrieve-tool wiring today (confirmed via repo-wide grep, including all
  unmerged branches — `fix/ccr-retrieve-full-only` looked promising by name
  but doesn't touch `responses.rs` at all).
- Scope: needs the same interception pattern the Anthropic buffered path
  already has (`crate::compression::ctx_offload`, CCR store lookups) ported
  into the Responses handler's request/response cycle.

</details>

## 3. Audit-safe compression mode (SmartCrusher) — DONE (2026-07-10)

Ported to `crates/headroom-core/src/transforms/smart_crusher/{config.rs,crusher.rs}`:
3 opt-in config fields (default off, byte-identical for non-opt-in callers),
5 helper primitives (canon/scan/splice/verify), wired into
`SmartCrusher::smart_crush_content` (the path `apply()` actually calls for
real tool-output compression). Simpler than Python: Rust's crush pipeline
emits no CCR markers yet, so only the statistical row-drop case needed
guarding, not the marker-hidden case. Invalid regex patterns panic at
construction (matches Python's `raise ValueError`; every SmartCrusher
constructor is infallible, so `Result` threading was out of scope). 335
smart_crusher tests pass (7 new), full core (1637) + proxy (979+) suites
green.

<details><summary>Original scoping notes</summary>

- Python: `bb112dd1` (#1899), new ~258-line module
  `headroom/transforms/smart_crusher.py` additions: `audit_safe` config,
  `protected_patterns` (regex/marker matching), fail-closed verification so
  compliance-relevant content (audit markers, leakage flags) can't be
  silently dropped or rehidden by compression.
- Rust: `crates/headroom-core/src/transforms/smart_crusher/crusher.rs` and
  `config.rs` have no `audit_safe`/`protected_pattern`/`fail_closed` concept
  at all (confirmed via grep across every branch in the repo, including the
  several branches with "audit" in the name — those are about an unrelated
  CCR/"toin" audit concept, not this feature).
- Scope: net-new feature — `SmartCrusherConfig.audit_safe` /
  `protected_patterns` / `fail_closed_on_protected_loss` fields, plus
  scan/splice/verify logic in the crusher's compression path.

</details>

## 4. Bedrock per-user application-inference-profile ARN pinning

- Python: second half of `33c7f6cd` (#1795) — `HEADROOM_BEDROCK_MODEL_MAP`
  env-driven override letting operators pin specific per-user Bedrock
  application-inference-profile ARNs (for cost attribution).
- Rust: the *other* half of this commit (the `global.*` inference-profile
  prefix normalization) is already correctly ported and tested
  (`crates/headroom-proxy/src/bedrock/vendor.rs:16`, `GEO_PREFIXES`). Only
  the `HEADROOM_BEDROCK_MODEL_MAP` per-user ARN override is missing — this
  is a genuinely new operator-facing feature, not a bug fix, so lower
  priority than the others above.

## Notes on methodology (for whoever picks this up)

- Cross-checked every item against **all** branches in the repo (not just
  `main`/`local/npu-integrated`) before concluding "unstarted" — several
  branch names suggested existing work (`*audit*`, `fix/ccr-retrieve-full-only`,
  `tejas/turn-hooks-extension`) but turned out to be false leads (stale
  branches, unrelated features, or Python-only commits already merged
  elsewhere). Always verify with `git grep <feature-marker-string> <ref> --
  '*.rs'` across `git for-each-ref` before assuming something needs
  building from scratch.
- The 5th correctness item from the original triage (`55efb1c7`, OpenAI tool
  observations mutable in cache mode) had conflicting triage verdicts (C vs
  A) — needs a dedicated read of `crates/headroom-proxy/src/cache_stabilization/prefix_replay.rs`
  and `live_zone_openai.rs` before deciding whether it's a real gap.
</content>
