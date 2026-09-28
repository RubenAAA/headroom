# Upstream triage: fork point `964671d8` → upstream `v0.39.1` (138 commits)

Method: per-commit `git show` of the Python change + behavior-match against the
local Rust tree (`crates/headroom-proxy/`, `crates/headroom-core/`). `upstream-python/`
is a stale mirror, ignored. Verdicts: NEEDS-PORT / ALREADY-THERE / N/A.

Totals: **53 NEEDS-PORT, 18 ALREADY-THERE, 65 N/A.**

## NEEDS-PORT — oldest 21 (1)

| sha | behavior | local file |
|---|---|---|
| f734c573 | budget 429 gates on OpenAI/Gemini routes (`check_budget` has zero call sites in handlers) | `handlers/chat_completions.rs`, `handlers/gemini.rs`, `websocket_codex.rs` |


## NEEDS-PORT — proxy rate limiting / cache / savings (13)

| sha | behavior | local file |
|---|---|---|
| b9e8462a | scope OpenAI rate-limit bucket by `api-key` (HMAC identity) | `handlers/chat_completions.rs:146` |
| dfdc7251 | partition response-cache key by `upstream_base_url` | `semantic_cache.rs:87` |
| ab62b9e0 | never batch-compress already-forwarded small Responses outputs in cache mode | `compression/live_zone_responses.rs:59` |
| 138736c9 | enforce TPM via `check_tokens` in handlers (`check_tokens` has zero callers) | `handlers/chat_completions.rs:146` |
| 79681226 | TPM default unlimited; oversize request waits for full bucket then goes negative (today: refuses forever) | `config.rs:2869`, core `rate_limiter.rs:139` |
| ed08069b | budget checks O(1) via evicting deque + running aggregates | core `cost_tracker.rs:443` |
| a8bd9bec | rate limiter O(1): bounded LRU, evict-at-capacity | core `rate_limiter.rs:108` |
| 2c4dc446 | stamp `x-headroom-tokens-before/after/saved/model/transforms` on streaming (only gemini/batch do) | `openai/stream.rs` |
| b84c4c9f | keep `cache_control` when it is a JSON-Schema `properties` name during key strip | `semantic_cache.rs:27` |
| a2bf5ed1 | freeze system/tool bytes at/before last `cache_control` before compacting | `tool_schema_compaction.rs:529` |
| 1455f002 | report negative compression savings (no floor) + warning + net-negative latch | core `savings_tracker.rs:709` |
| 1cb779e1 | carry exact per-checkpoint cache-read cost in history rollups | core `savings_tracker.rs:1393` |
| c766bdb2 | track lifetime output spend as bill-share denominator | core `savings_tracker.rs` |

## NEEDS-PORT — proxy protocol / SSE / tool-search (18 + 1)

| sha | behavior | local file |
|---|---|---|
| 871bbde3 | 501 for Anthropic batch routes on Copilot targets | `handlers/batch_anthropic.rs` |
| 55c78dea | include `custom_tool_call` names in Responses exclude-tools map | core `transforms/live_zone/openai_responses.rs:143` |
| f12e3fbe | repair client-side `tool_result` carrying `tool_reference` blocks | `tool_search_deferral.rs:519` |
| 690fb251 | enforce wire-size `MAX_REQUEST_BODY_SIZE` while streaming | `body.rs:192`, `proxy/app.rs:76` |
| 7c3cbc82 | fail closed (error JSON) on unresolved memory project, not silent fallback | `memory/router.rs:476` |
| 000fefce | buffered-CCR SSE recovery must not relay ping-only stream as success | `proxy/sse.rs:217` |
| 6c0e817b | discard partial SSE tail at finalize instead of flushing `"\n\n"` (partial) | `proxy/sse_anthropic.rs:91` |
| c0292984 | output shaper: pinned level survives cache mode + warn-once helper | `output_shaper.rs:229` |
| 665b73df | detect all client-deferral wire shapes + descend `namespace` tool groups | `tool_search_deferral.rs:24,174` |
| a9757c9c | core-tools override authoritative (legacy env, extras, recurrent warning) (partial) | `tool_search_deferral.rs:71,99` |
| b36e59a8 | count ceiling-stopped streams exactly via request ceiling | `proxy/sse_anthropic.rs:322` |
| 46ac52d3 | `GET /transformations/feed?include_messages=0` + per-request cache-split fields | `proxy/app.rs`, `request_logger.rs` |
| 12c15796 | preserve Claude Code auto-mode protocol (headers, safeguard payload, opaque SSE) | — (no `anthropic_wire` equivalent) |
| ca02f28c | keep transport context when `str(e)` is empty | `bedrock/invoke.rs`, `invoke_streaming.rs` |
| c81378c8 | normalize xAI `/v1/models` payload; sanitize forwarded headers | `handlers/local_model.rs:29` |
| a8ae0a6f | admit container-host gateway on compress routes | `loopback_guard.rs`, `proxy/app.rs:222` |
| 5cb87bc3 | CCR payload previews default OFF + owner-only (0600) log files | `sse/anthropic.rs:771` |
| ecb5e5af | parse `cmd` from JS object literals; unknown `cmd` ⇒ treat as read | core `transforms/read_protection.rs:177` |
| c7ebaedd | `headroom_headroom_retrieve` alias resolves to `headroom_retrieve` (OpenCode double-prefix) | core `tool_exclusion.rs:99` (`tool_name_aliases` has no such branch; `ccr_retrieve_aliases()` has zero callers; repair in `ccr_retrieve_repair.rs:79` compares exact name only) |

