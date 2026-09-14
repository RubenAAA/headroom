# Learning: platform axes are orthogonal (OS vs host)

- **Source:** `docs/context-mode-integration-analysis.md` §8
- **Claim:** Headroom tracks OS coverage (`platform-feature-matrix.json`);
  context-mode tracks agent-host coverage (18 hosts); Headroom tracks no host
  matrix at all. P2-style work needs a second matrix, not new rows. Companion
  gaps from the same pass: no plugin-authoring docs (first plugin sets house
  style — write it as part of P1), no committed benchmark results (run the
  harness to prove the zero-bust claim empirically).


## Follow-up gaps

*moved from `docs/context-mode-integration-analysis.md`*

**No plugin-authoring docs exist.** `docs/` is a Next.js site (`app/`, `content/`, `components/`);
`wiki/` has nothing on extension authoring (only `macos-deployment.md` matched). `plugins/headroom-oauth2/SPEC.md`
remains the de-facto authoring reference — which means whichever plugin lands first sets the house
style. Worth writing the authoring doc as part of P1.

**Headroom publishes no benchmark results.** `benchmarks/` is 29 runner scripts with no committed
results artifacts, so no like-for-like number exists to compare against context-mode's 96%. The
comparison has to be run. The harness is there and is unusually strong on exactly the axis that
matters: `prefix_cache_benchmark.py`, `cache_bust_trace_report.py`, `cache_validation_bundle.py`,
`synthetic_token_cache_bust_report.py`, `proxy_mode_benchmark.py`, `agent_cost_benchmark.py`,
`real_world_agent_benchmark.py`. Use it to *prove* the §1 cache-safety claim empirically rather than
asserting it — a measured "zero cache-bust events" result is the strongest possible artifact for the
No-Proxy Edition.

**Bonus finding — the platform axes are orthogonal.**
`docs/platform-feature-matrix.json` (schema v1, updated 2026-07-06) tracks coverage across
`["linux", "macos", "windows"]` — Headroom's platform axis is **operating system**. context-mode's
platform axis is **agent host** (18 of them). Headroom tracks no host-coverage matrix at all. P2
therefore fills a dimension that does not currently exist in Headroom's own feature accounting,
which also means it needs a second matrix rather than new rows in this one.

*Process note:* six subagents were dispatched across this analysis and all six stalled at the
600-second watchdog; one reported "Bash is temporarily unavailable" before dying, so the failures
were tool-layer, not analytical. Every finding in this document was verified directly.
