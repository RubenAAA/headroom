# Idea: intelligent task-aware model routing with cost x cache x limits

- **Status:** open, but phase A is dead — measured 2026-09-28, its recipe
  matches 2 of ~4,500 Claude requests in a month (see Findings). Phase B
  waits on `harness-whole-tree-cost.md`.
- **Source:** investigation 2026-09-22 (proxy routing, cost model, task signals)
- **Value:** user never chooses a model; proxy dispatches each task to the
  cheapest model that can solve it, aware of model prices, recache costs, and
  remaining limits.
- **Constraint (non-negotiable):** decide only on `NewUserAsk`, then pin for
  the whole conversation. Per-turn rerouting loses: one 134k opus turn moved
  to sonnet costs a 134k sonnet 1h-write (~$0.54) vs ~$0.08 opus cache-read +
  output. See `rejected/per-turn-model-routing.md`, `learnings/cost-saves-measured.md`
  (91% input from cache at 0.10x; writes 55% of bill).

## What already exists (extend)

- Router mechanism: `crates/headroom-proxy/src/model_router.rs` (opt-in ordered
  `CostRule`, `estimate_input_tokens`, cooldown + fallback to client model via
  `routed/routing.rs:150-209`, `local_model.rs:615-670`). No price lookup, no
  quota/budget input.
- Price table: `crates/headroom-core/src/pricing.rs` (per-1M + 5m 1.25x / 1h
  2.0x, `muse-spark` free). `CostTracker::check_budget()` exists.
- Limits feed: `observability/proxy_metrics.rs:270-364` scrapes
  `anthropic-ratelimit-*-remaining` + unified utilization + Codex headers into
  gauges; surfaced via `/codex-limits`, `/cache-health`. No decider consumes it.
- Task signals (structural only): `body[model,messages,tools,system]`,
  `output_config.effort` (`output_shaper.rs:48`), `classify_turn()` NewAsk vs
  Mechanical/ErrorContinuation (`output_shaper.rs:98-157`). No client task
  header today; subagent choice (`contrib/claude/agents/codex-*.md`) collapses
  to a model string.
- Cache safety: `identity_model` preserved for session/prefix keys
  (`routed/transforms.rs:173`) — keep using it.

## Design

```
NewUserAsk? no -> passthrough (protect cache)
  yes -> classify {lookup, mechanical-write, plan, code}
    from size + has_tools/tool-names + effort + messages.len()
  -> score: P(miss)*write_rate*size + read_rate*cached + out_rate*E[out]
    skip cooldown / quota~0 / over-budget targets
  -> pin (user_id + prefix hash) for conversation lifetime
  -> on 429/5xx or ErrorContinuation: cooldown cheap target, escalate one tier
```

No hot-path I/O, LLM call, content regex, or real tokenizer.

## Next (phased)

- **A (days):** gate `select()` on NewAsk; add `turn_kind`, `tool_name_any`,
  `effort_any` to `CostRule`. One recipe: toolless + small + effort low/medium
  -> cheap alias. Measure bill scoped by date.
- **B (weeks):** tier map + session stickiness; warn on mid-conversation split.
- **C (only if A/B wins after write costs):** full optimizer + escalation fed by
  live hit stats (`observability/cache_hit_rate.rs`), `/codex-limits`,
  `ModelCooldowns`.

## Findings 2026-09-28 — phase A has nothing to route

Every Claude request body in `~/headroom-capture-netvalue` (2026-08-23 to
09-25), bucketed on the signals phase A would see:

| model | tools | messages | size | effort | requests |
|---|---|---|---|---|---|
| opus | yes | >3 | >20k | medium | 2,140 |
| opus | yes | >3 | >20k | high | 1,154 |
| sonnet | yes | >3 | >20k | medium | 935 |
| opus | yes | >3 | >20k | max | 149 |
| sonnet | no | 2–3 | ~47k | none | 98 |
| everything else | | | | | 52 |

- The phase A recipe (no tools, small, effort low or medium) matched 2
  requests, about 1.2k tokens. No request sent `effort: low`.
- The 98 toolless sonnet calls are Claude Code's own permission classifier
  (`<transcript>` prompt, `max_tokens` 64 or 8192). Claude Code chose sonnet
  for them on purpose, and a weaker safety check is not a saving worth having.
- The spend sits in long tool-using opus conversations. A `NewUserAsk` inside
  one of them cannot move without a full rewrite of its prefix, which is the
  loss `rejected/per-turn-model-routing.md` measured. First turns, the only
  place a pin is free, are under 1% of requests.

So the only form of this that could pay is phase B: pick the model once, on
the first turn, and pin it. That picks the model for a whole agentic session
from one prompt, and needs the per-tree cost from
`harness-whole-tree-cost.md` to show it did not just move the cost into
retries. Do not build phase A.
