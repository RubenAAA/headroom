# Idea: intelligent task-aware model routing with cost x cache x limits

- **Status:** open
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
