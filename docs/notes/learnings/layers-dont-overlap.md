# Learning: pre-context and in-context don't overlap where it matters

- **Source:** `docs/context-mode-integration-analysis.md` §1 (2026-07-29)
- **Claim:** admission control (block/redirect/sandbox/externalize before data
  enters) vs compression (squeeze after it entered). Headroom's live zone —
  latest user text + latest tool outputs — is precisely what context-mode
  intercepts a layer earlier. Complements: the upstream position has nothing
  to compress, invalidate, or token-validate. Three unlocks in value order:
  cache safety (structurally zero bust risk), subscription safety (invisible
  upstream — deployable where the proxy can't go), proxy-free deployment
  (verified: proxy down → stats zeros, compress no-ops).
