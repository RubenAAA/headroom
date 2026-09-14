# Implemented: proxy-owned tools invariant + guard

- **Status:** done 2026-08-07 (`tests/integration_tool_invariant.rs`)
- **Source:** `docs/notes/rust-parity-gaps.md` §6
- **Summary:** audit of the §5 class found two more instances: routed models
  (streaming arm chains `ccr_stream` post-translator; buffered arm resolves
  pre-translation; `CcrShape` carries continuation shape) and memory tools
  (`handle_memory_response`, previously zero callers — unfinished wiring, not
  a streaming gap). OpenAI-streaming audit claim was wrong (injection gated
  on Anthropic endpoint — unreachable arm); a `can_resolve` gate pins it
  anyway. Invariant test asserts wire truth: Anthropic streams may advertise
  retrieve (they resolve it); OpenAI-shaped paths get none.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 6. Proxy-owned tools that nothing answered — DONE (2026-08-07)

Item 5 fixed one instance of a class. An audit for the rest found two more,
plus one claim that turned out to be wrong.

**Routed models (live).** `handlers/local_model.rs` injects `headroom_retrieve`
*and* hands compression a CCR store, so it emits retrieval markers too, and
resolved neither arm. Two comments in that file contradicted each other:
`:706` claimed "this path injects headroom_retrieve and resolves it", `:2154`
said the opposite and justified it on the grounds that the Claude path did not
resolve it either — true when written, false since item 5. Fixed: the streaming
arm chains `sse::ccr_stream` after the OpenAI→Anthropic translator (the turn is
already in the Anthropic vocabulary by then, so the same rewriter serves it),
and the buffered arm calls `handle_ccr_response` on the OpenAI shape before
translating. `CcrShape` carries the continuation shape — Anthropic,
chat-completions, or Responses `output[]` — with converters both ways.

**Memory tools (latent, worse in kind).** `proxy.rs` injects `memory_save`,
`memory_search`, `memory_update`. `MemoryHandler::handle_memory_tool_calls`
executes them correctly and had **no callers in the workspace** — so these
failed on every path, buffered included; this was never a streaming gap but an
unfinished wiring job. Dormant only because `memory_enabled` requires
`HEADROOM_MEMORY_ENABLED=1` and is not even a CLI flag. Fixed by
`handle_memory_response` (shaped after `handle_ccr_response`, same round cap
and mixed-tool rule) wired into the buffered branch, and by the stream
rewriter treating memory tools as proxy-owned alongside CCR.

**OpenAI streaming — the audit was wrong.** The claim that streamed
chat-completions and Responses requests were offered an unanswerable tool does
not hold: the whole injection block is gated on
`endpoint == AnthropicMessages`, so its OpenAI arm is unreachable. Those
clients were never handed the tool. A `can_resolve` gate was added at the
injection site anyway — inert today, but it means lifting the Anthropic-only
restriction cannot silently reintroduce the bug.

**The invariant, guarded.** `tests/integration_tool_invariant.rs` asserts what
leaves the proxy on the wire: the Anthropic stream path may advertise
`headroom_retrieve` because it resolves it; the OpenAI-shaped paths are offered
none of the proxy's own tools. Nothing tied injection to resolution before, and
they drifted apart three separate times.
