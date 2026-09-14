# Learning: sidecar fallbacks poison the main session's cache

- **Status:** mechanism fixed via `ideas/implemented/sidecar-strip-long-context-beta.md`
- **Source:** `docs/notes/savings-ideas-2.md` §4.3
- **Claim:** a fallback forwards the full request under a *different system
  prompt* — 17/50 system drifts + 16/30 early-message drifts in the AM window
  were the sidecar itself, each dropping the replay store (43/1000 vs 1/1000
  baseline; 590k re-write downstream). A four-word spinner answer cost a
  session's prefix.
