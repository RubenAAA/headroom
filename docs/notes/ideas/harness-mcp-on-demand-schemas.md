# Idea: MCP server on-demand schemas (§3)

- **Status:** open — needs prevalence proof before any build
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §3 (magnitudes only: rare-tool offload 60% of tool-description tokens; MCP offload 46.9% of total in sessions using them). Proxy: `--prune-drop-mcp`/`--prune-drop-tools` (drop), `tool_search_deferral.rs:1-33` (defer via injected `tool_search_tool_regex`; tools prefix still caches; savings in tags), `CORE_TOOLS` + `MIN_TOOLS=12`.
- **Not a retry of:** `rejected/tools-block-cuts.md` (2026-09-03: pruning harder returns little at 0.1× read pricing, $32/day, quality trade) — this is deferral (excluded from first-party billing until searched), not pruning. `rejected/port-sdk-integrations-on-demand.md` (don't port framework integrations until users exist) — same principle applies here: no build until MCP-heavy sessions are shown in our traffic. `defer-core-tool-schemas.md` (open, built-ins) — this is the MCP-server half, grouped per server.
- **Constraints from existing work:** keep tool order stable (`enable-e1-e2-sort-ab.md`; `first-turn-write-sharing.md` D1: only 1 tool-set in 2 orders, wrong sort breaks history reads). Deferral must be order-preserving and deterministic so the tools prefix still caches. Resident set keeps high-frequency + hallucinated-when-absent tools.
- **Value:** only if some sessions carry heavy, rarely-called MCP servers AND the win shows in creation cost (mass ≠ billed cost on subscription — reads free).
- **Next:** per baseline idea: MCP schema tokens/turn, session share, call rate per server (doc's <20% rule). If rare AND heavy, prototype behind a flag as grouped per-server pointers. Track billed cost, latency, extra round trips, tool-call errors, task success.
- **Traps:** offloading a turn-one tool or one the model calls when missing. Ship behind flag only on win with flat errors; reject with the number otherwise.

## Findings 2026-09-24 — prevalence measured, parked pending core-deferral (`section_cost_baseline` tables 1+3)

Schema leg (`tools:mcp` billed equiv): netvalue 3.2% (1.71M/53.0M, ~3.8k schema tokens/turn), blindguard 3.8% (8.30M/219.7M, ~5.5k/turn). Call rates clear the doc's <20% bar for the top candidates: `search_code` 12.6%/16.4% of turns (22–23% of sessions), `search_graph` 2.8–3.1%, athena `manage_aws` 14.9% of blindguard turns. Shape is textbook offload (present every turn, called rarely) — but the ceiling is the whole leg, ~3–4% of bill, minus search round trips (each a full-prefix read + latency + error risk).

- **Parked, not rejected:** the core-deferral experiment (`defer-core-tool-schemas.md`, ~13–15k tokens/turn prize) uses the identical mechanism for ~3× the prize. Its round-trip findings (added searches, latency, 400/retry rate) price this idea's cost side exactly. Revisit when it reports; if round trips prove cheap there, MCP servers are the next tranche. Do not prototype independently before then.
- **Lane split:** `result:mcp` (blindguard 1.6%, athena alone 6.4M result tokens) is a compression/offload target, not a schema target — belongs to ctx-offload, not here.
