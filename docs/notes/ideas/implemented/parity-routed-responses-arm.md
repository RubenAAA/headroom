# Implemented: routed buffered Responses arm

- **Status:** done 2026-08-07 (3 new tests)
- **Source:** `docs/notes/rust-parity-gaps.md` §8
- **Summary:** SSE fold moved to `responses_stream_to_turn`
  (`output[]` on `response.completed` wins); arm takes `RoutedCcr` with
  resolve-then-book order via `responses_output_as_anthropic_turn`. Two test
  found bugs fixed en route: text fallback dropping tool-call turns' text,
  hardcoded `stop_reason: end_turn` on tool turns.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

- **Routed buffered Responses arm** — DONE (2026-08-07). The SSE fold moved out
  of `handle_buffered_responses_response` into `responses_stream_to_turn`, which
  collects `output[]` items from `response.output_item.done` and lets the
  `output[]` on `response.completed` win over them (a call whose item event
  never arrived is still in there). The arm now takes a `RoutedCcr` and runs the
  same resolve-then-book order as the chat arm, converting through
  `responses_output_as_anthropic_turn`. Two things fell out of writing the
  tests: keying the text fallback off "no items at all" dropped the text of any
  turn that also made a tool call, and `responses_output_as_anthropic_turn`
  hardcoded `stop_reason: "end_turn"`, which would have handed a client a tool
  to run inside a turn marked finished. Both fixed; 3 new tests.
