# Upstream triage: `964671d8` → `v0.39.1` (138 commits)

Redone 2026-09-28 after the merge `e9db17e6`; replaces a first pass that
counted upstream's own Rust commits as ports and missed behaviour already
in `crates/`. Each of the 99 commits touching `headroom/` was checked
against the merged tree with `git show` plus a file:line in `crates/`.
The other 39 are Rust (arrived with the merge), releases, CI, dependency
bumps, SDK or tests.

Totals: **30 PORT, 3 PORT-NEW, 5 MERGED,
26 ALREADY-THERE, 74 N/A.**

- PORT: a Rust counterpart exists and lacks the behaviour.
- PORT-NEW: the Rust side has the code only as dead code, so the port
  includes wiring it in.
- MERGED: upstream changed Rust directly; the merge brought it.
- `cache` = yes when the change alters bytes forwarded upstream.

The Python reference for every row is `upstream-python/` at `v0.39.1`
(`git show <sha>` shows the diff and its tests).

## Decisions (2026-09-28)

- `7c3cbc82` (memory refuses to run when it cannot resolve the project):
  N/A. Keep the fork's fallback to the shared user partition
  (`memory/router.rs:90-103`).
- `deca575c` (prefix replay for `/v1/chat/completions`) and `12c15796`
  (Claude Code auto-mode safeguards): port. The maintainer's own logs show
  no chat-completions traffic, but other people run this fork.
- `a9757c9c`: ALREADY-THERE; the hourly "client tool search disabled"
  warning and the legacy `HEADROOM_TOOL_SEARCH_CORE` alias are still
  absent and not planned.

## Port order

### 1. Safety and security

Small, no forwarded bytes change.

| sha | kind | cache | behavior | Rust target | note |
|---|---|---|---|---|---|
| `9b8cae84` | PORT | no | Create credential/CCR files private (0600, O_EXCL/O_NOFOLLOW or temp+rename) instead of writing then narrowing | core ccr/backends/sqlite.rs:59 Connection::open creates ccr.db at umask mode (live via proxy ctx/offload_store.rs:104, dir from create_dir_all at :101); bin/headroom_cli/copilot_auth.rs:229-237 truncates and writes an existing token file before set_permissions | S. Codex-recovery part N/A (no Rust counterpart). Same gap likely on ctx-offload-index.db and project stores (not checked) |
| `5cb87bc3` | PORT | no | CCR retrieval payload previews off by default (opt-in HEADROOM_LOG_PAYLOAD_PREVIEW=1); runtime log files created owner-only 0600, rotated backups too | Preview half ALREADY-THERE: Rust never logs CCR payload content (sse/anthropic.rs:771 payload_preview is a 96-byte SSE parse-error snippet, not CCR). Gap: contrib/claude-launcher:243-274 and contrib/restart-headroom.sh:227 create ~/headroom-proxy.log via shell redirect/mv at the umask; the live files are 0644 | S (shell change: umask 077 around the redirect, chmod 600 on rotated files). The old doc pointed at sse/anthropic.rs:771, which is the wrong spot |
| `26a2c493` | PORT | no | Binary downloads with no SHA-256 pin are refused (delete the file) unless HEADROOM_BINARIES_ALLOW_UNVERIFIED is set, which warns loudly | crates/headroom-proxy/src/bin/headroom_cli/tools.rs:300-303 verify_sha256 returns Ok when the pin is missing (fails open); called at :463 | Disagree with old doc (N/A): the fork ships `headroom tools install` in Rust with the same fail-open. All registry assets are pinned today, so it bites only on off-registry/version-override fetches. S. |
| `000fefce` | PORT | no | Buffered CCR must not relay a ping-only SSE reply to a de-streamed request as a successful 200; rebuild it and 502 when there is no terminal event | Anthropic path doesn't apply (Rust streams CCR through sse/ccr_stream.rs with no buffered flip). Same gap on the Responses buffered path: proxy/sse.rs:275-280 passes the collected SSE through with is_sse=true when there is no response.completed, so a keepalive-only body reaches the client as 200 | S. The old doc cited sse.rs:217, which is the Anthropic rewriter. The real target is the Responses else-branch at :275. Keep passing real error events through; 502 only when there is no terminal and no error event |

### 2. Rate limiting and budget (adopted)

Land 79681226 with or before 138736c9. The limiter ships on with a 100k TPM default and refuses any request larger than the bucket forever, so enforcing TPM alone would 429 every Claude Code turn over 100k tokens. `0` is documented as unlimited but denies everything; fix that in the same change. Today only chat completions and responses check RPM; `/v1/messages` and Gemini check nothing, and `check_budget` has no caller. Tests from 00896a7c state the budget contract.

