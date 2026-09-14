# Rejected: pool-swap endpoint for rotations

- **Status:** deliberately not built
- **Source:** `docs/notes/vpn-egress-learnings-2026-09.md` (proactive cycling)
- **Summary:** needs `AppState.client` behind a lock — churn across every
  sender — for something drain + idle-shrink already cover.


## Detail

*moved from `docs/notes/vpn-egress-learnings-2026-09.md`*

NOT built (deliberately): a pool-swap endpoint (needs `AppState.client`
behind a lock — churn across every sender — for a window drain + idle
shrink already cover); semantic hold for thinking models (SSE parsing in
the retry layer + first-paint delay on every thinking turn).
