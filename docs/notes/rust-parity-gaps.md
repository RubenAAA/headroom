# TODO: Rust/Python parity gaps (larger feature ports)

> Note (2026-09-10): line numbers below are stale — `proxy.rs` has grown
> to ~14k lines and `local_model.rs` was rewritten (~3100 lines). Current
> anchors: main upstream client at `proxy.rs:323`
> (`upstream_client_builder`), `header_upstream_override` at
> `proxy.rs:3063` (call site `:3319`). Also, `extract_json_block`
> (`headroom-core/src/transforms/content_router.rs:1105`) now has a
> production caller at `:1194` — the §9 claim that neither function is
> called outside tests is half-wrong (`split_into_sections` at `:1154`
> is still test-only).

Tracking doc for parity work identified while auditing `42ebbc6c..origin/main`
(40 commits, 23 Python-only with no corresponding Rust change). The small
correctness-bug batch (status_code/5xx accounting, x-headroom-base-url,
non-streaming cache metrics) is being ported directly. The items below are
larger, standalone features — deliberately scoped out of that batch and
tracked here so the thread isn't lost.

> **Moved to [`ideas/implemented/parity-turn-hooks.md`](ideas/implemented/parity-turn-hooks.md)** — turn-hooks item incl. scoping notes.

> **Moved to [`ideas/implemented/parity-ccr-openai-buffered.md`](ideas/implemented/parity-ccr-openai-buffered.md)** — OpenAI buffered-CCR item incl. scoping notes.

> **Moved to [`ideas/implemented/parity-audit-safe-smartcrusher.md`](ideas/implemented/parity-audit-safe-smartcrusher.md)** — audit-safe item incl. scoping notes.

> **Moved to [`ideas/implemented/parity-bedrock-model-map.md`](ideas/implemented/parity-bedrock-model-map.md)** — ARN-pinning item (was-already-there verification).

> **Moved to [`ideas/implemented/parity-ccr-streamed-anthropic.md`](ideas/implemented/parity-ccr-streamed-anthropic.md)** — streamed-CCR item in full.

> **Moved to [`ideas/implemented/parity-proxy-owned-tools.md`](ideas/implemented/parity-proxy-owned-tools.md)** — proxy-owned-tools item in full.

> **Moved to [`ideas/implemented/parity-metrics-fixes.md`](ideas/implemented/parity-metrics-fixes.md)** — metrics item in full.

## 8. Known gaps left open

> **Moved to [`ideas/implemented/parity-routed-responses-arm.md`](ideas/implemented/parity-routed-responses-arm.md)** — routed Responses arm record.

> **Moved to [`ideas/implemented/parity-openai-streaming-buffered.md`](ideas/implemented/parity-openai-streaming-buffered.md)** — port record (moved from the gap entry it closed).

> **Moved to [`ideas/implemented/parity-ccr-inject-marker.md`](ideas/implemented/parity-ccr-inject-marker.md)** — inject-marker wiring record.

## 9. Round of 2026-08-28 — upstream `32d7ca45..` (v0.36.2..v0.37.0, 40 commits)

Merged at `78240da9`. Twelve commits touch no Python source (CI bumps, docs, a
Windows process-tree fix). Of the 28 that do, sixteen patch subsystems Rust
does not have — `wrap`, `learn`, `doctor`, the dashboard, MCP/Serena,
`session_engine`, the `/v1/compress` sidecar, the memory graph adapter, Copilot
provider routing — so they are a scoping question, not a gap. Two are answered
by the Rust design and are covered in 9.3 below. Every Rust-applicable gap
identified in this round is now ported.

Ported this round, committed 2026-09-01 and untested against production:
`27b4e2d1` (proxy token on every route and transport, `proxy_auth.rs`),
`7c0b8860` and the resolve-timeout half of `3e3c4094` (`upstream_guard.rs`),
`b9d7dcc3` (atomic ledger write, and the flush moved to `spawn_blocking`),
`9c30b629` (cross-turn dedup skipped on streaming chat), `8884d873` (the
64-word Kompress floor), `7784bb18` (datetime-prefixed prompts no longer typed
as grep output), `4408e881` (`HEADROOM_PROTECT_READS`, including Copilot
`view` and both local-shell wire shapes), `25ca5808` + `1617f839` (Codex
`additional_tools` lift/restore), and `36cc8001` (corporate CA trust on every
outbound reqwest client). The caller-supplied upstream property from
`3e3c4094` is enforced at the connection boundary too: the approved DNS answer
set is pinned into the reqwest transport that performs the request.

Two commits look like gaps and are not. `split_into_sections` and
`extract_json_block` (`content_router.rs:1105`, `:1154`) carry the pre-fix
bracket-counting bug from `8884d873`, but neither has a caller outside its own
tests, so nothing reaches it. Verify with
`grep -rn 'split_into_sections\|extract_json_block' crates/ --include=*.rs`
before spending time there.

> **Moved to [`ideas/implemented/parity-codex-additional-tools.md`](ideas/implemented/parity-codex-additional-tools.md)** — additional-tools item incl. scoping notes.

> **Moved to [`ideas/implemented/parity-corporate-ca.md`](ideas/implemented/parity-corporate-ca.md)** — corporate-CA item incl. scoping notes.

> **Moved to [`ideas/implemented/parity-caller-upstreams.md`](ideas/implemented/parity-caller-upstreams.md)** — caller-upstream item incl. scoping notes.

### 9.4 Completion verification

`cargo test -p headroom-core --lib --tests`: 2056 passed, 2 ignored, plus all
core integration binaries green. `cargo test -p headroom-proxy --lib --tests`:
1794 library tests passed, 1 ignored, plus every proxy integration binary green
(only explicitly live-store/operator-data tests ignored). `cargo check -p headroom-proxy
--all-targets` and `git diff --check` are green.

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
  A). **Resolved 2026-08-07: verdict C, not a gap — the bug class does not
  exist in this architecture.** The Python bug lived in a *role-gated* freeze
  boundary: `_strict_previous_turn_frozen_count` treated a trailing
  `role: "tool"` / `"function"` message as non-mutable, so the whole
  conversation froze before `ContentRouter` ran and there was no live tool
  observation left to compress. Rust has no such function anywhere in
  `crates/`, and computes the boundary purely by token accounting —
  `prefix_replay.rs:517-529` walks per-message estimates against what the
  provider reported cached, with no role branch. Independently,
  `live_zone_openai.rs:11-13` already defines the live zone as the latest
  `tool` message's content *and* the latest `user` message's text, which is
  the post-fix Python semantics. Verified structurally; not measured against
  a live OpenAI cache-mode session.
</content>
