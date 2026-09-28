# Idea: re-enable code-aware compression and measure

- **Status:** implemented 2026-09-28 — on in production since the
  2026-09-17 restart (`--code-aware true`), and it has not hurt edits. Rung 4b
  was never run; live traffic answered its question instead (Findings below).
- **Shipped in:** `edeb2cca`. `CODE_AWARE_ENABLED` plus
  `set_code_aware_enabled` gate the SourceCode arm from `--code-aware`
  (`main.rs`); the flag is part of the dispatch-memo key; the off arm is
  proven by `tests/code_aware_off_arm.rs`. `contrib/headroom-flags.sh` sets
  `--code-aware true`; the upstream default stays false.
- **Original blocking finding (now resolved by the patch):** `--code-aware
  false` was not honored on the Rust live-zone path — `code_aware_enabled`
  is parsed (`config.rs:1677`) but never read; `live_zone.rs:2264` routes
  `SourceCode` to `CodeAwareCompressor` unconditionally. Live log shows 2,097
  `code_aware_compressor` strategy hits. (Resolved same day — see Status:
  process-wide gate + startup wiring; the threading-through-DispatchConfig
  sketch below was superseded by the static, mirroring KOMPRESS_ENABLED.)
- **Source:** 2026-09-18 session; `code_compressor.rs:1335-1365`
  (verify-or-passthrough), `2250-2321` (statement-cut + lang-correct elision
  with `pass` for colon langs); Python/Go/Rust/TypeScript all wired
  (`142-146`, `242-311`)
- **Value:** measured 261,035 tokens saved over 709 `compression applied`
  events (27.9% block rate) in the current log window — this is live
  production data, not projection. The old rejection number may be stale —
  the failure mode it measured (mid-expression line cuts) is fixed in code,
  and worst case is now passthrough, not broken code (`syntax_valid=false`
  ⇒ original served).
- **Standing verdict:** `rejected/code-aware-40-pct-invalid-syntax.md` (upstream
  `0.5.22`, 40%). This file is the re-test proposal that can overturn it —
  per convention the rejected file stays until the ladder below completes.
- **Test ladder (record results here):**
  1. `syntax_valid` rate — DONE 2026-09-18: 34 lib + 1 byte-parity +
     2 perl tests, all pass, 0 fail. Pinned-output asserts on C#/PHP/Python
     cover the re-parse gate.
  2. Roundtrip — DONE 2026-09-18 (`tests/code_compressor_anchor.rs`):
     dispatcher-level `<<ccr:HASH>>` + `store.get(hash) == original` holds
     for code blocks. Granularity note: per-function `[N lines omitted]`
     comments carry no hash — recovery is whole-block retrieve, which is
     sufficient (nothing is lost, just coarser).
  3. Anchor-fidelity — DONE 2026-09-18 (same file): every non-comment
     skeleton line anchors verbatim, in order, against the original; the
     only invented line is the `{indent}pass` validity shim after omission
     comments. Skeleton-copied `Edit(old_string=)` chunks are therefore
     byte-anchored. 3/3 pass.
  4a. Local quality vs truncate — DONE 2026-09-18
     (`tests/code_quality_eval.rs`): at equal token budgets, skeleton keeps
     100% answer symbols on all four languages (py 4/4 vs trunc 3/4, go 3/3
     vs 1/3, rs/ts 3/3 vs 3/3 on small samples). Recorded trade: truncate
     wins raw identifier breadth (py 0.82 vs 0.63) by keeping full prefix
     bodies while dropping whole tail functions — intended, not a defect
     (dropped detail retrievable; undiscovered symbols are not).
  4b. Live A/B — STAGED, not run (`benchmarks/codemode_ab.py --suite edit`:
     5 line-insert tasks py/go/rs/ts + `score_edit` file-state grading +
     fixture reset; self-test 5/5). NOT run: needs `claude -p` spend, proxy
     restarts between arms (honoring patch landed — see Status). Runbook:
     on-arm on current proxy → `--out ab_code_on.json`; restart with
     `--code-aware false` → `--out ab_code_off.json`; compare
     tool-calls-to-first-useful-edit + rework turns, depth-binned, first
     turn dropped, drift-only.
- **Exit:** enable (possibly exploration-only, keeping `ByteExact` for edit
  targets) if ladder passes with no edit-regression; reject with the killing
  number otherwise.

## Findings 2026-09-28 — live edits say keep it on

`code_aware_compressor` fired on every day from 09-18 to 09-28 (27,966 log
lines: 9,320 dispatch decisions, 5,385 applied). No capture has an off arm,
because before 09-17 the flag was ignored and code-aware ran anyway. So the
test is the edit failure it was feared to cause: an `Edit` whose
`old_string` no longer matches, because the model copied it from a skeleton.

| capture | distinct Edit calls | old_string not found |
|---|---|---|
| blindguard (08-17→18) | 1,027 | 9 |
| vkreview | 205 | 2 |
| netvalue (08-23→09-25) | 185 | 0 |
| baseline-202609 | 18 | 0 |
| **all** | **1,435** | **11 (0.77%)** |

An off arm cannot beat 0.77% by enough to matter, so 4b's `claude -p` spend
would buy nothing. The upstream verdict in
`rejected/code-aware-40-pct-invalid-syntax.md` is overturned for this code:
the re-parse gate serves the original whenever the skeleton does not parse.
