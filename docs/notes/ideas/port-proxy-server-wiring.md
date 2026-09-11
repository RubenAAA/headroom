# Idea: port the proxy server wiring delta

- **Status:** open (large)
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