## NEEDS-PORT — transforms / memory / relevance (16)

| sha | behavior | local file |
|---|---|---|
| 6c9aef1d | detect space-aligned cmd output as TABULAR, keep tables out of Kompress | core `transforms/content_detector.rs`, `content_router.rs:1528` |
| c1fc84ad | CCR sentinel for dropped scalar-array items | core `smart_crusher/crusher.rs:819` |
| 6feb1fb2 | detect CMTrace `<![LOG[` logs as BUILD_OUTPUT | core `transforms/content_detector.rs:317` |
| 195910b2 | defer embedded-JSON splice so HTML extraction runs first (embedded-JSON pass itself never ported — larger than the diff) | core `content_router.rs:2075` |
| 62d1cc08 | describe *what* CCR dropped (error labels, source files), not just counts | core `transforms/log_compressor.rs:1298` |
| 72217242 | drop dead `crush_object` clone on object path (perf, behavior-neutral) | core `smart_crusher/crushers.rs:396` |
| 7ad9f809 | keep null vs missing vs `""` vs `"null"` distinct in CSV compaction | core `smart_crusher/compaction/formatter.rs:354` |
| d971f7c3 | timestamped log rows are never grep matches | core `transforms/content_detector.rs:839` |
| 7f2766ca | Kompress deadline spans whole request, not per block (no deadline machinery at all) | core `kompress.rs`, `kompress_remote.rs` |
| 1a7c5bbf | .xls ints above 2^53 render as float, bound inclusive (`<` vs `<=`) | core `spreadsheet_ingest.rs:67` |
| bf290ba9 | Kompress must-keep: boolean connectives and/or/nor/xor | core `kompress.rs:121` |
| 6880984b | BM25: BTreeMap so terms order once, not per doc (perf) | core `relevance/bm25.rs:90,121` |
| 2d10b10a | LRU-bound feedback tool-pattern map (outer map unbounded) | `compression_feedback.rs:235` |
| e9114964 | numeric/boolean metadata filters match, not just strings | `memory/models.rs:38` |
| 3f3cf19e | `extract_system_prompt` concatenative, not first-match | `memory/router.rs:521` |
| ca54c2ab | fall back when forced Kompress is cold (`is_ready` never consulted) | core `content_router.rs:690` |

## NEEDS-PORT — launcher / savings basis / pricing / hooks (4)

| sha | behavior | local file |
|---|---|---|
| c4df2dde | SIGHUP trap on shared-proxy wrap paths so watcher/proxy don't leak | `contrib/claude-launcher`, `contrib/restart-headroom.sh` |
| e92cccca | new-input savings basis beside whole-wire figure (/stats, ledger) | `handlers/stats.rs`, `request_logger.rs` |
| b0dfa334 | DeepSeek V4.1-Flash peak/off-peak card + `deepseek-flash` rename/alias, vision=true | core `pricing.rs:289`, `data/model_prices_and_context_window.json` |
| d17ac6a1 | `protect_messages` hard per-message compression veto hook | `turn_hooks.rs` |

## ALREADY-THERE (11)

