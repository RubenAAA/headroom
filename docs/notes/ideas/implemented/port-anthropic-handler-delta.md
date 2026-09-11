# Idea: port the Anthropic handler delta

- **Status:** shipped 2026-09-11 (every row dispositioned)
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `proxy/handlers/anthropic.py` (+1446/-868) — mixed tool/CCR
  stream handling, cache-control normalization.
- **Dispositions:** rows 1/2/5 shipped; 3/4 covered by chain-id /
  conversation-key; 6 cold-prefix fork (`HEADROOM_COLD_RECOMPACT`, default
  off, Spark reasoning-summary strip on cold turns); 7 skipped (no queue);
  8a content-encoding strip for plain-JSON bodies; 8b closed as N/A
  (client abort cancels the handler future — no response deliverable,
  verified live, implementation reverted); 9 audited (single-tokenizer
  pairs only); 10 compact-summary skip (never triggers proactive
  expansion). Stage-matrix discipline held: each behavior change an
  approvable row, never a drive-by.
