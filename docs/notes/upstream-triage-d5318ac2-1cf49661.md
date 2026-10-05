# Upstream triage: `d5318ac2` → `1cf49661` (23 commits)

Range is `git log HEAD..upstream/main` at fetch 2026-10-04 (merge-base
`d5318ac2`, fork HEAD `d998c86b` == `origin/main`). Each commit touching
`headroom/` was checked against `crates/` with `git show` plus a file:line.
Classes as in [`upstream-triage-0.39.md`](upstream-triage-0.39.md):
PORT (Rust counterpart exists, lacks the behaviour), ALREADY-THERE,
OPTIONAL (verify then decide), N/A (no Rust counterpart, or docs/CI/deps).

Totals: **7 PORT (6 landed, 1 open), 5 ALREADY-THERE, 0 OPTIONAL, 11 N/A**.

Port order below is safety/security first, then correctness. Cache column
marks whether forwarded bytes change.

## Landed ports

- `0147cf02` → proxy-token scrub in `proxy_auth.rs`: `scrub_proxy_token_headers`
  drops `x-headroom-proxy-token` always and `Authorization: Bearer` only when it
  equals the configured token, on every gate path that reaches the handler
  (authenticated, loopback, exempt probes) — so no forward path
  (`routed/auth.rs:73-79` forwards client `authorization` verbatim) can leak the
  secret upstream. 7 unit tests.
- `cfa479af` → `proxy_inbound_response` debug event in `proxy/app.rs`
  (`track_inbound_request`): method/path/status/duration plus
  `response_content_length` from the `Content-Length` header (`None` when
  absent/non-numeric). Debug-level on purpose — counters stay the always-on
  signal. Unit test for the header read.
- `1cf49661` → streamed memory-only turns run the SSE rewrite without CCR:
  `assemble_response_stream` (`proxy/forward/response.rs`) builds the memory
  context whenever the forwarded `/v1/messages` body carries memory tool
  definitions (`forward::request_carries_memory_tools`), and
  `maybe_rewrite_anthropic_stream` (`proxy/sse.rs`) runs the fold when CCR
  *or* memory is present, with an empty `InMemoryCcrStore` standing in for
  retrieval on memory-only turns. Before, the rewrite was CCR-gated, so a
  streamed `memory_search` with CCR off reached the client untouched (`No
  such tool available`). E2E `a_memory_only_streamed_call_is_answered_with_ccr_off`
  plus unit tests for the tools scan. Audit follow-up, same change: the
  rewrite initially ran for any `/v1/messages` body carrying the tools,
  including buffered/error ones — but the fold only understands Anthropic
  SSE and would have swallowed a JSON body whole (empty 200). Gated on
  `is_sse && success && Anthropic kind`, with E2E
  `a_buffered_memory_turn_comes_back_whole` (fail-before verified), plus a
  handler-initialized check so non-memory deployments skip the body scan.
- `2c4b20fc` → `external_output_rejected` (`core
  transforms/content_router.rs`) now also rejects `compressed: false`
  (passthrough = the compressor declining the block), falling back to
  built-ins. The contract existed on `CompressOutput` but no caller read it.
  Test `a_passthrough_compressor_falls_back_to_the_builtin`.
- `0ef7b2a7` → Pass-2 `segment()` loop rewritten under one char budget in
  both copies: `core transforms/relevance_split.rs` (was chunking by line
  window only) and `core transforms/content_router.rs` (whose window loop
  was budgeted but whose indented-continuation extension was not). Mirrors
  Python's fixed loop verbatim, including the atomic over-long line.
  Fail-before verified on both; tests
  `segment_respects_max_chars_upstream_0ef7b2a7` and
  `segment_bounds_a_long_indented_run_by_max_chars`.
- `9a72bc02` → Savings Audit section in `headroom perf` (`core
  perf_analyzer.rs`: `build_savings_audit` + `push_savings_audit_lines`,
  between headline and per-model as in Python). First triaged as N/A
  (misread as Python-CLI-only); Rust ships the same `format_report`. The
  per-record lists behind the Python dict have no text consumer and are left
  out. Test `format_report_savings_audit_section`.

## Open

| sha | class | cache | what | Rust evidence | size |
|---|---|---|---|---|---|
| `dd84321d` | PORT | yes | Gemini paths join the prefix-freeze machinery + replay last turn's exact forwarded bytes over resent originals (all three apply sites + streaming finalizer) | `handlers/gemini.rs` compresses statelessly per request with no tracker/freeze-floor/replay wiring; generic replay at `cache_stabilization/prefix_replay/` is Anthropic-messages-shaped | M |
| (none) | — | — | `90a68e02`/`038c923b`/`2c4b20fc` verify items below | see Optional | S |

## PORT

