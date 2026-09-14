# Idea: read the forwarded-prefix-mutated instrument against live traffic

- **Status:** implemented 2026-09-11 (09-11 re-audit)
- **Source:** `docs/notes/recache-classification.md` ("residue and its instrument")
- **Summary:** 58 turns / 1.29M tokens of replay-applied-yet-lost residue had no
  separating event, because nothing recorded whether forwarded bytes still
  matched the replayed prefix. `forwarded_prefix_mutated_after_replay` (WARN,
  per-message digests at replay-exit vs pre-forward, last two messages
  excluded) plus the length-change companion now record exactly that —
  single-request invariant, no cross-turn state needed.
> **09-11 outcome:** zero fires over ~2,586 streams — stages exonerated for the window.
- **Next (superseded):** read it live. Fires → a proxy stage corrupts a cached prefix (index
  names which). Never fires → proxy exonerated, residue is provider-side.


## Tail confirmation

*moved from `docs/notes/recache-classification.md`*

- **`forwarded_prefix_mutated_after_replay`: zero fires** (length companion
  likewise zero) across ~2,586 streams. Per this file's own rule the proxy
  stages are exonerated for this window — no stage corrupts the settled
  prefix between replay and forward.
