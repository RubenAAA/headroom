# Upstream triage: `fc1ba891` → `bb8d18a5` (203 commits)

Merged as `b0793b91` on `upstream-sync-2026-11-09`. Fetched 2026-11-09;
covers v0.43.0 and v0.43.1. Classes as in
[`upstream-triage-4c7acba5-fc1ba891.md`](upstream-triage-4c7acba5-fc1ba891.md):
PORT, ALREADY-THERE, N/A. Python paths map to `upstream-python/headroom/`.

Totals: **28 PORT (6 HIGH, 22 LOW; 22 landed, 6 skipped), 19 merged as Rust
from upstream, 21 ALREADY-THERE, 135 N/A** (82 Python-only, 53 docs, CI,
deps, tests and plugins).

## Merge

Rust that upstream patched itself, merged by hand where our files had moved:

- CCR backends: `secs_i64` TTL clamp and seq-based in-memory eviction
  (`c6f5bf95`, `9e8cc64b`); our eviction log and compaction stay.
- `4d7765c8`, `a538740e`, `4b788fd0`, `9355ec70`: offload, log, search and
  diff compressors store the input behind the marker; our put-failed guard
  (return the original) stays. Diff keeps our helpers and takes multi-hunk
  context and combined-diff parsing.
- `93279f32` lossless JSON minify fallback, ported into our live-zone
  dispatch, embedded-JSON and content-router paths. No CCR marker for it.
- Kompress and ORT (`5daaf336`, `7e827285`, `69fbd239`, `eef44f60`):
  `hf_cache`, pinned revisions and the session gate, behind the `ml` feature.
  The offline tokenizer still serves a cached file before it refuses.
- `adc67249` Bedrock credentials resolve through an async provider source.
- `9e8cc64b` `auth_mode`: ours plus upstream's `trim_start`; Redis URL redacted.
- `08e66222`, `8f4122f4`, `385d487f`, `ced26852`: clean.
- Edition 2024: upstream `set_var`/`remove_var` calls wrapped in `unsafe`.
  Parity fixture paths point at `upstream-python/tests/parity`.

Full suite at the merge: 6196 passed.

## Landed

| sha | commit | what landed |
|---|---|---|
| `31f13040` | `f53d3ca1` | ORT telemetry off unless `ORT_DISABLE_TELEMETRY` is set to a falsy value: env set before the runtime starts, `with_telemetry` on the OpenVINO and CUDA inits too |
| `1b9d296c` | `80395e83` | Cline's `headroom__headroom_retrieve` counts as the retrieve tool (exclusion and routed paths) |
| `da6d9736` | `db6f1923` | only `1/true/yes/on` in `HEADROOM_BINARIES_ALLOW_UNVERIFIED` skips the sha256 check |
| `3912c040` | `804d43aa` | retrieve hash lowercased before the store lookup (tool call and handler); also the Rust half of `842d7e1a`, whose Python came in with an earlier merge |
| `682bf73f` | `508e3117` | Codex WS remembers retrieval call ids across frames (cap 512, then passthrough); E2E test fails without it |
| `9648d2d8` | `58e3aaeb` | `chatgpt-4o-*` counts with `o200k_base` |
| `911c75ff` | `711daffe` | a rejected base URL logs scheme, host and port only |
| `3871b366`, `dfcac781` | `b2aca7d6` | Connection-listed headers filtered on Gemini, routed and sidecar responses |
| `673831a4` | `1de6709b` | `/api/hello` answered with an empty 200 behind the token gate |
| `12dca26a` | `ed6c95a0` | `<system-reminder>` spans cut out of the memory and CCR query |
| `2568cbed` | `1eaffc0e` | chat memory only when the last message is the user's; multimodal content keeps its parts |
| `96e81ac6` | `fa9855bd` | a pre-rule output-savings ledger is rewritten once at startup |
| `312ffec8`, `8cdce172` | `9a7cd584` | read protection parses sed options (GNU and BSD) and ignores quoted or escaped shell operators; upstream's 73-case table passes |
| `7c150ec6`, `0d0cda22`, `54f48fb1` | `0f46df44` | code compressor: Rust attributes stay with their items, multi-line class headers keep the brace line, `;`-shared lines are kept once. Two departures, below |
| `50bf775e` | `9157ed3a` | a declared body over `--compression-max-body-bytes`, within `--max-body-bytes`, is forwarded unchanged instead of 413 |
| `35ca9b83` | `a04e30fd` | chat replay mirrors the client's markers (message, tool call, block) before the cap |

