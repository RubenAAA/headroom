# Learning: traffic independently proved the thesis twice

- **Source:** `docs/context-mode-integration-analysis.md` §8
  (`audit/reads.py` docstring)
- **Claim:** (1) message-history dedup measured 0.1% of Read bytes — the
  addressable bytes are at the tool boundary, matching the realignment's
  cache-side conclusion from the opposite direction. (2) "Context residency"
  argues compress-before-cache-entry from Headroom's own traffic; context-mode
  is the terminus: compress before *context* entry.


## Audit corroboration

*moved from `docs/context-mode-integration-analysis.md`*

**`headroom/audit/reads.py` does not overlap P3 — and it independently validates the whole thesis.**
It is a *measurement* tool, not an audit trail: it streams Claude Code `*.jsonl` transcripts to size
"the addressable bytes for each Read compression mechanism... so defaults are set from traffic, not
theory." No policy, no tamper-evidence. P3's audit trail remains a gap.

Two lines in its docstring are the most useful corroboration in either repo:

- *"context residency — how many assistant turns each Read stays in context (the multiplier on its
  prefix-cache read cost; **the case for compress-before-cache-entry**)"* — Headroom is already
  arguing, from its own traffic, for moving earlier in the pipeline. context-mode is the terminus of
  that argument: compress before **context** entry, not merely before cache entry.
- *"identical repeat — a dedup mechanism for this was prototyped and removed: it measured 0.1% of
  Read bytes on real traffic."* — Headroom has already empirically established that
  message-history-level dedup is worthless. The addressable bytes are at the tool boundary, not in
  history. That is the same conclusion the realignment reached from the cache side, arrived at
  independently from the traffic side.
