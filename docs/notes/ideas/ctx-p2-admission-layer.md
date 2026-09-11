# Idea: P2 headroom-admission — tool-boundary admission across 18 hosts

- **Status:** open (gated: after Phase A stabilizes; IP gate cleared 2026-09-11 — owner-confirmed personal use)
- **Source:** `docs/context-mode-integration-analysis.md` §4 P2
- **Summary:** the strategic piece: pre-wire enforcement (no cache-bust, no
  token-validation fallback), 18-host coverage (is Phase G's wrap-CLI work,
  already done), subscription-auth deployment where the proxy can't go, and
  the DLP story (egress control via `ctx_fetch_and_index` — different buyer).
  Ship as TS package under `plugins/`, report into `savings_ledger.py`.
  Mostly packaging + reporting bridge.
- **Next:** needs P1's seams; fills a dimension (host
  coverage) Headroom doesn't track — needs a second matrix, not new rows.


## Proposal

*moved from `docs/context-mode-integration-analysis.md`*

### P2 — `headroom-admission`: tool-boundary admission control across 18 hosts

**What:** context-mode's adapter + hook layer, distributed the way `plugins/openclaw` and
`plugins/opencode` already are (TS package under `plugins/`), reporting savings into Headroom's
`savings_ledger.py` JSONL and emitting Headroom pipeline events.

**Why:** this is the strategic piece. It gives Headroom:
- a **pre-wire** enforcement point, upstream of Phase B's live-zone engine, with no cache-bust and
  no token-validation fallback required;
- coverage of **18 agent hosts** — the realignment's Phase G wants to "extend wrap CLIs (cline,
  continue, goose, openhands)"; this is that work already done, and then some;
- a deployment mode that works under **subscription auth**, where the proxy is a revocation risk.

**Enterprise value — this is the DLP story Headroom cannot currently tell.** A `curl` inside a Bash
tool call never touches the proxy, so Headroom is blind to it. context-mode blocks
`curl`/`wget`/`WebFetch`/inline `fetch()`/`requests.get` at the tool boundary and forces network
egress through `ctx_fetch_and_index`. That converts a token-savings feature into an
**egress-control** feature — a different budget line and a different buyer.

**Effort:** high, but it's mostly packaging + a reporting bridge, not a rewrite. Keep it TypeScript;
Phase H retires Python *proxy* code but explicitly preserves "CLI wrappers, RTK installer" — the
installer layer is the surviving Python, and it can shell out.
