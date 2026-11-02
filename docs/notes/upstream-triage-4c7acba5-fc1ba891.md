# Upstream triage: `4c7acba5` → `fc1ba891` (113 commits)

Merged as `7ca63e6c` on `upstream-sync-2026-11-02`. Fetched 2026-11-02.
Classes as in [`upstream-triage-976aa714-4c7acba5.md`](upstream-triage-976aa714-4c7acba5.md):
PORT, ALREADY-THERE, N/A. Python paths map to `upstream-python/headroom/`.

Totals: **25 PORT (22 landed here, 3 skipped), 2 merged as Rust from upstream, 18 ALREADY-THERE, 68 N/A**.

## Merge

- `1be699f6` tungstenite drops its default webpki store (and the 0.26
  shim); `ctutils` 0.4.5. Our sorted `Cargo.toml` kept, the feature edit applied.
- `703755e8` Haiku 5.5 rates and long-context card. We already had
  multi-tier pricing (`da3fee18`), so ours stays. Ported its one addition:
  when compression keeps a prompt under a threshold the uncompressed one
  would have crossed, credit the difference of the two full bills
  (`PriceCard::compression_savings_usd`). Its three ledger tests came along.
- `023dd1bc` log compressor omitted-level counts, `a2d4fe72` embedding batch
  cap: upstream patched the Rust directly; merged clean.

## Landed

| sha | commit | what landed |
|---|---|---|
| `703755e8` | `1d1f2314` | core `pricing.rs` Haiku 5.5 row (was the `claude-` catch-all at 30x); 100K card not modelled there |
| `41c15b31` | `e91c46d8` | budget ledger keeps in-period costs; the 100k entry cap is gone |
| `4c3799fd` | `2d9ac190` | an Edit/Write rejected with `<tool_use_error>` no longer makes Reads stale |
| `11f743dd` | `a30f910b` | conversation savings key: `thread_id` before `session_id` |
| `2cf64ef0` | `2ab4dedb` | `private_db`: lstat first, `O_NOFOLLOW` + fd chmod; no owner check (no `geteuid` without a rustix feature) |
| `b196424e` | `7f30360b` | router compression cache: 64 MB LRU, idle expiry; no disk tier |
| `21e88675` | `56aa1988` | dedup folds only when the pointer is shorter in bytes; 18-digit cap |
| `33e5f674` | `74eb3d3a` | `bounded_model_key` in cost tracker; history model names clipped |
| `37654390`, `760c05e1` | `fb481786`, `3b99548b` | cwd read only at line start (proxy and core copies); non-UTF-8 cwd unresolved |
| `9f5cfcfd` | `777c54c2` | transparent images flattened onto white before JPEG |
| `421d453c` | `ac3cca4e` | Kompress keeps words past the 512-token cut; chunk loop untested (no libonnxruntime here) |
| `55602fc1` | `d12153b8` | chat replay caps `cache_control` markers at 4, oldest first |
| `89545fbd` | `cd4a26d1` | Headroom's MCP retrieval aliases count as the retrieval tool |
| `d9958bfd` | `d0285840` | cc-switch loop guard matches every local spelling of the proxy |
| `f59c8ffc` | `75ac81dd` | a non-2xx turn-hook re-drive returns Null; the original response stands |
| `185454be` | `7de42dbc` | CCR store refuses the original: live zone, diff and log forward the input, no marker (Redis half N/A) |
| `4bf1bf55` | `8f7ad544` | Gemini compression keeps snake_case, code-execution and thought parts |
| `4fd9cddf` | `3758a78a`, `a107c4a1` | bypass header honoured on Anthropic, OpenAI and Google batch create |
| `2a70d128` | `d83bf4e5` | Responses drops `web_search_call` history when no web_search tool is declared (HTTP + Codex WS); `HEADROOM_WEB_SEARCH_HISTORY=0` off. Not on the unbuffered `--compression false` HTTP path |
| `18a588b3` | `2e81ef09` | `defer_loading` tools out of the drift tools hash, first-party upstreams only |
| `4ee61645` | `c66fe553` | Anthropic shell results under 2000 chars stay verbatim (`HEADROOM_SHELL_RESULT_MIN_CHARS`); 1.96% of Bash tokens in 10,574 captured results; not on OpenAI paths |

## Skipped

- `fc1ba891` document blocks priced by page: `Tokenizer::count_messages` has
  no production caller. End to end, a 4-page PDF in a `tool_result` counted
  `tokens_before=0`; no savings path prices base64 as text. The proxy prices
  documents itself in `handlers/count_tokens.rs` (decoded bytes / 750).
- `40cd7e75`, `534cec0e` batch-compression guards, YAML block scalars:
  modules have no production callers.

Ported for colleagues' traffic, though our logs (2026-10-26..11-02) had none:
0 Gemini, batch off, 0 `web_search_call`, 0 `defer_loading`, 0 `ccr_put_failed`.

## Found on the way, not upstream

- Chat prefix replay never fires when `messages[0]` is `role: system`, the
  usual OpenAI chat shape: `first_illegal_system_position`
  (`prefix_replay/compare.rs:174`) is Anthropic-only and declines every such
  turn with `system_adjacency_broken`. Fixed in `125a25af`; 0 native chat
  requests in our logs, Zen chat routes replay before translation.
- `--subagent-5m-ttl` (`d2d610f1`, default off): unmarked bodies with
  `cc_is_subagent=true` skip the 1h pin. Judge by a meter refit, not
  `vs_stock`.
- Open: ctx offload (`ctx_offload.rs` ~1269) turns an array `tool_result`
  over 20 KB into one text block, dropping sibling image/document blocks
  the store cannot give back.
- Tool search: see the 11-02 entry in `ideas/cache-safe-client-tool-search.md`.
- Response-cache lookup keys on the transformed body, store on the original
  (`proxy.rs:512,623` vs `:440`). Cache is off and rejected; noted only.

## ALREADY-THERE

`e11afdbd`, `a79def4d`, `19c91a87`, `b66c9c7d`, `cddd2fd6` (crash half),
`aebb729e`, `3c1fcbdf`, `75cfbba9`, `8f7b4199`, `755293dd`, `a25623109`,
`03590cd6`, `4d2dd62b`, `5f308e1c`, `c014bd63`, `f1035ac7`, `8c07cf4a`,
`70589537`.

## N/A

Redis CCR/compress sessions, SDK client, install/Docker/Windows, Copilot,
OpenCode, zcode, learn, evals, doctor, TOIN store load, semantic cache layer,
cache optimizer registry, learned TTLs, SQLite memory filters and history,
gateway `/v1/compress`, docs, CI, npm/pip bumps.
