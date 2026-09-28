# Idea: tool-error taxonomy per tool × model (§5)

- **Status:** rejected 2026-09-28 — every frequent error is set up outside
  the proxy, so a proxy-side taxonomy has nothing to fix.
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §5 (classify expected: bad args, environment, provider, timeout, abort; unknowns as harness bugs; per tool/model tracking cut unexpected errors 10×). Gap verified: no taxonomy and no per-tool error-rate join in `crates/headroom-proxy/src` — only bust attribution, status codes, and `tool_search_calls`/`results` at SSE close (`proxy/sse_anthropic.rs:449` pattern).
- **Scope (to avoid duplicating siblings):** this is model→tool call errors (bad args, env, timeouts, aborts). Provider failures (429/529, rate limits) belong to `implemented/label-otel-persistent-failure-paths.md` (verified in `/stats` 2026-09-28) — sibling, not duplicate. Cache busts belong to attribution — different lane.
- **Pricing note from learnings:** `failed-turns-cost-nothing.md` (146 failed turns billed zero output) — error cost is input/prefix re-read + debris in later turns, not output. Price it that way. Per `profile-live-not-offline.md`, rank from live events, not an offline harness.
- **Next:** extend the SSE-close outcome join (no new pipeline): expected classes + unknown-as-bug, rate per tool per model, input-cost of error turns. Fix the top offender in its own revertible commit. Guardrails: error rate, tokens/turn, turns/task, hit rate.
- **Exit:** taxonomy half closes when dashboarded; each fix carries its before/after.
- **Interim instrument (no new code):** `section_cost_baseline` table 3 already counts `is_error` per parent tool — that is the seed list until the join exists. Live logs carry zero tool-error signal (verified 2026-09-24: bodies aren't logged, only digests), so this mines captures, never logs.

## Seeds 2026-09-24 (from the baseline runs)

Blindguard (7,839 turns): WebSearch 37.4%, `search_code` 23.9%, TaskOutput 13.3%, TaskStop 12.5% vs Read 0.4%, Write 0.0%. Netvalue: Agent 21.6%, `search_code` 33.7%. First fix candidate is whichever of WebSearch/`search_code` reproduces on the fresh capture — both outlie by an order of magnitude against quiet tools.

## Findings 2026-09-28 — the errors belong to the client setup

Netvalue capture (to 09-25), errors per distinct `tool_use_id` (the
`section_cost_baseline` err% column now counts calls, not re-sent blocks):

| tool | errors / calls | cause |
|---|---|---|
| Bash | 162 / 4,224 | 72 non-zero exit codes; 36 a hook denying grep over Go source; 8 permission denials |
| Agent | 19 / 79 | 13 "fork not available inside a forked worker", 6 "teammates cannot spawn teammates" |
| `search_code` (codebase-memory MCP) | 9 / 22 | all "project not found or not indexed" |
| athena query executions | 10 / 101 | query failures (missing table, Iceberg metadata) |
| Edit | 2 / 185 | a placeholder hook, one ambiguous match |

The fresh baseline-202609 capture shows only Bash (6.3%), and no WebSearch
or `search_code` at all. None of these is a bad request the proxy sent or
could repair. The fixes live in agent prompts, hooks and MCP indexing:
tell forked workers not to fork, index the project before `search_code`.