| sha | kind | cache | behavior | Rust target | note |
|---|---|---|---|---|---|
| `79681226` | PORT | no | TPM defaults to unlimited; a request larger than the bucket waits for a full bucket, then is charged in full (balance goes negative) instead of being refused forever | core src/proxy/rate_limiter.rs:141-161 denies any token_count > tpm forever (bucket caps at tpm); config.rs:2879 defaults HEADROOM_TPM to 100000; config.rs:2564-2566 claims 0 = unlimited but rate_limiter.rs treats 0 as deny-all with infinite wait (same for RPM) | Agrees with old doc. S. Land before or with 138736c9 |
| `138736c9` | PORT | no | Charge each request's input tokens against the per-key TPM bucket before forwarding; 429 + Retry-After when over | core src/proxy/rate_limiter.rs:141 check_tokens has zero callers; proxy handlers/chat_completions.rs:146 check_rate_limit calls only check_request (used at chat_completions.rs:185, responses.rs:452); /v1/messages forward path and handlers/gemini.rs have no limiter call at all (not even RPM) | Agrees with old doc. M. Must land with or after 79681226: rate limiting defaults ON (config.rs:2872) with HEADROOM_TPM=100000 (config.rs:2879), so wiring TPM alone would 429 every >100k Claude Code turn forever |
| `a8bd9bec` | PORT | no | Rate limiter buckets bounded by LRU with O(1) evict at capacity, no stale-table scan per request | crates/headroom-core/src/proxy/rate_limiter.rs:108-115 (and check_tokens): above MAX_RATE_LIMITER_BUCKETS every call scans all buckets and removes only >10-min-stale ones, so the map stays unbounded | Rate limiting is adopted. Also: check_rate_limit is wired only in handlers/chat_completions.rs:185 and responses.rs:452, not /v1/messages. S |
| `b9e8462a` | PORT | no | OpenAI rate-limit bucket keyed by an HMAC of Authorization or api-key header, not a raw/truncated credential | handlers/chat_completions.rs:147-152 keys on the raw Authorization value, ignores `api-key` (all such clients share "anonymous"), and holds the full credential as a map key | Agrees with old doc. S |
| `5ff4ea1e` | PORT | no | Rate-limited counter split by source (headroom limiter vs upstream 429) and failed counter labelled by provider, across Prometheus, persistent metrics and /stats | Prometheus part present: crates/headroom-proxy/src/observability/proxy_counters.rs:587,605; proxy/outcome.rs:203-207 records source=upstream. Missing: rate_limited_by_source in core persistent_metrics.rs:1261 (records by provider only) and in handlers/stats.rs; no source=headroom call site because the rate limiter is not enforced yet | Not in old doc. S; the headroom-source call sites come with the rate-limit adoption. |
| `f734c573` | PORT-NEW | no | Budget limit returns 429 on OpenAI chat/responses, Gemini, and closes the Codex WebSocket (1008) when over budget; Anthropic path refactored to the same check | crates/headroom-core/src/cost_tracker.rs:473 check_budget has zero callers anywhere (Anthropic included); flag exists at crates/headroom-proxy/src/config.rs:1652. Targets: proxy/forward.rs (Anthropic), handlers/chat_completions.rs:176, handlers/responses.rs:443, handlers/gemini.rs:430, websocket_codex.rs:1171 (first frame + per-frame) | Agree it needs porting; PORT-NEW because enforcement is dead code on every route, not just OpenAI/Gemini. M (needs a budget_denial_detail equivalent too). |
| `ed08069b` | PORT-NEW | no | Budget checks read running totals for the current hourly/daily/monthly window from an evicting deque instead of summing all records | crates/headroom-core/src/cost_tracker.rs:443-470 (get_period_cost scans every entry); check_budget at :473 has no caller in crates/headroom-proxy, so budget is never enforced | Budget is adopted, but enforcement is unwired: port this together with the wiring. The Rust tracker also has no measured/estimated split. S for the O(1) part |

### 3. Cache-sensitive rewrites

Each changes bytes sent upstream. Read `docs/notes/learnings/` and the matching `cache_stabilization/` module first, and check the cache-health numbers after deploy.

