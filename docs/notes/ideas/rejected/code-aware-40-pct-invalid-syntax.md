# Rejected: code-aware compression (40% invalid syntax)

- **Status:** rejected upstream in `0.5.22` (commit `0f7f7b0f6`, 2026-04-11) —
  standing verdict, live `--code-aware false`
- **Source:** `CHANGELOG.md` (`[0.5.22]` / Changed): "AST-based code
  compression produced invalid syntax on 40% of real files. Code now passes
  through uncompressed."
- **Summary:** the line-cut compressor of that era broke real files often
  enough that upstream disabled it by default (re-enable: `--code-aware`).
  Same release fixed statement-based truncation ("walks AST statements, never
  cuts mid-expression"), so the fix and the rejection shipped together — the
  40% number was never re-measured after the fix.
- **Re-test proposal:** `../code-aware-reenable-ab.md` (open) argues the
  number is stale and lays a 3-rung ladder (syntax-valid rate →
  anchor-fidelity → live A/B). Overturning this file requires completing that
  ladder, not re-arguing it.
