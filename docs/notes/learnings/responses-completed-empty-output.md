# Learning: an empty `output: []` on `response.completed` erased the turn

- **Source:** `~/headroom-proxy.log` 2026-09-22, request
  `6ffbe940-22a6-4637-9c39-068a1f73539f`, provider `openai_responses`
  (GPT-5.6 sol over the Codex route). Found while auditing a subagent run
  that came back having done no work.
- **Claim:** a complete 186,544-byte Responses stream folded to zero output
  blocks. `ResponseState` took the `output` array on the
  `response.completed` envelope as authoritative, and that array was empty,
  so it discarded the message it had already gathered from
  `output_item.done` and the text deltas. `continuation_turn_from_body`
  saw an empty `output`, returned `None`, and the caller logged
  `ccr: failed to parse continuation response` (`content_type: ''`,
  `terminal: 'completed'`) then served the store-fetched fallback note in
  place of the whole model turn.
- **What that looks like from outside:** the agent answered with the raw
  text of a document it had retrieved and stopped. No error, no empty
  response — a plausible-looking turn with the model's actual work missing.
  This is why it went unnoticed.
- **Rate, 2026-09-22 window only:** 1 occurrence in 294 CCR continuations.
  Rare, and it destroys the entire turn when it fires.
- **Not the cause, though it looks like one:** the missing `Content-Type`.
  `continuation_turn_from_body` already sniffs the body when the header is
  absent (added 2026-09-14 for exactly this route), and the sniff passed —
  the body is a well-formed SSE stream beginning `event: response.created`.
  The fold ran and returned nothing.
- **Second, smaller finding:** the same stream logged `sse_unknown_event`
  for `response.reasoning_summary_part.added` / `.done`. GPT-5.6 emits
  those part boundaries around the summary deltas the existing arms already
  accumulate. They were harmless — no text is carried on them — but they
  buried the real signal in warn noise. Now recognised and ignored, which
  matches what the live translator does with them.
- **Changed:** `sse/openai_responses.rs` treats an empty `output` array as
  omitted rather than authoritative, so items gathered incrementally
  survive; the part-boundary events are recognised. Regression test in
  `openai/response.rs` replays the recorded shape.
