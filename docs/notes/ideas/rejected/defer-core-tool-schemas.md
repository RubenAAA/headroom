# Idea: defer built-in (core) tool schemas server-side

- **Status:** rejected 2026-09-28 before a live run — the core set left to
  defer is about 2.1k tokens, and the bar set below is 10k.
- **Source:** upstream `85f9e01d` "Also: HEADROOM_TOOL_SEARCH_CORE_TOOLS"
  (Sept 2026). Claude Code defers its built-ins since v2.1.69 (~14-16K
  schema tokens down to under 1K); behind a proxy the client stops
  deferring and we keep those resident. Upstream added the env override
  but left the default unchanged: our deferral is server-side, so first
  use of a deferred tool costs a search round trip, and that
  accuracy/latency trade is unmeasured.
- **Value:** potentially ~13-15K tokens per turn if the round trip
  proves cheap; nothing if it doesn't. Must be measured, not guessed.
- **Progress (Sept 2026):** experiment plumbing landed, still unrun.
  1. Gate: `HEADROOM_TOOL_SEARCH_CORE_TOOLS` ported from upstream
     (`tool_search_deferral.rs:67-126`): unset keeps the 19-name
     `CORE_TOOLS`, empty defers everything non-typed, otherwise a
     comma-separated list normalized via `resident_key()`. Read per
     request, no restart. `toolsearch` is hard-exempt regardless — it
     resolves client-local tools never in the body, so deferring it
     orphans them. Wired into `inject_deferral` (`:301-307`); the
     `deferred == 0` no-op guard and third-party strip path are
     unchanged. Documented in `docs/content/docs/configuration.mdx`.
  2. Metrics: `core_deferred_tokens` tag (disjoint slice of
     `tool_search_deferred_tokens`, emitted only when nonzero;
     `proxy.rs:2707`, `forward.rs:690`) plus `tool_search_calls` /
     `tool_search_results` joined to the same outcome row at SSE close
     (`proxy/sse_anthropic.rs:449`), promoted from
     `server_tool_inventory()` (`sse/anthropic.rs:629`). Latency
     (`total_latency_ms`) and 400/retry rate (`status_code`) already
     join by `tool_search_mode`, so no new fields were needed there.
     The routed (Spark) path books no search tags: deferral is an
     Anthropic-path feature, so the experiment runs first-party only.
- **Next:** run as an experiment against real traffic with the core
  set deferred (set the env empty on one deployment, leave unset on
  control): measure added search round trips (count + latency) vs.
  schema tokens saved over >=1 week. Suggest fixing thresholds first:
  median core saving >=10K input tokens/turn AND p90 added latency <2s
  AND <=1 extra round trip/turn median AND 400/retry rate flat vs
  control. Ship only on a positive trade. Record the numbers here
  either way, then move per `ideas/README.md` (`implemented/` with
  commit/flag, `rejected/` with the killing number).

## Findings 2026-09-28 — the prize was already taken

The 13–15K figure is Claude Code's whole built-in set. Default deferral
(`HEADROOM_TOOL_SEARCH` on, `31c99a07`) already moves everything outside
`CORE_TOOLS` behind the search tool: the last 2,000 `tool_search_deferral`
events deferred a median of 13 tools and 8,262 tokens per request. What stays
resident on the wire (netvalue `out/`, 09-25, all 577 Claude turns):

| tool | tokens (chars/4) |
|---|---|
| Bash | 653 |
| Skill | 443 |
| Read | 392 |
| Edit | 235 |
| WebFetch | 201 |
| Write | 150 |
| **total** | **~2,074** |

That is a fifth of the 10K median saving the thresholds above demand. It is
also the set of tools almost every session calls first, so deferring it
would add a search round trip to nearly every conversation to save a cached
prefix of 2k tokens. Not run.

Note for any later attempt: the env var is read per request, but a running
process cannot have its environment changed from outside, so flipping it
still takes a proxy restart.