| sha | kind | cache | behavior | Rust target | note |
|---|---|---|---|---|---|
| `f12e3fbe` | PORT | yes | Repair client-side tool_result blocks whose content lists tool_reference entries for tools no longer in `tools` (drop them; placeholder text if none left) | crates/headroom-proxy/src/tool_search_deferral.rs:571 (strip_unsupported_blocks only inspects server result types via server_result_family; plain tool_result is skipped) | Agrees with old doc. Rewrites history bytes, so keep the placeholder constant for byte stability. S-M |
| `c7ebaedd` | PORT | yes | Treat `headroom_headroom_retrieve` (OpenCode double prefix) as headroom_retrieve so retrieved content is not compressed again | crates/headroom-core/src/tool_exclusion.rs:99 (tool_name_aliases has no such branch); openai_responses.rs:149 matches only exact or `__headroom_retrieve` suffix | Agrees with old doc. Only affects OpenCode clients. It changes compression of live-zone retrieve results. S |
| `665b73df` | PORT | yes | Treat the client as already deferring when any tool has defer_loading:true, or a search meta-tool in any spelling (type/name tool_search, tool_search_tool, tool_search_tool_regex, composio_search_tools), including one level inside type:namespace groups; warn hourly on deferred tools with no search tool | crates/headroom-proxy/src/tool_search_deferral.rs:174-183 (client_uses_tool_search checks only the `tool_search_tool_` prefix; misses exact `tool_search_tool`, bare `tool_search`, defer_loading, namespace) | S. Changes whether Headroom injects its own search tool and defers, so it changes the tools prefix. The orphan warning is optional |
| `a2bf5ed1` | PORT | yes | Tool-description compaction skips when any tool carries cache_control; system-prompt compaction leaves blocks at or before the last cache_control untouched | crates/headroom-proxy/src/tool_schema_compaction.rs:529 compact_tool_descriptions has no marker guard; only caller handlers/chat_completions.rs:125 (opt-in HEADROOM_TOOL_DESC_MAX_CHARS, default off). System compaction does not exist in Rust, so that half is N/A | Agree with old doc. Note: old doc lists a6a9cef9 as ALREADY-THERE at the same line, which contradicts this row. S. |
| `9263b420` | PORT | yes | Elide dense whitespace-free lines (minified JS, base64) with a CCR marker, after text-returning strategies and as passthrough fallback; an HTML page that extracts to whitespace falls through instead of replacing the block | mostly there: core transforms/dense_line_elider.rs, content_router.rs:1849-1895, :2104, :2125, :2154. Gaps: dense_elide_after (content_router.rs:1849) omits CodeAware (upstream includes CODE_AWARE; code-aware is live in the fork's flags); HTML arm (content_router.rs:2084) tests `!compressed.is_empty()` not trim, and html_extractor.rs:217 does not trim text_content | Disagree with old doc (ALREADY-THERE). S. Live-zone tool-result bytes only, never frozen prefix. |
| `d971f7c3` | PORT | yes | A timestamped log row is never a grep search-result line, so logs don't route to the lossy SearchCompressor | crates/headroom-core/src/transforms/content_detector.rs:846 (is_search_result_line has no timestamp guard); reuse lossless_compaction.rs:61 timestamp_row_re (make pub(crate)) | S. Changes routing, so it changes compressed output |
| `6c9aef1d` | PORT | yes | Detect space-aligned command output (ls -l, ps, docker ps) as tabular, keep READs of it byte-exact, and never send tabular content to Kompress | core transforms/content_detector.rs:371-418 has no fixed-width detector (falls to PlainText); core transforms/content_router.rs:1528 maps ContentType::Tabular straight to Kompress; tabular_ingest.rs:134 parse_fixed_width exists but needs the width<2 bail | Agrees with old doc. M. Changes compressed tool-output bytes on first forward |
| `12c15796` | PORT | yes | Claude Code auto mode: on turns with `safeguards` or a dangerous-tool-use-* beta, forward client anthropic-beta/version exactly (no sticky union, memory or hook beta), keep unknown SSE events/fields (safeguard_results) across CCR re-synthesis, and keep classifier payloads out of logs | cache_stabilization/beta_sticky.rs unions betas unconditionally (default enabled, config.rs:566); proxy/forward/presend.rs:290 appends memory beta; sse/ccr_stream.rs rebuilds the turn from AnthropicStreamState on a retrieval round, dropping unknown events/fields of the continuation; no `safeguards` awareness anywhere in crates | Agrees with old doc that nothing exists. M-L. Cache-sensitive because anthropic-beta is part of the prefix identity (usage_observer.rs:3221); non-retrieval streams already pass unknown events through live |
| `deca575c` | PORT | yes | OpenAI /v1/chat/completions keeps last turn's forwarded bytes (replay over restored originals) so the provider prefix cache hits | Prefix replay is gated to AnthropicMessages at crates/headroom-proxy/src/proxy/forward/stages.rs:106-110. The chat live zone (core live_zone/openai_chat.rs header) compresses only the latest tool/user message, so the next turn sends it raw and the prefix changes | Real bug for direct chat-completions clients; not on the fork's Claude Code or routed Codex paths (routed/transforms.rs:940 replays the Anthropic-shape body). Overlay/canonicalize are Anthropic-shaped. M-L |
| `c0292984` | PORT | yes | Output shaper in cache mode steers at the pinned/startup level instead of forcing 0; default level 2; controller/learned profile skipped in cache mode | crates/headroom-proxy/src/output_shaper.rs:238-251 (shape_request_for_mode forces level 0 in cache mode); default already 2 at config.rs:1448 | S: Rust's level is always fixed at startup (verbosity_controller.rs isn't wired), so cache mode can just pass the configured level through. Adds the steering block to system in cache mode (byte-stable per level). No effect on this machine: the flags file runs --mode token with the shaper off |
| `bf290ba9` | PORT | yes | Kompress must-keep regex also pins lowercase and/or/nor/xor so conditions keep their connectives | crates/headroom-core/src/transforms/kompress.rs:134-136 (negation list has no and/or/nor/xor) | S (one regex alternation plus a test). Low value here: the flags file ships --enable-kompress false / --disable-kompress true |

### 4. Response cache key

Cache key only; forwarded bytes do not change.

