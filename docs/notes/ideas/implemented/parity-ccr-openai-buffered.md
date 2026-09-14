# Implemented: CCR retrieve interception for OpenAI buffered paths

- **Status:** done 2026-07-10 (182 core CCR + 1248 proxy tests green)
- **Source:** `docs/notes/rust-parity-gaps.md` §2 (Python `62cd3072`)
- **Summary:** `handle_ccr_response` generalized to a `provider` param
  (`anthropic` / `openai` / `openai_responses`) with 3-way path dispatch;
  `response_handler.rs` + `tool_injection.rs` gained the `openai_responses`
  branch; wiremock test proves Responses interception + continuation.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 2. CCR retrieve-tool interception for OpenAI (chat + Responses) — DONE (2026-07-10)

Turned out bigger than the audit item: Rust's `handle_ccr_response` in
`proxy.rs` was hardcoded to `"anthropic"` only — OpenAI chat-completions AND
Responses both got zero CCR interception at the HTTP layer, despite
`response_handler.rs` already having a working (but never-invoked)
`"openai"` provider branch. Ported: added `"openai_responses"` provider
branch to `headroom-core/src/ccr/response_handler.rs` +
`tool_injection.rs` (mirroring Python `62cd3072` exactly), generalized
`handle_ccr_response` to take a `provider: &str` param with an
`extend_or_push` helper covering all 3 providers' sentinel conventions, and
replaced the Anthropic-only gate with a 3-way path-based dispatch covering
`/v1/messages`, `/v1/chat/completions`, and `/v1/responses`. New wiremock
integration test proves OpenAI Responses `headroom_retrieve` interception +
continuation. Full suite green (core ccr: 182 tests, proxy: 1248 tests).

<details><summary>Original scoping notes</summary>

- Python: `62cd3072` (#1898), ~243 lines in `headroom/proxy/handlers/openai.py`.
  Wires CCR (context-compression-retrieval) `headroom_retrieve` tool-call
  interception into the `/v1/responses` handler, so a retrieval marker the
  model emits gets resolved server-side instead of leaking to the client.
- Rust: `crates/headroom-proxy/src/handlers/responses.rs` has zero CCR/
  retrieve-tool wiring today (confirmed via repo-wide grep, including all
  unmerged branches — `fix/ccr-retrieve-full-only` looked promising by name
  but doesn't touch `responses.rs` at all).
- Scope: needs the same interception pattern the Anthropic buffered path
  already has (`crate::compression::ctx_offload`, CCR store lookups) ported
  into the Responses handler's request/response cycle.

</details>