| sha | cache | what | Rust evidence | size |
|---|---|---|---|---|
| `0147cf02` | no | Scrub the proxy token before forwarding: drop `Authorization: Bearer <proxy-token>` (only when it equals the configured token) and always drop `x-headroom-proxy-token` | `proxy_auth.rs:91-145` gates but never strips; `routed/auth.rs:73-79` forwards client `authorization` verbatim when no route credential is set — the same leak | S |
| `1cf49661` | — | landed, see above | — | L |
| `dd84321d` | — | open, see above | — | M |
| `cfa479af` | no | Log `response_content_length` on `proxy_inbound_response` | Telemetry-only; find the `proxy_inbound_response` emission in `observability/` and add the field | S |

## OPTIONAL — all rows closed during verification

- `2c4b20fc` → landed (was a real gap, not already-there).
- `90a68e02` → N/A (Bedrock is native; see below).
- `038c923b` → ALREADY-THERE (see above).

## ALREADY-THERE

| sha | behavior | evidence |
|---|---|---|
| `57a57088` | Anthropic `tool_choice: {"type": "none"}` maps to OpenAI `"none"`, not `"auto"` | `openai/request.rs:286-300` already has the `Some("none") => json!("none")` arm; only the Python LiteLLM shim fell through. Locked by `anthropic_none_dict_tool_choice_maps_to_none_not_auto` |
| `3948dbe4` | Memory/CCR context injection skips trailing `system` messages to find the latest user turn | `memory_tail.rs:173-199` and `ccr_expansion.rs:109-114` both `rfind(role == "user")`, so a trailing system turn never blocks them the way Python's `messages[-1]` check did. Locked by `tail_anthropic_skips_a_trailing_system_message` (Claude Code 2.1.x shape) |
| `793bb856` | A timed-out call kills the image worker it abandons | No `image_isolation` module in `crates/` — the subsystem is Python-only, so there is no worker to leak. Close unless image handling is ever ported |
| `65e63c08` | Usage pollers only poll with the operator's own credential, never a network caller's bearer | Structurally safe: `core subscription/tracker.rs:202-204` reads the poll token only from env/file (PR-F3 keeps no raw bearer at all), `notify_active` has no proxy callers, and the Codex usage poll has no Rust counterpart (`websocket_codex.rs:15`). Locked by `traffic_bearer_never_becomes_poll_credential` |
| `038c923b` | Preserve Gemini media parts in message conversion instead of dropping the turn | TS SDK commit, but the Rust conversion holds back all four non-text part kinds (`handlers/gemini.rs:68-82`: `inlineData`/`fileData`/`functionCall`/`functionResponse`) into `preserved_indices` and `rebuild_gemini_contents` restores them at their original indices — the turn is never dropped. Pinned by `gemini_to_messages_preserves_non_text`, `rebuild_gemini_contents_interleaves`, `messages_to_gemini_roundtrip` |

## N/A

- Python-only subsystems with no Rust counterpart: `1631cde6`, `2e74f06e`
  (learn/traffic-learner — no such module in `crates/`), `f977d525` (oauth2
  plugin middleware — Rust has no plugin/middleware surface; only Vertex ADC
  scope and redaction placeholder minting, both unrelated), `31d03449`
  (install CLI — Rust `headroom_cli` has no install command), `3a3a465b`
  (wrap CLI — no Rust wrapper),
  `2bc94194` (dashboard HTML/CO₂ card; `core savings_tracker.rs` exists but
  the card is dashboard surface the fork does not ship).
- Docs, CI, deps: `95ae69a8` (config docs), `5eaf24ec` (Hermes docs page),
  `540f4da1` (docker workflow), `afe4f4ec` (npm bumps).
- `90a68e02` (LiteLLM streaming usage buckets): nothing to port. Bedrock is
  native in Rust (Phase D, `bedrock/`), speaks Anthropic shape end-to-end,
  and usage flows through the shared disjoint-bucket observer — the LiteLLM
  OpenAI-shape translation the bug lived in never runs. No live Bedrock
  traffic here to measure against.

## Found while triaging

- `proxy_auth.rs` accepts the proxy token from two carriers but no forward
  path strips either — the `0147cf02` port should cover HTTP and the
  WebSocket upgrades (`/v1/responses`, `/v1/live`) the same way Python's
  `_security_gate` + `WebSocketAuthMiddleware` do.
- `90a68e02` answered while triaging (Bedrock is native; see N/A), so the
  next triage does not re-ask it.
- `dd84321d` stays open deliberately: the live log shows zero Gemini traffic
  (83,760 `/v1/messages`, 10,617 `/v1/responses`, no `generateContent`), so a
  cache-sensitive Gemini replay overlay cannot be validated here. Same shape
  as the `deca575c` call in `upstream-triage-0.39.md` (ported although this
  machine serves no chat-completions traffic) — take it when a Gemini client
  appears or as a standalone task with mock-upstream tests.
