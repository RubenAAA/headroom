# Implemented: CCR marker knob honored end-to-end

- **Status:** honored 2026-04-29 (Rust + shim + tests)
- **Source:** `docs/notes/rust-dev.md` (SmartCrusher table)
- **Summary:** `SmartCrusherConfig.enable_ccr_marker`; `crush_array` checks it
  before marker text AND store write (storing under an off-switch is
  pointless). Python collapses both `enabled`/`inject_retrieval_marker` to the
  same gate. Scope: row-drop sentinel path only.
