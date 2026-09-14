# Implemented: OpenAI-native streaming CCR via buffering (this session)

- **Status:** implemented 2026-09-11 (`openai_buffered_ccr.rs` + `forward_http`
  seams + `tests/integration_responses_buffered_ccr.rs`)
- **Source:** `docs/notes/rust-parity-gaps.md` §8; Python
  `_should_buffer_openai_responses_stream_ccr` + buffered branch
- **Summary:** streaming `/v1/responses` carrying `headroom_retrieve` is called
  upstream `stream:false`, resolved by the buffered arm, resynthesized as SSE;
  residual retrieve fails closed (502), malformed 200 fails closed (502 JSON);
  ChatGPT-OAuth stays streaming; #2613 SSE-answers covered. Adaptations: no
  ASGI grace/heartbeat (status known pre-reply), no chat-streaming change,
  no live OpenAI rewriter, no new injection (invariant holds).
- **Scope:** direct Responses-API clients only; the Claude-Code→Codex
  translate flow was already covered by the Anthropic rewriter; Codex WS
  remains uncovered.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

- **OpenAI-native streaming CCR.** DONE (2026-09-11) via buffering, not a
  live rewriter — the Python answer, adapted. A streaming `/v1/responses`
  request carrying `headroom_retrieve` (and CCR handling on, non-ChatGPT
  auth) is called upstream with `stream: false`, resolved by the existing
  buffered `openai_responses` arm, and resynthesized as SSE
  (`response.created` → incremental item/text events → `response.completed`
  → `[DONE]`); residual retrieve after handling fails closed (502), as does
  a malformed 200. Port: `crates/headroom-proxy/src/openai_buffered_ccr.rs`
  (predicate + ChatGPT sniff + JSON↔SSE, mirroring
  `_should_buffer_openai_responses_stream_ccr`,
  `_openai_responses_to_sse`, `_openai_responses_from_sse`), wired in
  `forward_http` before the send and at the buffered return site; coverage
  in `tests/integration_responses_buffered_ccr.rs` (buffer+resynthesize,
  ChatGPT exclusion, no-tool passthrough). Deliberately not ported: the
  ASGI grace/heartbeat wrapper (status is known before anything is sent, so
  fidelity is free and there is no idle window), chat-completions streaming
  (both sides agree: never injected, nothing to resolve), and a live
  OpenAI-vocabulary stream rewriter (Python deferred it as #1877 B/C too).
  The `integration_tool_invariant` premise (OpenAI clients are offered no
  proxy tools) still holds — nothing new is injected.


## Scope note

*moved from `docs/notes/rust-parity-gaps.md`*

- **Scope note (2026-09-11):** this changes nothing for the local flow —
  a Codex model driven from Claude Code through the translate route speaks
  Anthropic to the proxy, and streaming retrieval there was already covered
  by `rewrite_anthropic_stream` (`RoutedChat`/`RoutedResponses`). The port
  only closes the leak for direct Responses-API clients (Codex CLI over
  HTTP, Copilot, agents); Codex over WebSocket remains uncovered.
