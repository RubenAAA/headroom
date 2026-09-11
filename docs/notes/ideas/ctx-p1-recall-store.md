# Idea: P1 headroom-recall — FTS5 lossless store on memory_text/ccr_backend

- **Status:** open (IP gate cleared 2026-09-11 by owner: both repos open
  source, personal use only, no third-party hosting/distribution — revisit if
  derived code is ever distributed; lands inside Phase B)
- **Source:** `docs/context-mode-integration-analysis.md` §4 P1 (2026-07-29)
- **Summary:** port context-mode's `store.ts` (tokenized + trigram FTS5,
  auto-externalize >100 KB, nothing discarded) behind the existing
  `headroom.memory_text` seam. Fixes retrieve-by-hash-only: query instead of
  hash. Doubles as the persistent CCR backend Phase B wants. Port the store,
  not the MCP surface. Medium effort.
- **Next:** build against the memory interface; write the plugin-authoring
  doc as part of it (§8 has none).


## Proposal

*moved from `docs/context-mode-integration-analysis.md`*

### P1 — `headroom-recall`: FTS5+trigram lossless store as `headroom.memory_text`

**What:** port `src/store.ts` behind the existing `headroom.memory_text` seam.

**Why this first:** it is the smallest diff onto an *already-existing* contract, and it fixes a real
product limitation. Today `headroom_retrieve(hash)` requires you to *know the hash* — the tool
description literally says "hash comes from compression markers like `[N items compressed... hash=abc123]`".
With an FTS5-backed store you get `retrieve-by-query`: "what did that build log say about OOM"
instead of "paste hash abc123". The trigram index matters specifically because BM25 tokenization
loses identifiers and stack frames.

Composes rather than replaces: `compress` → return squeezed text + hash → store the *original* in
FTS5 → rehydrate by hash **or** by query. Also a natural `headroom.ccr_backend` implementation —
the realignment wants "CCR hardens: persistent backend" (Phase B), and this is one.

**Enterprise variant:** shared team store, retention/TTL policy, per-project scoping (context-mode
already has `project-attribution.ts`), audit of every retrieval.

**Effort:** medium. Reimplement in Python/Rust against Headroom's memory interface, or ship the
node store as a sidecar. Do not port the MCP tool surface — only the store.
