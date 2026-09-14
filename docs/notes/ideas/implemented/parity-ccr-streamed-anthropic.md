# Implemented: CCR interception on streamed Anthropic turns

- **Status:** done 2026-08-07 (`sse/ccr_stream.rs`)
- **Source:** `docs/notes/rust-parity-gaps.md` §5
- **Summary:** streamed turns offered `headroom_retrieve` then passed the call
  to clients that answer `No such tool available` (injection unconditional,
  resolution buffered-only). Stream adapter suppresses the block (name arrives
  on `content_block_start` before bytes flow), withholds
  `message_delta`/`message_stop`, rebuilds into non-streaming shape for the
  same `handle_ccr_response`, synthesizes continuation SSE after already-sent
  blocks. Continuation usage folds via `CcrRoundUsage`; unresolvable retrieves
  dropped, never forwarded. Anthropic `/v1/messages` only.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 5. CCR retrieve-tool interception on streamed turns — DONE (2026-08-07)

Item 2 above wired `handle_ccr_response` for all three provider shapes, but
every one of those call sites sits inside `if should_buffer_for_cache`, which
is `!is_sse && status.is_success()`. Tool *injection* has no such condition, so
on a streamed turn the proxy offered `headroom_retrieve` and then passed the
model's call straight to a client that had never heard of it — Claude Code
reports `No such tool available: headroom_retrieve` and the turn dies. Since
every interactive client streams, the feature only ever worked on the batch
path and in tests.

Fixed by `crates/headroom-proxy/src/sse/ccr_stream.rs`: a stream adapter ahead
of the telemetry tee that suppresses the `headroom_retrieve` block (its name
arrives on `content_block_start`, before any of its bytes would go out),
withholds `message_delta` / `message_stop`, rebuilds the turn into the
non-streaming response shape at end-of-stream, and hands it to the same
`handle_ccr_response`. The resolved turn is synthesised back into SSE events
numbered after the blocks the client already received, so the client sees one
message and no retrieval round trip. A turn that retrieves nothing gets its
withheld events released verbatim.

Continuation-round usage travels to the SSE outcome through an
`Arc<Mutex<CcrRoundUsage>>` and is folded in the way the buffered path does,
so a retrieval no longer books only the last round's tokens. An unresolvable
retrieve (mixed with a client tool call, out of rounds, upstream refused) is
dropped rather than forwarded — forwarding it is the bug.

Anthropic `/v1/messages` only. The OpenAI chat-completions and Responses
stream shapes have their own event vocabularies and remain buffered-only.
