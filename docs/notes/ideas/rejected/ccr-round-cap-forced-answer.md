# Idea: answer instead of splicing when the CCR round cap is hit

- **Status:** rejected 2026-09-28 — the loop stopped on its own. Retrieves
  left unanswered (`ccr_retrieve_unresolved`): 140 on 09-23, 15 on 09-24, 6
  on 09-25, none since. Cap hits that left one standing: 80, 10, then 1, 0,
  0 (09-25..27). The offload floor and `--ctx-offload-zen` removed the
  trigger, as the Related line predicted. A forced-answer round would also
  cost cache: `tool_choice: none` breaks the message cache and dropping the
  tool breaks the whole prefix.
  Two corrections to the numbers below. `ccr_max_rounds_partial` overcounted:
  the budget check ran before the parse, so it fired on final rounds that had
  already answered (121 of 216 hits). Fixed the same day — it now fires only
  when the model asks for another retrieve past the cap. And the Spark cap is
  2, not 6: `ZEN_PROXY_TOOL_ROUND_CAP` (`routed/ccr.rs`) overrides
  `--ccr-max-retrieval-rounds` on the Zen route.
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
