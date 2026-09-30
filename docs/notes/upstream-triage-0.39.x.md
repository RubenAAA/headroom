# Upstream triage: `d13e1966` (v0.39.1) → `b73adaa0` (50 commits)

Done 2026-09-30 with the merge `ca2a6fd1`. Each commit touching `headroom/`
was checked against `crates/` with `git show` and a file:line. Classes as in
[`upstream-triage-0.39.md`](upstream-triage-0.39.md).

Totals: **6 MERGED, 3 ported, 4 open, 4 optional, 5 ALREADY-THERE, 28 N/A.**
Plus one gap with no upstream commit behind it (last row of Open).

## Merged as Rust

| sha | what | note |
|---|---|---|
| `9d98ea59` | Request paths forwarded verbatim; rustls provider pinned | Placed into `proxy/upstream.rs` and `proxy/state.rs`. Bedrock now keeps a `bedrock_endpoint` path prefix (the fork's `clear()` dropped it). |
| `66258c40` | OS + `HEADROOM_CA_BUNDLE` roots for WebSocket upstreams | Also applied to `websocket_codex.rs`. HTTP keeps `ssl_context.rs`; reqwest's platform verifier already reads the OS store, so `tls::extra_root_certificates` has no caller. |
| `7790bdee`, `d90b3200`, `f3f2e007` | Compressor fixes | Taken as they are. |
| `0d99c56d` | Claude 5.5 pricing | Only the `claude-sonnet-5-5` row was missing. |

## Landed ports

- `ccd9fffc` → `51ebd03f`: tool_result blocks first after retrieve repair.
- `b73adaa0` → `b5a3a021`: drop a `tool_reference` in `tools` that names the
  typed search tool.
- `8ff46dc9` → `765faa0d`: a bare string in a `tool_result` list survives the
  Responses translation.

## Open

| sha | class | cache | what | Rust evidence | size |
|---|---|---|---|---|---|
| `46755b5f` | PORT | yes | Don't inject memory tools into requests that sent no tools | `proxy/forward/memory.rs:33-40` injects on purpose; reverses a fork choice, needs a decision | S |
| `117ff72e` | PORT | once | Holdout key over the whole first user message | Session id keys Claude Code (`output_shaper.rs:241`); fallbacks cut at 512 in `headroom-core/src/output_savings.rs:79-113` | S |
| `afaaaa88` | PORT | no | Date suffix read as a minor version | `headroom-core/src/transforms/thinking_compactor.rs:128-145`; no caller yet | S |
| `2f076687` | PORT-NEW | Hermes only | Unwrap Hermes `tool_call` so exclusions still apply | No unwrap anywhere; `tool_exclusion.rs:99` | M |
| (none) | PORT-NEW | yes | `headroom_retrieve` on non-streaming chat completions, with Python's session-sticky injection | `inject_ccr_retrieve_tool` has a chat branch but its only caller is Anthropic-only (`proxy/forward/ctx.rs:620`) | M |

Optional, only if a user needs them: `ffc6edb4` (Bedrock 413 as JSON),
`4b7e5d24` (guarded upstreams through a proxy), `b1b005a9` (percent-decode
`X-Headroom-Cwd`), `922924ef` (only if `custom_tool_call_output` becomes
compressible).

## Already there

`bb8c2857` (`proxy/forward/memory.rs:322-373`), `126e1448`
(`proxy/forward/response.rs:56-60`), `c946b6b0` (`DefaultBodyLimit`,
`proxy/app.rs:535`), `7c9fbed6` (`live_zone/dispatch.rs:409-414`),
`1512c062`.

## N/A

Wrap, install and CLI (`c32a4f41`, `de4cf7e7`, `4227bd24`, `f69e2463`,
`3ffa57ec`, `5bf66123`, `08dda4cb`, `bd0296b5`), the GIL watchdog
(`79daeb59`, `7d856b81`), the Python-only surfaces (`0eba65ca`,
`717527bc`, `7e73438d`, `1854fd7f`, `63269650`, `f519fa8b`, `b10dd8db`),
SDK, docs, CI and dependency bumps.

## Found while triaging

- Every `headroom agent-savings` profile export stopped the proxy at
  startup: four flags used clap's true/false parser against `0`/`1` and a
  count. Fixed in the commit after the ports.
- The profile exports `HEADROOM_SMART_CRUSHER_COMPACTION`; the proxy reads
  `HEADROOM_SMART_CRUSHER_WITH_COMPACTION`. Nothing on a serving path reads
  the field either way, so left alone.
