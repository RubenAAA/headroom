# Learning: whatever the proxy injects, that same path answers

- **Source:** `docs/notes/rust-parity-gaps.md` §5–6 →
  `tests/integration_tool_invariant.rs`
- **Claim:** three incidents, one shape — injected tool reaches a client that
  never heard of it (`No such tool available`, model blamed for proxy's
  invention). Injection is per-request, resolution per-path; nothing tied them
  together until the wire-truth test did. Companion pattern: an inert-today
  `can_resolve`-style gate makes lifting a restriction safe by construction
  (whoever extends injection gets the exclusion for free).
