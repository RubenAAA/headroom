# Idea: P4 headroom-sandbox — Think-in-Code execution

- **Status:** open (gated: with P3, never before — the escape scanner is what
  stops the sandbox being an escape hatch)
- **Source:** `docs/context-mode-integration-analysis.md` §4 P4
- **Summary:** `executor.ts` as MCP tool `headroom_execute` (12 languages,
  stdout-only). Behind context-mode's largest measured savings (98% over
  315 KB fixtures vs 82% for index+search): programming the analysis beats
  compressing the output. Medium-high; runtime isolation is the hard part
  (`sandbox` extra in `pyproject.toml` exists).


## Proposal

*moved from `docs/context-mode-integration-analysis.md`*

### P4 — `headroom-sandbox`: Think-in-Code execution

**What:** `executor.ts` exposed as a Headroom MCP tool (`headroom_execute`), 12 languages,
stdout-only.

**Why:** this is the mechanism behind context-mode's largest measured savings —
`ctx_execute_file` returns 98% savings across 315 KB of real fixtures (`BENCHMARK.md` Part 1),
versus 82% for index+search (Part 2). Programming the analysis beats compressing the output.

Must ship *with* P3: the shell-escape scanner is what stops the sandbox being an escape hatch.

**Effort:** medium-high. Runtime isolation is the hard part; `headroom` already has a `sandbox` extra
in `pyproject.toml` to build on.
