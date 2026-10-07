# Learning: a fresh OpenCode install yields a session Zen's free tier accepts

- **Source:** live test, 2026-10-07, while making `install.sh` install the
  OpenCode CLI (commit `f65bf7a6`).
- **Claim:** Spark needs the OpenCode CLI but no OpenCode account. A CLI
  installed into an empty HOME, never logged in, creates a session through
  `opencode run`, and the proxy serves Spark on that session's id.
- **Status:** confirmed, with a control.

## Detail

Zen's free tier refuses any request whose `x-opencode-session` it does not
know (see `resolve_zen_session` in `routed/quirks.rs`). The open question
was whether a brand-new, anonymous install produces an id Zen knows, or
whether the gate wanted a logged-in or older session.

1. OpenCode 1.18.35 installed with its own script into an empty HOME.
   `opencode run --pure -m opencode/muse-spark-1.3-contributor-free` replied
   "ok".
2. A debug proxy saw only that HOME: no `opencode` on its PATH, no
   `HEADROOM_ZEN_SESSION`, so its sources were the fresh database or the
   made-up fallback. Spark request through it: **HTTP 200, "ok"**.
3. Control, same proxy and lane, `HEADROOM_ZEN_SESSION=ses_<random>`:
   **HTTP 403**, "OpenCode's free tier can only be used from within
   OpenCode".

All three ran through one egress-relay lane. The first attempt, from the
machine's own address, got a 429 in the run's model call, which says
nothing about the session. The 429 also left the session row behind, so a
row in the database does not prove Zen accepted it.

## Traps

- Bun, OpenCode's runtime, rejects SOCKS proxies
  (`UnsupportedProxyProtocol`), so `HTTPS_PROXY=socks5://…` cannot put
  OpenCode on a relay lane. The test used a throwaway HTTP CONNECT to
  SOCKS5 bridge.
- In step 2 the proxy logged one `zen_egress_rate_limited` on the lane
  before the 200: the per-exit limit applies off the home address too.
