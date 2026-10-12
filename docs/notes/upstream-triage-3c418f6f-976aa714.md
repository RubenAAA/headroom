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
read only base LiteLLM rates and ignored `*_above_<N>k_tokens`. Fixed
after the ports: `observability::pricing::lookup` takes the prompt size
(input + cache read + cache write) and returns the highest tier it
crosses. Unlike upstream's 8c1beb20 fallback, a model whose entry lacks
tier keys bills base: the vendored snapshot still carries them for
Sonnet 4.5, so no hand-kept premium table is needed today.

## ALREADY-THERE

`8c1beb20` (core pricing table carries 200k rates), `a9815bbb`
(`outcome.rs:33-46`), `01d91948` (different mechanism,
`ccr_response.rs:625-644`), `d091fbd9` (`sse/anthropic.rs:733-760`),
`33ade5b0` (`headers.rs:319-332`), `159daa25` (`bedrock/invoke.rs:604-611`),
`1a4e4194` (clap, `config.rs:1702`), `a6545580` (orchestrator logs the
transform; upstream now hides the message text, Rust still logs it, but
there the text is the proxy's own format strings, not payload),
and the four `crates/headroom-core` commits (`47f87108`, `a20c76c2`,
`eaa7308a`, `3db1a944`), which merged cleanly; each one's `crates/`
diff reverse-applies on HEAD.

## N/A

CLI wrap/init/learn (`976aa714`, `a6523b19`, `07b45115`, `71663b44`; Rust
has no `headroom learn`),
mcp_registry (`f2153fec`), traffic learner (`e6eaa411`), OTEL window
(`7d9dd55e`), gateway compress API (`5cac7ab6`), route specs (`69a16cb5`),
Python system relocation (`d9e29bb2`), `/api/hello` routing (`931e5b96`,
Rust sends everything to one upstream), openclaw plugin (`37f3f9ef`),
any-llm backend (`03538f2f`), import laziness (`59bd482f`).

Python dead-code removal and deprecation, nothing Rust mirrors:
`f14906e0`, `efd6e157`, `2c2357ad`, `0494edcc`, `7387b8cc`, `235043d3`,
`4bf4ae87`, `ebc59356`, `9b1971dd`, `2e497cbe`, `e4a1ec19`, `fc8ff5a7`,
`9b42fcc4`, `098df1c7`, `02ce467d`, `c191beef`, `73f59a32`, `31d378b0`,
`132cd583`, `25cc2d58`, `abbf36f0`, `30d23fb7`, `6d23895d`, `e7ac6ca6`,
`5c72d824`, `08fce183`, `310c108c`, `63cc9afa`, `de896926`, `688b3270`,
`8e61cca2`, `e5cbec5f`, `58387c11`, `fc5ac70b`, `7e5d97fe`.

Tests only: `1b7466d1`, `415fed07`, `9266b030`, `65a21987`, `85c35799`,
`a63b23fe`. Python install, release and deps: `64a661d5`, `5682883b`,
`79f2c278`, `c195a122`, `3f990663`, `51ec6e8f`. Python dashboard
templates (`4e4f49d0`, `20ce054f`): the Rust `observability/dashboard.html`
is its own page and has neither the savings-profile badge nor the
settings selects. Docs and CI: `a31d89b4`, `c7a223da`. The
`headroom-snip` plugin (`17abee27`) moved to `upstream-python/plugins/`.
