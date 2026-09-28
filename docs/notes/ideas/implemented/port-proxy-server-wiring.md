# Idea: port the proxy server wiring delta

- **Status:** implemented 2026-09-28 — the full `server.py` re-diff is done
  and nothing left changes forwarded bytes.
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `proxy/server.py` (+838/-67) — request-scope import guard,
  health/readiness changes.
- **Next:** re-diff; port the wiring that has no Rust equivalent yet.
- **Update 2026-09-11:** shipped slices — `--provider-name` + upstream-host
  display detection (`display_provider.rs`, `/stats by_provider`),
  `/stats tool_search` layer, `/livez` + `/readyz` (config-echo `/health`),
  dashboard authz (`HEADROOM_PROXY_TRUSTED_DASHBOARD_CLIENT_CIDRS`,
  loopback+same-origin `/stats/reset`), `/stats-lifetime`. Stays open: no
  full `server.py` re-diff done — remaining wiring still needs it.

## Re-diff 2026-09-28 (`42ebbc6c..964671d8`, 71 commits, +1836/-269)

25 already in Rust, 41 N/A (uvicorn/asyncio runtime, `/v1/compress`
sidecar, dashboard routes, extension seams, `/p/<project>` routing), 5 left.
The backlog's "request-scope import guard" appears in no `server.py` diff.

Left, none cache-relevant:

- `250ede2f`: log the savings profile at startup (`main.rs:130`
  `log_startup`). One line.
- `01df2452`: `--budget-estimated-basis count|ignore|block` for spend booked
  from Headroom's own estimate (`admission.rs:95`). Matters only when budgets
  are enforced on turns without provider usage.
- `074f0ae9` + `1390d897`: `/stats output_reduction`. Needs a shaper ledger
  first, so it lives in `port-output-shaper-policies.md`.
- `31452426`: per-row `savings_breakdown` on `/stats` recent requests.
  Optional polish.
