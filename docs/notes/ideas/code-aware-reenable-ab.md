# Idea: re-enable code-aware compression and measure

- **Status:** open, ladder partially run 2026-09-18 (rungs 1–4a green; 4b
  staged, live runs pending). **Honoring patch LANDED 2026-09-18
  (uncommitted):** `CODE_AWARE_ENABLED` static + `set_code_aware_enabled`
  (`live_zone.rs`, mirroring `KOMPRESS_ENABLED`), SourceCode-arm gate,
  startup wiring from `--code-aware` (`main.rs`), `flags.sh` flipped to
  `--code-aware true` (pins measured behavior; upstream default stays
  false). Off-arm proven by `tests/code_aware_off_arm.rs` (own process —
  the gate is process-wide). Drive-by fix in the same patch: the flag joins
  the dispatch-memo key, else a runtime flip serves 30-min-stale results.
  Full core suite green (2,188 lib pass), proxy compression tests green
  (217 pass), fmt clean, clippy shows only 3 pre-existing warnings in
  untouched files. LIVE since 2026-09-17T23:15:39Z restart (PID 16674, new
  binary, `--code-aware true` in cmdline) — gate wired, behavior pinned on.
  Open confirmation (2026-09-18): only ~1 min of traffic at the restart
  check, zero `code_aware_compressor` hits yet — spot-check the next hour's
  log for `live_zone_strategies: ["code_aware_compressor"]` to close the
  loop, then delete this line.
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
