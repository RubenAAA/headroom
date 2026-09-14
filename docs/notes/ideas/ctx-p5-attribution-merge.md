# Idea: P5 headroom-attribution — merge counterfactual methodologies

- **Status:** open (opportunistic; merge, don't port)
- **Source:** `docs/context-mode-integration-analysis.md` §4 P5 + §8
- **Summary:** port the *methodology* (`RealBytesStats`, per-project
  attribution, multi-host coverage) into `savings_ledger`/`reporting` — but
  `audit/reads.py` already measures counterfactuals over the same corpus with
  the better mechanism taxonomy. Combine the two, don't add a third
  implementation. Do NOT port `pricing.ts` (overlaps `headroom/pricing/*`).
  Low-medium effort.


## Proposal

*moved from `docs/context-mode-integration-analysis.md`*

### P5 — `headroom-attribution`: counterfactual savings + per-project cost

**What:** port the *methodology* from `session/analytics.ts` — `RealBytesStats`,
`ThinkInCodeComparison`, `enumerateAdapterDirs`, `project-attribution.ts` — into Headroom's
`savings_ledger` / `reporting` / `dashboard`.

**Why:** Headroom measures compression deltas (what it squeezed). context-mode measures the
counterfactual (what never entered). Enterprise buyers want the second number, sliced by team and
repo. Do **not** port `pricing.ts` — `headroom/pricing/*` already does this with litellm resolution.

**Merge, don't port.** `headroom/audit/reads.py` is already a counterfactual measurement tool over
the same Claude Code transcript corpus (see §8). It has the better mechanism taxonomy — identical
repeat, subset containment, write-readback, stale, line-number scaffolding, context residency,
cache-death windows. `analytics.ts` has the multi-host coverage and per-project attribution it
lacks. Combine the two rather than adding a third implementation.

**Effort:** low-medium, mostly a metrics-definition merge.


## Overlap note

*moved from `docs/context-mode-integration-analysis.md`*

It *does* overlap **P5** — `audit/reads.py` and context-mode's `session/analytics.ts` are two
independent implementations of counterfactual measurement over the same transcript corpus. Merge
them rather than porting; `audit/reads.py` has the better mechanism taxonomy, `analytics.ts` has
multi-host coverage and per-project attribution.
