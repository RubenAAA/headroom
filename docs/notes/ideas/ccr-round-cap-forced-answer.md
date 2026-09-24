# Idea: answer instead of splicing when the CCR round cap is hit

- **Status:** open, queued 2026-09-24. Waiting on the peer session's uncommitted
  CCR work in `proxy/ccr_response.rs`, `routed/ccr.rs` and
  `core/ccr/response_handler.rs`. Start after that lands, not beside it.
- **Source:** proxy logs 2026-09-17..24; transcripts of the 7 sessions in which
  `retry-dropped-turn.sh` fired over 2026-09-21..24.
- **Value:** stops a loop. When the model keeps calling `headroom_retrieve`,
  `check_ccr_round_budget` (`proxy/ccr_response.rs`) ends the hidden rounds
  (`ccr_max_rounds_partial`: 194 on Spark in the window). The turn goes back
  with fetched content spliced in and no answer. The Stop hook then pushes the
  model on, up to three times, and it often retrieves again. The sessions it
  hit ran ~220 tool calls against 30–70 for the rest.
- **Next step:** when the budget is spent, send one last continuation with the
  retrieve tool withheld (`tool_choice: none`, or the tool dropped from
  `tools`), so the model must answer from what it already holds. Test: a
  wiremock upstream that answers every round with another retrieve call must
  end in model prose, not a splice, and `retry-dropped-turn.sh` must stay
  quiet on the resulting transcript. Measure with `ccr_max_rounds_partial`
  and the hook's fire count before and after.
- **Related:** `docs/notes/learnings/offload-loses-at-2000-bytes.md` (most of
  the trigger volume goes away with `--ctx-offload-zen` and a 20,000-byte
  floor, so re-measure the count once those are live).
