# Implemented: --ccr-inject-marker wired at the store gate

- **Status:** wired 2026-08-07
- **Source:** `docs/notes/rust-parity-gaps.md` §8
- **Summary:** the flag (and `ContentRouterConfig.ccr_inject_marker`) existed
  but nothing read either. Gate placed at `AppState::ccr_store()`: false
  withholds the store, marker text and store writes stop together (suppressing
  text alone would offload blocks the model can't ask back — Python pairs them
  the same way).


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

- **`--ccr-inject-marker`** — WIRED (2026-08-07). The entry above said the
  feature "genuinely exists in `content_router.rs`; the proxy flag simply is
  not wired to it". Half right: `ContentRouterConfig.ccr_inject_marker` is
  declared and defaulted `true` but read by nothing either, and the proxy never
  constructs a `ContentRouterConfig` at all. What actually decides whether a
  `<<ccr:HASH>>` marker is emitted is whether the caller hands compression a
  `CcrStore` (`live_zone.rs:1836`, early-returns on `ccr_store.is_none()`).
  So the flag is now gated at `AppState::ccr_store()` — false withholds the
  store, and marker text and store writes stop together. Suppressing only the
  text would offload blocks the model has no handle to ask back. Python pairs
  them the same way: every `ccr_inject_marker=False` call site also passes
  `ccr_enabled=False`.
