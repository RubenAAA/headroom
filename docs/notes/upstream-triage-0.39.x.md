# Upstream triage: `d13e1966` (v0.39.1) → `b73adaa0` (50 commits)

Done 2026-09-30 with the merge `ca2a6fd1`. Each commit touching `headroom/`
was checked against `crates/` with `git show` and a file:line. Classes as in
[`upstream-triage-0.39.md`](upstream-triage-0.39.md).

Totals: **6 MERGED, 6 ported, 1 open, 4 optional, 5 ALREADY-THERE, 28 N/A** (the
three ports after the first three came the same day; see "Landed ports").
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
- `46755b5f` → `cd860f0b`, narrowed: upstream skips memory tools for any
  client not known to carry tools, because its client must run the calls. Here
  the proxy answers them, so the rule is only "the client sent no tools, so
  none are added" on `/v1/messages`. In the 2026-09-25..30 logs that was 195 of
  24,702 injections, all Sonnet with a ~130 KB system prompt and two messages
  (Claude Code's permission classifier), none of which called a memory tool.
- `117ff72e`: the holdout key reads every text block of the first user
  message in full (`headroom-core/src/output_savings.rs`). The 512-character
  cut gave every conversation in a project one key, so one arm.
- `afaaaa88`: a date suffix is not a minor version
  (`headroom-core/src/transforms/thinking_compactor.rs`). Nothing calls
  `bills_prior_thinking` yet.

## Open

| sha | class | cache | what | Rust evidence | size |
|---|---|---|---|---|---|
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

# Upstream triage: `b73adaa0` → `d5318ac2` (71 commits)

Done 2026-10-02 with the merge `0d69a49d`. Same method and classes as above.

Totals: **3 ported, 6 ALREADY-THERE, 7 optional, 55 N/A**. The one Rust
commit, `49f69be2`, was not taken; see "Already there".

## Landed ports

- `ffc35997`: a chained command is a read when any `;`/`&&`/`||` segment is
  one; redirects and `tee` count against their own segment, a heredoc against
  the whole command (`headroom-core/src/transforms/read_protection.rs`).
  Before, `git status && cat f.rs` went to the code-aware compressor.
- `f19bc9a9`: the whitespace waste signal was always 0; each run now
  collapses to one space (`headroom-core/src/waste_signals.rs`). Feeds
  metrics only.
- `f8645257`: output-savings baseline back-off pools every stratum under the
  shorter prefix and returns no evidence instead of the global mean
  (`headroom-core/src/output_savings.rs`). Upstream's opt-in
  `fall_back_to_global` has no caller and was left out. This machine's
  baseline is empty (`glob.n = 0`), so today's numbers do not move.

## Optional

| sha | what | Rust evidence | why not now |
|---|---|---|---|
| `fe2ed2b5` | Kompress keeps line breaks and table rows | `kompress.rs:522` joins kept words with a space | Kompress is off in the flags file |
| `8dbbd1d8` | Keep record-bearing JSON away from Kompress | JSON arms go to SmartCrusher in `live_zone/dispatch.rs` | Kompress is off |
| `005a4e14` | "Original content preserved." in the Kompress marker | | Kompress is off |
| `74603899` | `HEADROOM_LICENSE` is the licence variable now | `main.rs` warns on `HEADROOM_LICENSE_KEY` only | No licence in use |
| `9b26a49c` | Batch paths honour `x-headroom-bypass` | `handlers/batch*.rs` never read it | `--enable-batch-api false` |
| `0a2c80d5` | Memoize OpenAI token counts | No count cache in `tokenizer/` | Perf only, unmeasured here |
| `861e94d8` | No exception text in client error bodies | `error.rs:74-86` sends the reqwest error | Local single-user proxy; the text is for its own user |

## Already there

- `49f69be2` (live-zone SourceCode/PlainText arms): both arms route in
  `live_zone/dispatch.rs:309,375` (`edeb2cca`), and Kompress loads only off
  the request path (`live_zone/compressors.rs`). Declined: the
  `HEADROOM_LIVE_ZONE_DISABLE_ARMS` env switch (`--code-aware` and
  `--disable-kompress` already turn both arms off) and the 2048/5120 byte
  floors (code-aware savings were measured at 512). Its tests and bench
  target upstream's API and were dropped.
- `94bb0558` (`proxy/ccr_response.rs:827`), `c46e7cc6`
  (`proxy/egress.rs:344`, keepalive 20 s), `dcec8458`
  (`proxy/ccr_expansion.rs:55-62` skips `<system-reminder>`), `c719d4af`
  (`savings_tracker.rs` folds tool-schema into tokens and dollars alike),
  `8f3d6773` (`compressors.rs`, a failed load is `None`).

## N/A

- No Rust counterpart: `728ff7e2` (the CCR store keeps raw text), `f87848cf`
  (no `passthrough:*` model names), `143a38d5` and `4257ed4d` (no traffic
  learner), `f90a56b0` and `9a9f35cd` (no TOIN), `88a1f4e5` (no per-path
  inbound counter), `a6d6c14c` (Chat Completions through a gateway),
  `246162d7` (no client sends the header), `3c9ed010` (not run stateless).
- Python-only: wrap, doctor, install and MCP registry (`8cfeb692`,
  `1982b3a7`, `bb1ab6af`, `d5318ac2`, `7287589d`, `0712e048`, `d0fd56e4`,
  `f824a270`), copilot (`81a8a28d`, `2b2dc1b2`), learn (`c072251c`,
  `db90b93d`, `c46e74d0`), memory and storage (`231a6277`, `a8c2e4d9`,
  `b17b8127`, `91237cae`), plugins (`d75eecd8`, `2157400b`, `d1ad1898`),
  dashboard and telemetry (`ecc49670`, `7df8bd87`, `f0ec2bb3`, `58b14542`),
  and the rest (`fef99cc5`, `f78e66f7`, `eaa16d9f`, `fed72811`, `ef1c528e`,
  `8538a831`, `d5e5534b`, `d7ed2309`, `b625d188`, `76ef2c3b`, `885af385`).
- Docs and dependencies: `7ff67693`, `6ff74c5d`, `dea0f626`, `3080a9d4`,
  `2df0c634`, `14a4b543`, `573e385f`, `59b8cefc`, `9320c972`, `ff1a0d69`.
