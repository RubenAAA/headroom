# Upstream triage: `d5318ac2` → `1cf49661` (23 commits)

Range is `git log HEAD..upstream/main` at fetch 2026-10-04 (merge-base
`d5318ac2`, fork HEAD `d998c86b` == `origin/main`). Each commit touching
`headroom/` was checked against `crates/` with `git show` plus a file:line.
Classes as in [`upstream-triage-0.39.md`](upstream-triage-0.39.md):
PORT (Rust counterpart exists, lacks the behaviour), ALREADY-THERE,
OPTIONAL (verify then decide), N/A (no Rust counterpart, or docs/CI/deps).

Totals: **5 PORT, 4 ALREADY-THERE, 3 OPTIONAL, 11 N/A**.

Port order below is safety/security first, then correctness. Cache column
marks whether forwarded bytes change.

## Landed ports

(none yet — fill in as they land, with the local sha after the arrow.)

## PORT

| sha | cache | what | Rust evidence | size |
|---|---|---|---|---|
| `0147cf02` | no | Scrub the proxy token before forwarding: drop `Authorization: Bearer <proxy-token>` (only when it equals the configured token) and always drop `x-headroom-proxy-token` | `proxy_auth.rs:91-145` gates but never strips; `routed/auth.rs:73-79` forwards client `authorization` verbatim when no route credential is set — the same leak | S |
| `65e63c08` | no | Usage pollers must only adopt a loopback operator's bearer (`credential_policy.py`); network callers mark activity only | `proxy/subscription.rs` poll loop + `core subscription/tracker` have no `is_local_operator`/`from_local_operator` gate anywhere | S-M |
| `1cf49661` | yes | Run injected memory tools server-side on streaming turns instead of forwarding `tool_use` to a client that never declared them (new `memory_tool_stream.py`, 1500 lines incl. tests) | `proxy/memory_continuation.rs` handles the buffered path; verify the SSE/streaming path answers `memory_*` calls in-proxy and withholds them from the client stream | L |
| `dd84321d` | yes | Gemini paths join the prefix-freeze machinery + replay last turn's exact forwarded bytes over resent originals (all three apply sites + streaming finalizer) | `handlers/gemini.rs` has no tracker/freeze-floor/replay wiring (no matches for freeze/replay/tracker in that file); generic replay exists at `cache_stabilization/prefix_replay/` | M |
| `cfa479af` | no | Log `response_content_length` on `proxy_inbound_response` | Telemetry-only; find the `proxy_inbound_response` emission in `observability/` and add the field | S |

## OPTIONAL (verify, then port or close)

| sha | what | Rust evidence | why not now |
|---|---|---|---|
| `90a68e02` | LiteLLM streaming usage: exclude cache tokens from `input_tokens`, use real `completion_tokens` for output | `bedrock/mod.rs:6` names the Python shim as replaced — if Bedrock still streams through `headroom/backends/litellm.py`, the Python fix covers it and there is nothing to port; if native, mirror the two-bucket fix in the Rust Bedrock usage accounting | Routing question first |
| `2c4b20fc` | Router falls back to built-ins when an external compressor passes through | `core transforms/content_router.rs:1674-1687` already falls back on empty/expanded output — confirm the pure-passthrough (unchanged-output) case reaches the same fallback, else one branch | Probably ALREADY-THERE |
| `038c923b` | Preserve Gemini media parts in message conversion instead of dropping the turn | TS SDK commit, but check Rust `handlers/gemini.rs:68-96` (`has_non_text_parts`/`text_parts` + `gemini_contents_to_messages`) preserves rather than drops the same shapes | SDK commit; Rust check is a read-through |

## ALREADY-THERE

| sha | behavior | evidence |
|---|---|---|
| `57a57088` | Anthropic `tool_choice: {"type": "none"}` maps to OpenAI `"none"`, not `"auto"` | `openai/request.rs:286-300` already has the `Some("none") => json!("none")` arm; only the Python LiteLLM shim fell through |
| `0ef7b2a7` | Relevance segments bounded by `max_chars` as well as window | `core transforms/relevance_split.rs:50-80` bounds both axes in `segment()` |
| `3948dbe4` | Memory/CCR context injection skips trailing `system` messages to find the latest user turn | `memory_tail.rs:173-199` and `ccr_expansion.rs:109-114` both `rfind(role == "user")`, so a trailing system turn never blocks them the way Python's `messages[-1]` check did. Add the Claude Code 2.1.x shape as a regression test |
| `793bb856` | A timed-out call kills the image worker it abandons | No `image_isolation` module in `crates/` — the subsystem is Python-only, so there is no worker to leak. Close unless image handling is ever ported |

## N/A

- Python-only subsystems with no Rust counterpart: `1631cde6`, `2e74f06e`
  (learn/traffic-learner), `f977d525` (oauth2 plugin middleware),
  `31d03449` (install CLI), `9a72bc02` (perf CLI), `3a3a465b` (wrap CLI),
  `2bc94194` (dashboard HTML/CO₂ card; `core savings_tracker.rs` exists but
  the card is dashboard surface the fork does not ship).
- Docs, CI, deps: `95ae69a8` (config docs), `5eaf24ec` (Hermes docs page),
  `540f4da1` (docker workflow), `afe4f4ec` (npm bumps).

## Found while triaging

- `proxy_auth.rs` accepts the proxy token from two carriers but no forward
  path strips either — the `0147cf02` port should cover HTTP and the
  WebSocket upgrades (`/v1/responses`, `/v1/live`) the same way Python's
  `_security_gate` + `WebSocketAuthMiddleware` do.
- `90a68e02` needs the Bedrock-routing answer recorded here once checked,
  so the next triage does not re-ask it.
