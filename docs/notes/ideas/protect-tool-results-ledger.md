# Idea: price per-tool lossy protection (`--protect-tool-results`)

- **Status:** open (live unset — deliberately implicit in flags.sh:727; force-
  merges into the exclude set and survives `--exclude-tools ""`,
  `config.rs:2856-2862,3491-3500`)
- **Source:** 2026-09-18 session; `config.rs:1781-1785`
- **Value:** the knob for "model acts on summaries of X and gets it wrong":
  naming a tool here exempts its results from lossy compression (verbatim/
  byte-exact members untouched, others get the reversible lossless fold
  only). Direction is conservative (costs savings, buys fidelity), so this
  file is the method for spending that budget deliberately instead of by
  anecdote.
- **Next:** when an edit/wrong-action regression traces to a lossy tool
  result, add ONLY that tool, then track per-tool savings cost (strategy
  ledger before/after on the same workload) against regression recurrence.
  Never bulk-add; each entry needs its incident. Current default
  `--exclude-tools` already covers Read/Glob/Grep/Write/Edit/WebSearch/
  WebFetch/view/read_file/Skill/headroom_retrieve — additions must justify
  themselves against that baseline.
- **Exit:** per-tool keep/remove with its own numbers; this file stays open
  as the ledger.
