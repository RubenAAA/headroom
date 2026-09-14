# Learning: parity-recorder limits (tokenizer cost, CCR shape)

- **Source:** `docs/notes/rust-dev.md` (Phase 0 blockers)
- **Claim:** `cache_aligner` fixtures record only if a usable tokenizer is
  cheap (else logged blocker + skip); CCR has no single class — the recorder
  targets encoder-style entry points (`inject_tool`, `parse_response`), and a
  different Phase-1 split means updating `record_all`.
