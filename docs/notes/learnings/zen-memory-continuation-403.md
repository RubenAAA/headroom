# Learning: Zen 403s memory continuations, and the nonce fix did not stop it

- **Source:** `~/headroom-proxy.log` 2026-09-22, a Claude Code session on
  `/model claude-muse-spark-1.3` that answered every turn with a thinking
  block and nothing else. Follows
  `docs/notes/ideas/implemented/zen-continuation-request-nonce.md`.
- **Claim:** on the Zen route the proxy-side memory tool takes the turn down
  with it. The model asks for memory, the proxy suppresses the block and
  POSTs a continuation to finish the turn, and Zen answers `403
  FreeTierError: "OpenCode's free tier can only be used from within
  OpenCode"`. 403 was not in the retry set, so the continuation returned
  `Done(None)`, the promised tool call was dropped, and `stop_reason` was
  downgraded `tool_use` → `end_turn`. Claude Code renders that as
  "Thought for 27s" and an empty turn.
- **Numbers, 2026-09-22 window only:** 8 memory continuations on
  `muse-spark-1.3-contributor-free`, of which 4 returned 403 and 1 returned
  400. On Anthropic-route models, 15 memory continuations and 0 failures.
  294 CCR continuations, all 200, all sonnet/opus — CCR continuations never
  run on the Zen route, which is why only the memory path shows this.
  All four 403s logged `attempt: 0, round: 1`: no retry was ever tried.
- **The nonce fix did not fix it.** `refresh_zen_request_id` (fresh
  `x-opencode-request` per continuation POST) was live — binary built
  2026-09-20, proxy up since 02:43Z — and all four 403s happened under it.
  The idea note already flagged message-id replay as unproven; this is the
  measurement that refutes it as the sole cause. Keep the nonce refresh: it
  is client-faithful and costs nothing.
- **Ruled out this window:**
  - *Fallback session id.* No `zen_session_mint_started` events all day, so
    `resolve_zen_session` found a real cloud-synced OpenCode session every
    time and never served the legacy minted id.
  - *Concurrency on the session.* Zero other Spark requests were in flight
    at any of the four 403s, measured by folding each PERF line's
    `total_ms` back to a start time and intersecting.
- **Open, and the best hypothesis:** body shape. All four failures had
  `msgs=2` and `tok_before ≈ 24.4k`; all four passes had `msgs=1` and
  `tok_before` 12.4–15.8k. Perfect separation, but n=8 — do not treat it as
  settled. The next probe is replaying a 403'd continuation body against
  Zen unchanged; if it 403s again the trigger is the body, and retrying is
  pointless.
- **Changed:** `memory_status_is_retryable` (`proxy/forward.rs`) adds 403 to
  the retry set on the Zen route only, gated on the `x-opencode-request`
  presence check `refresh_zen_request_id` uses. This is a hedge against
  transience, not a fix for the split above. The fix that holds either way
  is the empty-turn guard below.
- **Also changed, and this is the durable part:** the empty-turn notice in
  `sse/ccr_stream.rs` used to fire only when `next_client_index == 0`. A
  thinking block occupies an index, so a thinking-only turn looked like a
  turn that had already delivered content and the notice was skipped. The
  guard now asks whether the client saw *visible text* — thinking and
  whitespace-only text do not count. Any continuation failure that empties a
  turn now says so, on every route, whatever Zen decides to do next.

## Resolved 2026-09-22: Zen mandates streaming, and the 403 says so badly

The body-shape hypothesis above is refuted. The discriminator is `stream`.

- **The measurement.** Same prompt, same Zen session, seconds apart, through
  the same proxy process: a client that asks for a streaming turn 403s on
  every memory continuation, and a client that asks for a non-streaming turn
  gets 200 and a real memory answer. The split is not the model, the session,
  the token count or the message count — it is which of the two code paths
  builds the continuation.