`5ab1a7e1` moved fork-only tests out of `crusher.rs` and
`code_compressor.rs` so both stay inside the file-size ratchet; `0f46df44`
moved the fork-only syntax breaker to `code_compressor/syntax_breaker.rs`.

Departures in `0f46df44`:

- Upstream skips a root-level `;` in every language. Perl keeps a statement's
  `;` as a root sibling, so dropping it joins two statements and the Perl
  reference test fell back to the original. Ours skips it for Python only.
- Upstream's `reorder_unsafe` is not ported. It returns the original whenever
  a Python declaration follows top-level code, so one constant above the first
  class (upstream's own fixture `1e496eba`) moves the ratio from 0.63 to 1.0.
  Import demotion is ported: an import after top-level code stays in place.
- Rust attached leading comments only to package, import and export nodes.
  Python has attached them to functions, classes, decorated definitions and
  types since `adf8fed9`; ours does now too.
- The parity test rewrites the two Rust fixtures recorded before `7c150ec6`
  so `#[derive(Debug)]` sits on its enum instead of trailing the output.

## Skipped

- `b1f340e0` CCR proactive expansion after the last cache breakpoint. Upstream
  puts the expansion after the client's last marker and strips it from the
  replay snapshot. Ours places tail breakpoints after expansion runs, so the
  marker can land on the expansion block itself; stripping it from the
  snapshot alone would make the next turn miss from there. Needs its own look
  at `place_replay_breakpoints`. `contrib/headroom-flags.sh` ships
  `--ccr-proactive-expansion false`; the binary default is `true`.
- `861ef956` model router shadow mode, `9b2dec56` `/stats` metadata for
  token callers, `8388ff5e` cumulative output tokens in history,
  `84cedbf2` `cost_usd` in recent requests: features nothing here reads.
- `0ddaa56c` Vertex errors explained in-band: an optional hint on the
  rawPredict error branch (`vertex/raw_predict.rs:598`); nothing asks for it.

## Found on the way

- Dispatch caches request-scoped declines for 30 minutes:
  `dispatch_compressor_with_config` calls `mark_skip` on every `NoOp`
  (`transforms/live_zone/dispatch.rs:234`), including an ML budget refusal,
  a Kompress deadline hit and "model not cached yet". Same content then skips
  Kompress for 1800 s. Mark skip only when the compressor ran and
  `declined_by` is `None`. Not measured.
- `--compression-max-body-bytes`: fixed for declared lengths in `9157ed3a`.
  A chunked body still gets 413 past it, and the conversations, Vertex and
  Foundry handlers buffer at the same limit.
- Memory injection ignores cache mode: Python never injects in cache mode;
  ours appends to the live tail and leans on replay, so a replay miss after a
  restart shifts the prefix. Moot under `HEADROOM_MEMORY_MODE=tool`.
- Python pins HTTP/1.1 for chatgpt.com (Cloudflare HTTP/2 fingerprint). Ours
  never has. If Codex HTTP fallbacks show 403 challenges, try `http1_only()`.
- ORT's implicit default environment also has telemetry on, which is why
  `f53d3ca1` sets the env var as well as the builder flag.
- Several upstream fixes land in Rust modules with no production caller:
  `thinking_compactor`, `tabular_ingest`, `dense_line_elider`, and
  `content_router` sectioning. Porting them changes nothing until wired in.
- Upstream regressions worth reporting there: the all-language `;` skip
  (Perl), and `reorder_unsafe` against its own parity fixture.

## ALREADY-THERE

`78736a16`, `effa5a58`, `6ef52c20`, `98018333`, `51258ef1`, `5c8772b7`,
`5d40fe76`, `5ad23637`, `e9331b6b`, `9226eaf3`, `bb133848`, `946a70cf`,
`e4a60ed7`, `3afaa97f`, `e51b8065`, `398d7cdc`, `7592e681`, `ce14271b`,
`a656eb43`, `bfbadec8`, `00d4f441`.

Both must-checks against the last sync came up as PORT and landed: `35ca9b83`
follows `55602fc1` and `682bf73f` follows `89545fbd`. `bfbadec8` follows
`421d453c`; ours already took the approach upstream settled on.

## N/A

Python-only proxy plumbing (handlers we route differently, litellm and other
backends, Copilot, OpenCode, OpenClaw, LangChain, LangGraph), memory and mem0,
TOIN, learn, wrap, install, init, doctor, update check, dashboard, MCP, SDKs
and plugins, docs, CI, npm, pip and cargo bumps, release commits.