| sha | kind | cache | behavior | Rust target | note |
|---|---|---|---|---|---|
| `b84c4c9f` | PORT | no | Response-cache key strips cache_control directives but keeps a JSON-Schema property that happens to be named cache_control | proxy src/semantic_cache.rs:27 drops every `cache_control` key at any depth, including inside `properties`, so two different tool schemas share a key | Agrees with old doc. S. Cache key only, not forwarded bytes |
| `dfdc7251` | PORT | no | Include the resolved upstream base URL in the response-cache key so two gateways never serve each other's cached replies | proxy/forward/stages.rs:33 (lookup) and proxy/forward/response.rs:192 (store) key on model+messages+extra only; proxy/upstream.rs:73 lets x-headroom-base-url change the upstream per request | Agrees with old doc. S |
| `2d10b10a` | PORT | no | Cap the compression-feedback per-tool map at 1024 names with LRU eviction | crates/headroom-proxy/src/compression_feedback.rs:235 (HashMap), inserts at :272 and :319, only cleared wholesale at :476 | Agrees with old doc. S |

### 5. Savings and stats

Accounting only.

| sha | kind | cache | behavior | Rust target | note |
|---|---|---|---|---|---|
| `89a58fd1` | PORT | no | Price removed tokens by prompt region: live-zone compression at the write/uncached mix, tool-schema deferral (prefix) read-first against the full mix; 1h writes at the 1h rate; ledger compaction stops rewriting the file on every append | live-zone half and 1h rate present: core request_outcome.rs:206-266, cost_tracker.rs:402-413. Gaps: (1) proxy/outcome.rs:232-240 prices headline_tokens_saved (compression + tool-schema tags) entirely at the live-zone mix, so deferral savings are not priced read-first (upstream measured ~10x overstatement on warm turns); (2) core savings_ledger.rs:578-583 maybe_compact has no growth threshold, so once the ledger is >8 MB with all events in retention every append rewrites it under flock | Disagree with old doc (ALREADY-THERE). M. OTel/dashboard/CLI parts N/A. |
| `1cb779e1` | PORT | no | Savings history rollups carry cache_read_tokens_delta, cache_savings_usd_delta and cache_read_cost_usd_delta (read cost priced per checkpoint model; null if the model is unknown) | crates/headroom-core/src/savings_tracker.rs:1565 build_rollup emits no cache fields, though HistoryEntry already stores cache_read_tokens/cache_savings_usd (savings_tracker.rs:453-458) | Agree. S. |
| `1455f002` | PORT | no | Savings are no longer floored at zero; throttled `savings_negative` warning; edge-triggered `net_tokens_negative` warning when cache-bust tokens overtake compression savings; /stats compression_vs_cache.net_is_negative | core cost_tracker.rs:1101-1107 already reports signed net_tokens but no net_is_negative flag and no warning; savings_tracker.rs:89-94 coerce_float floors, used for compression_savings_usd at :1314, :1235. Rust compression dollars cannot go negative today (priced from clamped tokens), so the floor itself is moot | Agree it needs porting, but the useful part is smaller: net_is_negative plus the edge-triggered bust-vs-saved warning. S. |
| `c766bdb2` | PORT | no | Savings tracker keeps lifetime total_output_cost_usd (emitted output priced at output rate) plus per-bucket deltas, recovered on restart | crates/headroom-core/src/savings_tracker.rs:356 (lifetime has total_input_cost_usd and output_savings_usd, no output cost); estimate_output_savings_usd at :174 is the rate to reuse | Agrees with old doc. M |
| `e92cccca` | PORT | no | Show the new-input savings basis (saved over uncached plus cache-write) beside the whole-wire figure in the ledger and `headroom savings` | crates/headroom-core/src/savings_ledger.rs:163 (SavingsEvent has no new_input/deferred fields); headroom_cli savings command; /stats has no new_input_savings_percent anywhere in crates/ | Dashboard half is N/A. It depends on an earlier upstream /stats field the fork also lacks. Low value next to /cache-health and conversation_savings.rs. M |
| `2c4dc446` | PORT | no | Streaming responses carry x-headroom-tokens-before/after/saved, x-headroom-model, x-headroom-transforms like buffered ones do | Only gemini.rs:617-639,984-1001 and batch.rs:736 stamp them; the main Anthropic/OpenAI forward path stamps them on neither buffered nor streaming responses. Target: response header assembly in crates/headroom-proxy/src/proxy/forward/response.rs (near wrap_buffered_success_body :390) and the SSE response builder | Agrees with old doc; the gap is wider than streaming. Low value for Claude Code (ignores them). S |
| `3f3cf19e` | PORT | no | Project/workspace resolution must see the cwd in Claude Code's <env> user message, not stop at the system field | Memory and CCR already use extract_project_prompt/extract_opening_prompt, which scan messages (crates/headroom-proxy/src/memory/router.rs:568-655; callers forward/memory.rs:455, memory_continuation.rs:72, ccr_expansion.rs:21,47). Remaining gap: crates/headroom-proxy/src/proxy/forward/response.rs:684 still calls system-only extract_system_prompt for the outcome project label and set_current_project (compression feedback, audit) | Mostly already there under a different design; the old doc's router.rs:521 target is the wrong fix. Residual: swap one call at response.rs:684. S |

### 6. Deferred

Kompress is off in the flags file.

| sha | kind | cache | behavior | Rust target | note |
|---|---|---|---|---|---|
| `7f2766ca` | PORT-NEW | yes | Kompress inference deadline (HEADROOM_COMPRESSION_DEADLINE_MS) bounds the whole request, not each block | crates/headroom-core/src/transforms/kompress.rs and content_router.rs have no kompress deadline at all; only the outer compression_quarantine.rs timeout-debt exists | M: needs the deadline machinery first (per-chunk check, env, shared origin per apply). A deadline makes output depend on timing, so it is cache-sensitive by nature. Low value while kompress is off in the flags file |

