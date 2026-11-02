# Upstream triage: `4c7acba5` → `fc1ba891` (113 commits)

Merged as `7ca63e6c` on `upstream-sync-2026-11-02`. Fetched 2026-11-02.
Classes as in [`upstream-triage-976aa714-4c7acba5.md`](upstream-triage-976aa714-4c7acba5.md):
PORT, ALREADY-THERE, N/A. Python paths map to `upstream-python/headroom/`.

Totals: **25 PORT (16 landed here, 9 deferred), 2 merged as Rust from upstream, 18 ALREADY-THERE, 68 N/A**.

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

## Deferred

Log counts are over all five proxy logs, 2026-10-26 to 11-02.

- `4ee61645` shell results under 2000 chars stay verbatim: costs savings,
  changes live bytes. The failure it fixes (the model fetching back a
  compressed short result) barely happens here: 6 `ccr_retrieval_call` in
  the week. Needs an A/B before it could pay.
- `2a70d128` drop `web_search_call` history with no web_search tool: 0
  `web_search_call` in the logs.
- `18a588b3` keep `defer_loading` tools out of the drift tools hash: 0
  `defer_loading` in the logs; inert while the launcher sets
  `ENABLE_TOOL_SEARCH=false` (`3253bfd9`).
- `fc1ba891` document blocks priced by page: `count_messages` has no
  production caller; the proxy prices documents in `handlers/count_tokens.rs`.
- `4bf1bf55` Gemini snake_case parts, `4fd9cddf` batch bypass header: real
  gaps, no traffic (no Gemini; `--enable-batch-api false`). `handlers/batch.rs`
  OpenAI/Google batch create ignore the bypass header too.
- `40cd7e75`, `534cec0e` batch-compression guards, YAML block scalars:
  modules have no production callers.
- `185454be` (non-Redis half): forward the original when the CCR store
  write fails, instead of a dangling marker. 0 `ccr_put_failed` in the logs.

## Found on the way, not upstream

- Chat prefix replay never fires when `messages[0]` is `role: system`, the
  usual OpenAI chat shape: `first_illegal_system_position`
  (`prefix_replay/compare.rs:174`) is Anthropic-only and declines every such
  turn with `system_adjacency_broken`.
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
