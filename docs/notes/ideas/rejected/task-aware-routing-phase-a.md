# Idea: route small toolless asks to a cheap model (task-aware routing, phase A)

- **Status:** rejected 2026-09-28 — the recipe matched 2 of ~4,500 Claude
  requests in a month (about 1.2k tokens). Split out of
  `../intelligent-task-aware-routing.md` on 2026-10-12; phase B stays open
  there.
- **Source:** investigation 2026-09-22; measured on every Claude request body
  in `~/headroom-capture-netvalue` (2026-08-23 to 09-25).
- **Idea:** gate `select()` on `NewUserAsk`; add `turn_kind`, `tool_name_any`,
  `effort_any` to `CostRule`. One recipe: toolless + small + effort low/medium
  -> cheap alias.

## Findings

Bucketed on the signals phase A would see:

| model | tools | messages | size | effort | requests |
|---|---|---|---|---|---|
| opus | yes | >3 | >20k | medium | 2,140 |
| opus | yes | >3 | >20k | high | 1,154 |
| sonnet | yes | >3 | >20k | medium | 935 |
| opus | yes | >3 | >20k | max | 149 |
| sonnet | no | 2–3 | ~47k | none | 98 |
| everything else | | | | | 52 |

- No request sent `effort: low`.
- The 98 toolless sonnet calls are Claude Code's own permission classifier
  (`<transcript>` prompt, `max_tokens` 64 or 8192). Claude Code chose sonnet
  for them on purpose, and a weaker safety check is not a saving worth having.
- The spend sits in long tool-using opus conversations. A `NewUserAsk` inside
  one of them cannot move without a full rewrite of its prefix, the loss
  `per-turn-model-routing.md` measured. First turns, the only place a pin is
  free, are under 1% of requests.

Retry only if traffic changes shape: many short toolless asks, or clients
sending `effort: low`.