- **Why the paths differ.** The streaming path
  (`sse/ccr_stream.rs`) de-streams the continuation through
  `non_streaming_continuation_request`, because routed SSE was not foldable
  when that was written. The non-streaming path (`routed/ccr.rs`) forwards the
  request body untouched, and a Responses-shaped body is already
  `stream: true`. So only streaming clients — which is every Claude Code
  session — ever sent Zen a `stream: false` continuation.
- **What Zen does with it.** `403 FreeTierError: "OpenCode's free tier can only
  be used from within OpenCode"`. The real OpenCode client always streams, so a
  de-streamed request fails the gate no matter how correct its
  `x-opencode-*` headers are. The message names auth; the cause is the body.
  This is why the nonce refresh could not have helped, and why replaying a
  403'd body would have reproduced it forever.
- **The fix.** `restore_stream_when_mandated` now covers `opencode.ai`
  alongside `chatgpt.com`. The codex gateway answers a de-streamed
  continuation `400 Stream must be set to true`; Zen answers 403. Same
  requirement, different manners. Folding the SSE back is already supported
  for `openai_responses` (`continuation_turn_from_body`), so nothing else had
  to change.
- **Before and after, same machine, minutes apart:** old binary, 4 of 4
  continuations 403 and 0 of 4 turns carrying a memory answer. Fixed binary,
  12 continuations, 0 × 403, 1 × 400, and memory answers arriving.
- **Still open, and separate:** Spark sometimes ends a turn right after
  announcing the search, with the continuation sent and no error logged. That
  is the model, not the transport, and it is what the empty-turn notice exists
  to make visible.
- **A trap worth keeping.** Reproducing this needs `--ctx-capture --ctx-offload
  --ctx-inject`, which `claude-launcher` passes and `~/.headroom-flags.sh` does
  not. Memory-tool injection lives inside that transform block, so a test proxy
  started from the flags file alone never owns a `memory_search` call and the
  bug cannot appear. A toolless request misses it too: the `no-tools->spark`
  router rule takes a different path.

## The 403 was the first of three, and the other two were silent

Fixing the 403 uncovered what it had been hiding. A Spark turn that asks for
memory passes through three places where its answer can vanish, and only the
first said anything.

1. **The 403 itself.** De-streamed continuation; fixed above.
2. **A continuation that returns nothing.** `fold_or_retry_round_body`
   (`proxy.rs`) returned `MemoryRoundRead::Done` with no log when the body
   folded to no usable turn. Captured 2026-09-22: Zen answered a continuation
   with `response.incomplete`, `incomplete_details.reason:
   max_output_tokens`, `usage.output_tokens_details.reasoning_tokens: 597` of
   a 600 budget, and `output: []`. The tool had already run; its answer was
   thrown away without a word. There is now a `memory_continuation_folded_empty`
   warning carrying the terminal status and, when there is one, the incomplete
   reason.
3. **A turn that loses its answer but had already spoken.** Two paths, both
   in `sse/ccr_stream.rs`:
   - The continuation came back with zero blocks, so nothing was dropped and
     nothing was emitted. The client got `end_turn` straight after the model's
     "I'll search memory for that" — indistinguishable from the model deciding
     to stop. Now `ccr_continuation_answer_lost` plus a client-visible note.
   - A promised tool call was dropped and `stop_reason` downgraded, but the
     notice was gated on `turn_lacks_visible_text`. Any turn that had said
     anything at all — which is most of them — was told nothing. The notice now
     always goes out; only its wording depends on whether the turn had spoken.

**On the budget.** A reasoning model gets the client's full `max_output_tokens`
for the continuation and may spend all of it thinking. At Claude Code's
defaults there is room to spare; at 600 there is not, and the whole answer is
lost. The proxy does not raise the client's ceiling — that is the client's
number to set — but it no longer loses the turn quietly when the ceiling bites.

**Measured, same prompt, same session, 600-token ceiling:** before, 6 of 6
turns ended silently with no answer and nothing in the log. After, 8 of 8
turns carried a second block — six real memory answers, two explicit notices.
At an 8000-token ceiling, 5 of 5 answered and no notice fired.
