# Upstream triage: `3c418f6f` → `976aa714` (87 commits)

Merged as `fd8137b8`. Fetched 2026-10-12. Classes as in
[`upstream-triage-0.39.md`](upstream-triage-0.39.md): PORT (Rust counterpart
exists, lacks the behaviour), ALREADY-THERE, N/A (no Rust counterpart, or
docs/CI/tests). Python paths map to `upstream-python/headroom/`.

About 50 commits only delete or deprecate Python code (mirrors of Rust
selectors, memory adapters, dead helpers). Nothing in `crates/` depends on
them: N/A.

Totals: **9 PORT (all open), 9 ALREADY-THERE, the rest N/A**.

## PORT

| sha | what | where in Rust |
|---|---|---|
| `42d4f58b` | a 200 stream that ends in an `error` event or without `message_stop` counts as a failed request, not an unbooked turn | `proxy/sse_anthropic.rs:445-475`; check `proxy/sse_openai.rs` for `response.failed` |
| `f8212a39` | strip `defer_loading` before measuring a turn hook's tool delta | `proxy/hooks.rs:155-177` |
| `fd067033` | dense-line elider must not elide lines holding nested JSON after a label (`result: {...}`) | `headroom-core/src/transforms/dense_line_elider.rs:46-51` |
| `2a12e035` | holdout estimate needs 30 clusters, 0.5 coverage, 10pp band before it beats the baseline | `headroom-core/src/output_savings.rs:450-588` |
| `463c1a43` | a missing cache rate must not force the multiplier to 1.0 and label it `vendored` | `headroom-core/src/cost_tracker.rs:877-885` |
| `b1fb162b` | grok-shell/grok UA → `grok_build`, `api.x.ai` → xAI, xAI cache rates (read 0.16, write 1.0) | `websocket_codex.rs:112-126`, `cost_tracker.rs:39-62`, `display_provider.rs:17-27` |
| `489de468` | per-agent savings attribution (`by_agent` next to `by_provider`/`by_model`) | `headroom-core/src/savings_tracker.rs:646,1682,2626` |
| `a88e2d51` | error bodies echo raw error text; return fixed messages per error kind | `handlers/gemini.rs:320,479`, `batch.rs` (10 sites), `batch_anthropic.rs:157` |
| `1fb4cbae` | astgrep outline should read Claude Code's `[Truncated: PARTIAL view …]` banner (optional disk check skipped) | `interceptors/astgrep.rs:157,257` |

Side gap found on the way, not an upstream commit: the observability ledger
reads only base LiteLLM rates and ignores `*_above_200k_tokens`
(`observability/pricing.rs:161-187`).

## ALREADY-THERE

`8c1beb20` (core pricing table carries 200k rates), `a9815bbb`
(`outcome.rs:33-46`), `01d91948` (different mechanism,
`ccr_response.rs:625-644`), `d091fbd9` (`sse/anthropic.rs:733-760`),
`33ade5b0` (`headers.rs:319-332`), `159daa25` (`bedrock/invoke.rs:604-611`),
`1a4e4194` (clap, `config.rs:1702`), `a6545580` (orchestrator logs the
transform; upstream now hides the message text, Rust still logs it),
and the four `crates/headroom-core` commits, which merged cleanly.

## N/A

CLI wrap/init (`976aa714`, `a6523b19`, `07b45115`, `71663b44`),
mcp_registry (`f2153fec`), traffic learner (`e6eaa411`), OTEL window
(`7d9dd55e`), gateway compress API (`5cac7ab6`), route specs (`69a16cb5`),
Python system relocation (`d9e29bb2`), `/api/hello` routing (`931e5b96`,
Rust sends everything to one upstream), openclaw plugin (`37f3f9ef`),
any-llm backend (`03538f2f`), import laziness (`59bd482f`), docs, CI,
tests, and the `headroom-snip` plugin (moved to `upstream-python/plugins/`).