## ALREADY-THERE

| sha | behavior | evidence | note |
|---|---|---|---|
| `44b8e7ba` | /stats get_recent stops deep-copying request/response payloads that it drops anyway | crates/headroom-proxy/src/request_logger.rs:93-110 (RequestLogEntry holds no message bodies), :168 get_recent clones slim entries | Agree. |
| `334317db` | A CSV with one cell over Python's csv field-size limit passes through verbatim instead of crashing the ingest | crates/headroom-core/src/transforms/tabular_ingest.rs:76-91 parse_csv splits lines manually; there is no field limit and no error path | Agree. Rust compresses such a table rather than passing it through, but the failure the fix addresses cannot happen. |
| `6f404de5` | Streaming /responses to OpenCode Zen is not reshaped to buffered stream:false (Zen 403s reshaped requests) | crates/headroom-proxy/src/openai_buffered_ccr.rs:79-96, called from proxy/forward/buffered_send.rs:436-441; also sse/ccr_stream.rs:1228, routed/quirks.rs:41 | Agree. |
| `85fac8c3` | Any upstream >=400 skips the savings/cost funnel; 429 counts as rate-limited, the rest as failed | crates/headroom-core/src/request_outcome.rs:600-603; crates/headroom-proxy/src/proxy/outcome.rs:188-213 | Agree. |
| `0ad5e68c` | Retired Claude 3/3.5 Sonnet ids alias to a still-priced Sonnet-tier model instead of falling to the GPT-4o default | crates/headroom-core/src/pricing.rs:233-234 prices claude-3-5-sonnet / claude-3-sonnet directly at Sonnet tier via longest-prefix lookup (pricing.rs:306) | Old doc called it test-only N/A; it does carry a pricing fix, but the fork's static table already has it. |
| `85f9e01d` | Stripping the first-party tool-search tool for third-party upstreams also clears defer_loading; deferral savings marked estimated; client-deferral detection by type or name; tool_search_mode tag; HEADROOM_TOOL_SEARCH_CORE_TOOLS | crates/headroom-proxy/src/tool_search_deferral.rs:174 (detect), :218-250 (strip + undefer), :71 (core tools env); proxy/forward/presend.rs:166 (mode tag); handlers/stats.rs:122-128 (estimated). OpenAI Responses injector absent in Rust, so its realized flag has nothing to change | Agree. |
| `34da21e8` | HF tokenizer loader never runs remote code and only fetches allowlisted repos (request-body model names could name any Hub repo) | crates/headroom-core/src/tokenizer/hf_impl.rs loads tokenizer.json only (no code); registry.rs:212 try_register_hf has zero callers, so no request-driven Hub fetch | Old doc said N/A; the security property holds in Rust, so ALREADY-THERE. |
| `a9757c9c` | Core-tools override replaces the default set, trims spaces around names, always keeps ToolSearch resident; the disabled-search warning re-arms hourly; legacy env name honoured | crates/headroom-proxy/src/tool_search_deferral.rs:99-123 (trim at :106, toolsearch floor at :117 and :306); Rust has no OpenAI deferral so no `terminal` union to remove | Disagree with old doc (partial PORT): the substantive fixes are present. Left out: legacy alias HEADROOM_TOOL_SEARCH_CORE (it came from the Python plugin, which the fork doesn't ship) and the hourly hint. The hint (claude_code_tool_search_inactive, pre-fork #753) was never ported, so there is nothing to re-arm. Optional S if wanted |
| `a6a9cef9` | Compact tool schemas/descriptions even when a tool carries cache_control (compaction is deterministic, so the prefix stays stable) | crates/headroom-proxy/src/tool_schema_compaction.rs:529-543 (compact_tool_descriptions has no marker guard); proxy/request_transforms.rs:412 (compact_tools has none either) | Agree with old doc: Rust never had the guard |
| `ecb5e5af` | Parse cmd from Codex exec JS object literals ({cmd: "cat f"}); an unknown cmd counts as a read, so the output is protected | crates/headroom-core/src/transforms/live_zone/openai_responses.rs:163-172: custom_tool_call_output is never a compression candidate on any layer (no other crates/ match), so exec outputs are already never compressed | Disagree with old doc (PORT at read_protection.rs:177). If custom_tool_call_output ever becomes a candidate, port the JS-literal parse and None-as-read into read_protection.rs:177 first (M) |
| `994a89f1` | Request log keeps message payloads only on the newest 100 entries; periodic malloc_trim on by default on Linux | crates/headroom-proxy/src/request_logger.rs:93-110 (RequestLogEntry holds no message payloads); main.rs:27-28 uses mimalloc | Disagree with old doc's framing: nothing to bound. The glibc trim doesn't apply under mimalloc |
| `195910b2` | The embedded-JSON splice must not pre-empt HTML extraction; a blank extraction falls through | crates/headroom-core/src/transforms/content_router.rs:2075-2092 (no embedded-JSON pre-pass exists in Rust, so nothing pre-empts HTML) | Agree the pre-pass was never ported. One-line nit: :2085 checks !compressed.is_empty(), while Python checks .strip(). A whitespace-only extraction would be adopted in Rust (0 tokens < original). S, cache-sensitive if fixed |
| `6feb1fb2` | Detect CMTrace (SCCM/Intune `<![LOG[`) logs as BUILD_OUTPUT | crates/headroom-core/src/transforms/content_detector.rs:329-331 (+ tests at :1873-1905); upstream's own Rust change came in with the merge | Disagree with old doc (PORT): upstream's Rust commit, already merged |
| `c1fc84ad` | SmartCrusher appends a "… N more items <<ccr:HASH>>" sentinel (full array stored in CCR) when string/number/mixed arrays lose items | crates/headroom-core/src/transforms/smart_crusher/crusher.rs:830,838,848,1362 (append_scalar_drop_sentinel, arrived with the merge) | Upstream's own Rust commit, merged. Old doc lists it as a port: wrong. Heads-up: it changes crushed output bytes for scalar arrays when enable_ccr_marker is on |
| `690fb251` | Enforce the wire-size body cap while streaming the request in, not only after buffering | crates/headroom-proxy/src/proxy/forward/buffered_send.rs:24-80 (Content-Length pre-check plus http_body_util::Limited frame loop, 413); proxy/app.rs:76,527 DefaultBodyLimit; body.rs:192 decompressed cap | Old doc calls it a port: disagree |
| `11c7320b` | Restart waits for the old process to stop before starting the new one | contrib/restart-headroom.sh:251-261 (waits up to 10s for the port to free, then SIGKILL, then installs and starts) | The Python `install restart` is N/A; the fork's restart script already waits |
| `b0dfa334` | DeepSeek V4.1-Flash rate card, deepseek-flash rename/alias, peak/off-peak tiers by Beijing time | crates/headroom-proxy/data/model_prices_and_context_window.json (deepseek-flash, deepseek/deepseek-flash, deepseek-v4-flash at 3e-07/1.2e-06, max_output 393216, merged); read by compression/model_limits.rs:69 | The only Rust-side part (the vendored JSON) is merged. The peak/off-peak tier logic is Python pricing for a provider the fork does not route (core pricing.rs:289 leaves deepseek-* on the blended fallback), so N/A. Old doc calls it a port: disagree |
| `ca54c2ab` | Forced Kompress routing falls back to structural compressors while the model is cold | crates/headroom-core/src/transforms/content_router.rs:690 force_kompress_all is set by profiles (:127) but no routing code reads it, so a forced no-op passthrough cannot happen | Old doc says is_ready is never consulted: moot, since the force flag does nothing in Rust. Doctor part is Python CLI |
| `55c78dea` | Codex custom_tool_call names count for Responses exclude-tools, so an excluded `exec` output is not compressed | crates/headroom-core/src/transforms/live_zone/openai_responses.rs:166-171,187 (custom_tool_call_output is never a compression candidate on any layer) | Old doc calls it a port: disagree, nothing to exclude. Revisit if custom_tool_call_output ever becomes a candidate |
| `a102e05b` | Pin DNS-validated addresses to the socket so rebinding cannot redirect a caller-chosen upstream | crates/headroom-proxy/src/proxy/egress.rs:276-310 (resolve_to_addrs + no_proxy); upstream_guard.rs:47,61; used at proxy/forward/stages.rs:581 | Old doc calls it a port: disagree |
| `b0c19a25` | Public binds need auth: `main.rs:308`, `proxy_auth.rs:87,114` |  |  |
| `ab62b9e0` | Cache mode: do not batch-compress small earlier Responses outputs once new outputs push them over the batch floor | core transforms/compression_batches.rs:211 build_compression_batches has no caller outside its own module; core transforms/live_zone/openai_responses.rs:23 freezes every earlier *_output item | Disagrees with old doc (listed as port at live_zone_responses.rs:59) |
| `ca02f28c` | Error envelopes fall back to the exception type name when the transport error text is empty | reqwest::Error Display is never empty and every envelope adds context: error.rs:60 "upstream timeout: {e}", error.rs:65, error.rs:9 "upstream request failed: {0}"; bedrock/invoke.rs:435 error_response carries a code (e.g. bedrock_sigv4_failed) | Disagrees with old doc (listed as port in bedrock invoke*). Python-specific (str(e)=="") |
| `cc6a07cd` | Perf: reuse Google/Cohere token counters across requests instead of rebuilding per call | core tokenizer/registry.rs:122-163 gives Gemini/Cohere a stateless EstimatingCounter; there is no per-request counter state or cache to lose | Python SDK providers; nothing to port |
| `70edfb46` | Perf: BM25 batch scoring counts the shared query once, not per item | core relevance/bm25.rs:193-196 builds query_freq once in score_batch and passes it to bm25_score per item | Old doc listed as port; disagree |
| `2fed685e` | Perf: CCR store() no longer runs a full-backend TTL sweep on every new key | core ccr/backends/in_memory.rs:89 evicts by popping the order queue; core ccr/backends/sqlite.rs:233 put is one upsert, purge is an indexed DELETE on get (sqlite.rs:155) | Agrees with old doc (already-there) |

## MERGED

| sha | behavior |
|---|---|
| `545f5441` | Rust. The fork already had this change; the merge dropped a duplicated helper |
| `72217242` | Rust. Removes the dead `crush_object` pass; the fork's version always passed through |
| `62d1cc08` | Rust. CCR log marker names dropped error labels and files; the fork's put-failure warning is kept |
| `6880984b` | Rust. BM25 query terms ordered once |
| `7ad9f809` | Rust. CSV compaction keeps null, missing, `""` and `"null"` apart |

## N/A

| sha | what | why |
|---|---|---|
| `be00a798` | Gateway-turn contract (external gateway calls Headroom to compress a turn) accepts Codex/Responses bodies by building a chat view of `input` and writing it back | Agree with old doc. Only applies to the upstream gateway-extension API the fork does not ship. |
| `bc21c937` | Python dependency bump |  |
| `67eb910e` | Telemetry beacon schema v2: records content-type/strategy/yield shapes and gzips the beacon upload | Agree. The content_router.py hunk only feeds the beacon; compression output is unchanged. |
| `a7858fd2` | Fix float overflow in the Kompress model-download retry backoff (2**n before the cap) | Agree. The fix is to a mechanism the Rust side does not have; nothing to overflow. |
| `e326dd7b` | Cargo dependency bump, arrives through Cargo.lock |  |
| `13536dbc` | CI |  |
| `2d5909c3` | npm dependency bump |  |
| `abde3e6e` | `headroom learn` writers normalise CRLF so Windows context files stop accumulating \r | Agree on verdict; reason is that the subsystem is absent, not fs::write byte-exactness. |
| `3c1a9015` | Empty duplicate of abde3e6e (same tree) |  |
| `261796f9` | .xls loader renders dates/bools/ints/errors the way the .xlsx loader does | Agree. |
| `ebc0b363` | TypeScript SDK |  |
| `94206e26` | Release 0.38.0 |  |
| `d05bacb4` | Comment-only fix to MODEL_ALIASES in litellm_model_resolution.py | Agree. Docs only. |
| `a4cb2bc0` | `headroom install` re-applies managed env vars added after a deployment was installed | Agree. |
| `d90dadf2` | Docker wrapper |  |
| `b2d92753` | Python packaging |  |
| `b9585094` | Windows scheduled-task install falls back for non-admin users | Python installer/supervisor; the fork installs via install.sh |
| `9a111099` | Raise `headroom init hook ensure` hook timeout from 15s to 60s | The fork doesn't ship Python `headroom init` or its plugin hooks; cclaude starts the proxy itself |
| `6c0e817b` | At stream end, drop an unterminated SSE tail instead of appending \n\n and parsing it (Python's strict decoder could raise on partial UTF-8) | Disagree with old doc (PORT partial). Rust's flush is deliberate: it catches a message_stop that straddles the last chunk. Framer/JSON errors come back as Result and get logged (sse/framing.rs:68,289), so the crash can't happen. Porting would lose that recovery |
| `7f44a644` | CI (release Slack notice) |  |
| `46ac52d3` | GET /transformations/feed?include_messages=0 omits message bodies and adds per-request cache-split fields | Disagree with old doc (PORT). The endpoint only serves the Python dashboard, which the fork doesn't ship |
| `1a7c5bbf` | .xls integer cells above 2^53 render as float, not fabricated int digits; bound inclusive | Disagree with old doc (PORT `<` vs `<=`). The bound is .xls-only and Rust has no .xls. Also, `<` vs `<=` makes no difference to output in Rust, since Display prints 9007199254740992 either way. Related, not this commit: Rust .xlsx renders floats >=1e16 as '123456789012345680' where Python gives '1.2345678901234568e+17' (documented divergence at :50-53, dead code for the proxy) |
| `e8a51098` | .xls docstring note on sub-second datetimes; drop pragma no-cover on the .xls load path | Agree with old doc (N/A): docs/coverage only, and .xls is not ported (spreadsheet_ingest.rs:142) |
| `b36e59a8` | When a stream has no usage and stopped on max_tokens/length, book output_tokens = the request ceiling instead of estimating from text | Disagree with old doc (PORT at sse_anthropic.rs:322). This refines a text estimator the fork deliberately doesn't have. Anthropic ceiling-stopped streams still carry exact usage in message_delta |
| `d17ac6a1` | New CompressionHooks.protect_messages hook: user code returns message indices the router must leave verbatim | Disagree with old doc (PORT in turn_hooks.rs). This is an in-process Python extension API for SDK/embedders; the Rust binary has no user compression-hook registration |
| `b7769831` | Windows graph installer downloads and extracts the zip codebase-memory-mcp asset | Python graph installer, not shipped |
| `558b066b` | MCP headroom_read prices file content with the token estimator instead of word count | Agree with old doc (N/A): Python MCP server not shipped |
| `a8ae0a6f` | Admit the Docker/Podman host gateway IP on the loopback-only /v1/compress and /v1/usage routes | Disagree with old doc (PORT at app.rs:222). That's the debug-route guard, which upstream didn't touch. Revisit only if /v1/compress is ever ported |
| `6e712672` | Python handler cleanup: fix the scoping of the system-compaction label/log that raised a spurious warning; remove dead _finalize_pre_upstream calls and the fallback_* config | Python-internal control-flow/dead-code fix with no Rust counterpart |
| `ff60d576` | `headroom learn` treats an unreadable project memory dir as absent | The Python learn CLI isn't shipped (Rust CLI modules: agent_savings, audit, copilot_auth, network_diff, tools) |
| `7c3cbc82` | Explicit memory tools (save/search/update/delete/list) refuse with an error when PROJECT scope fails to resolve | The fork chose the opposite on purpose: an unresolved project writes to the shared user partition, and --memory-project-root makes unresolved rare. Porting would reverse that choice. Old doc calls it a port: disagree. Ask the user if fail-closed is wanted |
| `f3d95ee2` | `headroom init` profile slug drops non-ASCII directory names | Python CLI init; the fork has no init command |
| `5baa439e` | Copilot device-code auth over urllib limits ALPN to http/1.1 | Python stdlib http.client quirk in copilot_auth; the fork has no device-code flow and reqwest negotiates ALPN correctly |
| `df8ebdf9` | `headroom wrap` no longer blocks launch on the Serena pre-index | Python wrap CLI; fork uses cclaude |
| `21d166b7` | Python test isolation |  |
| `62f775d5` | CI |  |
| `fa58d773` | CI |  |
| `56edfbc9` | npm dependency bump |  |
| `63113329` | npm dependency bump |  |
| `fbbe8b8e` | Cargo dependency bump, arrives through Cargo.lock |  |
| `c32b3155` | Python test |  |
| `3e36aefb` | `headroom memory reindex` rebuilds FTS5 a page per transaction | Python memory CLI; the Rust memory backend (memory/ctx_backend.rs) has no bulk reindex command |
| `fa9edb3a` | Banner, log and /stats report anonymous beacon status accurately | Telemetry beacon, which the fork does not ship (no beacon in crates/) |
| `ca4b47a3` | TypeScript SDK |  |
| `b2b58474` | TypeScript SDK |  |
| `95cbbb81` | TypeScript SDK |  |
| `63676e6c` | TypeScript SDK |  |
| `c3e8a1ee` | TypeScript SDK |  |
| `00896a7c` | Test-only. Its tests are the contract for the budget port (f734c573) |  |
| `a37d7343` | Tooling |  |
| `577336d2` | LiteLLM backend keeps image blocks on /v1/messages | No LiteLLM backend in the fork (bedrock/mod.rs:6 names the Python shim as replaced) |
| `a28dd4ea` | OpenClaw plugin |  |
| `2e2758de` | Python test |  |
| `54357ff6` | npm dependency bump |  |
| `b9ded1ae` | Python dependency bump |  |
| `0024b574` | OpenCode transport plugin can exclude hosts from routing | OpenCode JS/TS plugin; not shipped |
| `0c19244e` | Opt-in Bedrock prompt caching on the LiteLLM OpenAI-compatible path | No LiteLLM backend in the fork |
| `fc5a09e1` | `headroom wrap vscode-claude --1m` writes a [1m] model selector into VS Code settings | CLI install tooling the fork does not ship |
| `c4df2dde` | Trap SIGHUP in the Python wrap watcher/launcher so a closed terminal does not leak the proxy it spawned | Disagrees with old doc (it targeted claude-launcher/restart-headroom.sh) |
| `a29162ba` | Dashboard shows rolling cache economics split by owner | Dashboard HTML only |
| `871bbde3` | Return 501 for Anthropic batch routes when the upstream is Copilot | Disagrees with old doc (listed as port). Becomes PORT only if a Copilot upstream is ever added |
| `c81378c8` | Add context_window to xAI /v1/models entries passed through for the Grok CLI; sanitize etag/cache headers | Disagrees with old doc (pointed at local_model.rs:29 as a port target) |
| `3aa50128` | Perf: precompute word sets once in the memory-file budget manager's merge step | Python-only memory file maintenance |
| `23c20545` | CI |  |
| `e9114964` | SQLite memory store: numeric/bool metadata filters bind native values so they match | Disagrees with old doc (pointed at memory/models.rs:38). No Rust metadata-filter query exists |
| `c22cd1f1` | CI (the fork keeps its own `pr-health.yml`) |  |
| `76ec363a` | Docs: list C# and PHP as AST-compressed, drop Perl | Docs/comment only |
| `1a6f9419` | Graph store: parenthesize source/target OR so relation_type filter applies to both directions | If the graph-memory idea gets built, carry the parenthesized OR |
| `19ddfe1a` | MCP install ledger accepts an empty/absent agents section | Python `headroom mcp` CLI only |
| `5ccec67f` | Inline memory parser tolerates None content on tool-call turns | Old doc pointed at ctx/extract.rs:76, which is unrelated and already null-safe (extract.rs:74-80) |
| `7ce2580f` | Google explicit-cache manager tolerates tz-aware expiry timestamps | Python cache optimizer library only |
| `e64b9f58` | Python test |  |
| `66f42617` | Release 0.39.0 |  |
| `d13e1966` | Release 0.39.1 |  |
