# Refuted WIP-review flags: Zen ceiling, CCR notices, empty output (2026-09-23)

Source: uncommitted-WIP review 2026-09-22/23. An initial pass flagged ten
issues; re-verification against the code killed four (plus one
review-methodology claim). Kept so future reviews do not re-flag them.

## 1. `lift_zen_output_ceiling` overwrites `reasoning` — NOT a bug

Flag was "clobbers client summary/detail". Wrong: the client's effort is
preserved via `output_shaper::requested_effort(anthropic)` (`max` maps to
Zen's `xhigh`, anything else passes through). Only `summary: auto` and
`stream_options.sequential_cutoff` are forced, which is the documented Zen
requirement, not client data loss. See the function doc comment and
`zen_route_passes_the_clients_effort_through` test in
`crates/headroom-proxy/src/routed/translation.rs`.

## 2. Zen sends no `max_output_tokens`, defaults to `xhigh` — NOT a bug

Flag was "unbounded spend/latency". This is the deliberate fix for a
measured failure: the client's `max_tokens` became `max_output_tokens`,
which on the Responses API covers reasoning *plus* output, so a reasoning
model burned 597 of 600 tokens thinking and returned `output: []`
(`incomplete_details.reason: max_output_tokens`). Covered by
`zen_route_sends_no_output_ceiling_and_defaults_to_xhigh`. The spinner
sidecar still pins `minimal` + a small budget; only this route is uncapped.

## 3. `client_saw_visible_text` set on any `text` start — coarse by design

Flag was "true even on empty text, contradicts the trim check". A
content check would be worse: `content_block_start` for text is usually
empty and the prose arrives in later deltas, which never revisit the flag
— so checking content at start would miss real text while the current
heuristic (a text block opened means prose is coming) only over-fires on
whitespace-only turns, where `turn_lacks_visible_text` still consults the
spliced `emit` blocks via `is_visible_text_block`. Documented on the
field in `crates/headroom-proxy/src/sse/ccr_stream.rs`.

## 4. CCR notice wording/branches — already guarded, do not "fix"

Three sub-flags, all misreads of guarded code in `ccr_stream.rs`:

- The dropped-call notice is inside `stop_reason_overclaims_tool_call`
  *and* splits wording on `turn_lacks_visible_text` (empty turn vs turn
  with visible text). It does not fire on succeeded turns.
- The lost-answer branch requires `!suppressed.is_empty()`, but the
  `next_client_index == 0` (client saw nothing at all) case is handled by
  the branch directly below it — not a bare `end_turn`.
- `openai_responses.rs` treating empty `output: []` as omitted is the fix
  for request `6ffbe940-…` (186 KB `response.completed` folded to zero
  blocks, discarding gathered deltas). A genuine-empty turn resurrecting
  stale deltas is the accepted tradeoff, documented at the call site.
- `orphan_tool_result.rs` converting `tool_result` to plain `text` drops
  `is_error`/`cache_control` because the target type has no such fields;
  neutralization (admit the gap, keep the text) is the documented design,
  not a loss bug.

## Review-methodology footnote

`for v in $VARS` in `scripts/check-drift.sh` does **not** trip shellcheck
(SC2086) — verified with shellcheck 0.9.0, exit 0. Do not add
disable-comments or refactors for it. The real coverage gap was the file
list (missed `claude-launcher`, `bench/*`, `hooks/*`); the new paths gate
at `-S error` because the old hook scripts carry pre-existing info/style
notes out of scope to reformat.
