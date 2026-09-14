# Implemented: audit-safe SmartCrusher mode (Rust port)

- **Status:** done 2026-07-10 (335 smart_crusher + full suites green)
- **Source:** `docs/notes/rust-parity-gaps.md` §3 (Python `bb112dd1`)
- **Summary:** `SmartCrusherConfig.audit_safe` + `protected_patterns` +
  fail-closed verification in the crush path (default off, byte-identical
  otherwise). Simpler than Python: only the statistical row-drop case needs
  guarding (no CCR markers emitted yet). Invalid regex panics at construction,
  matching Python's `ValueError`.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 3. Audit-safe compression mode (SmartCrusher) — DONE (2026-07-10)

Ported to `crates/headroom-core/src/transforms/smart_crusher/{config.rs,crusher.rs}`:
3 opt-in config fields (default off, byte-identical for non-opt-in callers),
5 helper primitives (canon/scan/splice/verify), wired into
`SmartCrusher::smart_crush_content` (the path `apply()` actually calls for
real tool-output compression). Simpler than Python: Rust's crush pipeline
emits no CCR markers yet, so only the statistical row-drop case needed
guarding, not the marker-hidden case. Invalid regex patterns panic at
construction (matches Python's `raise ValueError`; every SmartCrusher
constructor is infallible, so `Result` threading was out of scope). 335
smart_crusher tests pass (7 new), full core (1637) + proxy (979+) suites
green.

<details><summary>Original scoping notes</summary>

- Python: `bb112dd1` (#1899), new ~258-line module
  `headroom/transforms/smart_crusher.py` additions: `audit_safe` config,
  `protected_patterns` (regex/marker matching), fail-closed verification so
  compliance-relevant content (audit markers, leakage flags) can't be
  silently dropped or rehidden by compression.
- Rust: `crates/headroom-core/src/transforms/smart_crusher/crusher.rs` and
  `config.rs` have no `audit_safe`/`protected_pattern`/`fail_closed` concept
  at all (confirmed via grep across every branch in the repo, including the
  several branches with "audit" in the name — those are about an unrelated
  CCR/"toin" audit concept, not this feature).
- Scope: net-new feature — `SmartCrusherConfig.audit_safe` /
  `protected_patterns` / `fail_closed_on_protected_loss` fields, plus
  scan/splice/verify logic in the crusher's compression path.

</details>
