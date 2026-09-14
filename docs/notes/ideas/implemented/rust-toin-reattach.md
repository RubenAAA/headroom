# Implemented: TOIN learning loop re-attached after retirement

- **Status:** re-attached 2026-04-28 (tests pinned)
- **Source:** `docs/notes/rust-dev.md` (SmartCrusher table)
- **Summary:** the PyO3 shim's `crush()`/`_smart_crush_content()` call
  `toin.record_compression()` post-compression, filtered on
  `strategy != "passthrough"`, best-effort (debug-logged, never breaking).
  Per-tool hook partially threaded (`tool_name` → record; per-tool overrides
  not driven yet).