| sha | behavior | local evidence |
|---|---|---|
| 11c7320b | wait for old process to stop before restart | `contrib/restart-headroom.sh:293` |
| 994a89f1 | bound request-log payloads (no payloads logged at all) | `request_logger.rs:91` |
| b0c19a25 | reject unauthenticated public binds | `main.rs:308`, `proxy_auth.rs:87,114` |
| 76ec363a | C#/PHP AST-compressed | `headroom-core/Cargo.toml:233,242` |
| deca575c | replay last-forwarded OpenAI prefix over restored originals | `prefix_replay/compare.rs:558`, `tracker.rs:105` |
| a6a9cef9 | no tool-desc `cache_control` skip | `tool_schema_compaction.rs:529` |
| a102e05b | pin validated DNS addrs to socket | `proxy/egress.rs:272`, `proxy/upstream.rs:81` |
| 85f9e01d | deferral mode tags client/headroom/none, savings estimated | `request_transforms.rs:191`, `handlers/stats.rs:122` |
| 70edfb46 | BM25 shared query counted once per batch | core `relevance/bm25.rs:200` |
| 2fed685e | no eager TTL sweep on store() | core `ccr/backends/sqlite.rs:180,234` |
| 5ccec67f | None message content tolerated | `ctx/extract.rs:76` |

## ALREADY-THERE — oldest 21 (7)

| sha | behavior | local evidence |
|---|---|---|
| 44b8e7ba | `get_recent` skips deep-copy of heavy payloads (no bodies logged at all) | `request_logger.rs:93,168` |
| 9263b420 | dense machine-line elision + empty-HTML fall-through | core `transforms/dense_line_elider.rs:18`, `content_router.rs:1849,2075` |
| 334317db | csv-over-limit cell passes table through (manual line-split, no field limit) | core `transforms/tabular_ingest.rs:76` |
| 545f5441 | SmartCrusher preserves object fields; original-bytes path | core `smart_crusher/crusher.rs:494,1434` |
| 6f404de5 | skip buffered stream-CCR reshape for OpenCode Zen | `openai_buffered_ccr.rs:89,93` |
| 85fac8c3 | rejected turns skip savings funnel | core `request_outcome.rs:600` |
| 89a58fd1 | removed tokens priced at cache mix, not flat list | core `request_outcome.rs:228`, `cost_tracker.rs:218` |

## N/A — oldest 21 (12)

`be00a798` (gateway-turn Responses shape — subsystem never ported),
`bc21c937` (anyio CVE bump, `uv.lock` only), `67eb910e` (beacon v2 — never
ported), `a7858fd2` (kompress download-retry backoff — gate never ported),
`e326dd7b` (Cargo.lock bump), `13536dbc` (codeql v3→v4), `2d5909c3` (npm
lockfiles), `abde3e6e` + `3c1a9015` (CRLF in learn writers — `fs::write` is
byte-exact; 3c1a9015 is an empty duplicate, same tree), `261796f9` (.xls
parity — deliberately never ported), `ebc0b363` (SDK-TS only), `94206e26`
(release 0.38.0).

## N/A — releases, CI, deps, SDK, tests, unported subsystems (54)

Releases: d13e1966 (0.39.1), 66f42617 (0.39.0). Giant test-only rename:
0ad5e68c. Test-only: e64b9f58, 2e2758de, 00896a7c, c32b3155, 21d166b7.
CI/deps/tooling: c22cd1f1, 23c20545, b9ded1ae, 54357ff6, fbbe8b8e, 63113329,
56edfbc9, fa58d773, 62f775d5, a37d7343, 7f44a644, b2d92753, d90dadf2, a28dd4ea.
SDK-only: c3e8a1ee, 63676e6c, 95cbbb81, b2b58474, ca4b47a3. Python-CLI /
dashboard / wrap with no local counterpart: df8ebdf9, f3d95ee2, 9a111099,
a4cb2bc0, fc5a09e1, b9585094, fa9edb3a, a29162ba, ff60d576, 19ddfe1a, d05bacb4,
26a2c493. No local subsystem: 577336d2 + 0c19244e (litellm), 0024b574
(opencode/plugins), 558b066b (MCP server), b7769831 (graph installer),
1a6f9419 (graph store), 3aa50128 (`_merge_similar`), 3e36aefb (memory CLI/FTS5),
cc6a07cd (provider TokenCounters), 9b8cae84 (cred-file writes), 6e712672
(sys-prompt compaction step), 5baa439e (urllib ALPN), 7ce2580f (tz-aware
expiries — u64 epochs), 2fed685e already-there (see above), 34da21e8
(`tokenizer.json`-only fetch, no remote code), e8a51098 (xlrd vs calamine).
