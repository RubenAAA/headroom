# Upstream triage: `3c418f6f` → `976aa714` (87 commits)

Merged as `fd8137b8`. Fetched 2026-10-12. Classes as in
[`upstream-triage-0.39.md`](upstream-triage-0.39.md): PORT (Rust counterpart
exists, lacks the behaviour), ALREADY-THERE, N/A (no Rust counterpart, or
docs/CI/tests). Python paths map to `upstream-python/headroom/`.

About 50 commits only delete or deprecate Python code (mirrors of Rust
selectors, memory adapters, dead helpers). Nothing in `crates/` depends on
them: N/A.

Totals: **9 PORT (all landed, some in part), 9 ALREADY-THERE, the rest N/A**.

## Landed ports

| sha | commit | what landed | left out, and why |
|---|---|---|---|
| `42d4f58b` | `304d0e0a` | an in-band error on a 2xx stream (Anthropic `error`, Chat error frame, Responses `response.failed`) books as failed work, status 502 | a stream that just stops early stays unbooked: Rust cannot tell it from a client disconnect |
| `f8212a39` | `f5fa469d` | `defer_loading` stripped before measuring a hook's tool delta; deferral credit capped at the booked tools still deferred | — |
| `fd067033` | `db6d19f3` | the elider keeps a line whole when parseable JSON spans (nested included) make up half of it, or the scan runs out of budget | upstream declines on any span, which matches every `[]`/`{}` in minified JS. The router half has no Rust counterpart (no Kompress JSON-record guard) |
| `2a12e035` | `255e5be4` | measured beats baseline only at 30 conversations per arm, 50% coverage, ≤10pp band | — |
| `463c1a43` | `ec98b570` | a cache rate the table lacks keeps the provider default; source `mixed` when half-vendored | LiteLLM `cost_per_token` resolver: Python only |
| `b1fb162b` | `8564daaa` | `grok-shell/`, `grok/` → `grok_build`; `api.x.ai` (trailing dot too) → xAI | xAI cache row: Rust never labels a provider `xai` |
| `489de468` | `8e54ba7c` | history checkpoints carry `agent`; rollups get `by_agent` | — |
| `a88e2d51` | `38abb859` | fixed invalid-body and upstream-failure texts in the batch, Anthropic batch and Gemini handlers; the error goes to the log | workflow, eval and npm script changes |
| `1fb4cbae` | `ab4acb6f` | Claude Code's truncation banner changes the astgrep outline header | opt-in disk verification: needs `headroom wrap`'s workspace registry |

Side gap found on the way, not an upstream commit: the observability ledger
reads only base LiteLLM rates and ignores `*_above_200k_tokens`
(`observability/pricing.rs:161-187`).

## ALREADY-THERE

`8c1beb20` (core pricing table carries 200k rates), `a9815bbb`
(`outcome.rs:33-46`), `01d91948` (different mechanism,
`ccr_response.rs:625-644`), `d091fbd9` (`sse/anthropic.rs:733-760`),
`33ade5b0` (`headers.rs:319-332`), `159daa25` (`bedrock/invoke.rs:604-611`),
`1a4e4194` (clap, `config.rs:1702`), `a6545580` (orchestrator logs the
transform; upstream now hides the message text, Rust still logs it, but
there the text is the proxy's own format strings, not payload),
and the four `crates/headroom-core` commits, which merged cleanly.

## N/A

CLI wrap/init/learn (`976aa714`, `a6523b19`, `07b45115`, `71663b44`; Rust
has no `headroom learn`),
mcp_registry (`f2153fec`), traffic learner (`e6eaa411`), OTEL window
(`7d9dd55e`), gateway compress API (`5cac7ab6`), route specs (`69a16cb5`),
Python system relocation (`d9e29bb2`), `/api/hello` routing (`931e5b96`,
Rust sends everything to one upstream), openclaw plugin (`37f3f9ef`),
any-llm backend (`03538f2f`), import laziness (`59bd482f`), docs, CI,
tests, and the `headroom-snip` plugin (moved to `upstream-python/plugins/`).
