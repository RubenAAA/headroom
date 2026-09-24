# Idea: per-section cost share baseline (§1)

- **Status:** open — precondition for every other harness-derived idea
- **Source:** `LOOK_AT_THIS_WHEN_YOU_HAVE_TIME.md` §1 + §8. Proxy counterparts: `docs/measurement.md` (savings_verdict vs wire_verdict; billed = creation + uncached input, reads free on subscription), `cache_stabilization/usage_observer.rs:1-51` (turn classifier, conversation-keyed, observer-only).
- **Gap, verified:** totals exist (saved vs lost-to-busts, billed tokens, hit %) but no source split (system/tools/reads/search/commands/history). Ledger rows carry `cost_basis` (fresh/read/free) without source, so "share × removable ÷ risk" ranking is guesswork.
- **Value:** stops work on a 2% leg. Denominator for §8 validation (cost per completed task, turns/task, errors, hit rate).
- **Next (no request-path change):** offline script over 200–500 captured/replay bodies, split by section, counted from provider usage fields by billing type. Emit the three §1 tables.
- **Method constraints (from existing learnings — violations caused wrong findings before):**
  - `recache-counting-rules.md`: scope by date AND process start (log never rotates; JSON-parse, don't regex); drop each conversation's first turn for cost comparison (first turns are 42% of writes per `first-turn-write-share.md`); count only `drift` as waste.
  - `input-tokens-uncached-tail.md`: size from `bytes_out` / cache counters, never `input_tokens` (warm-turn `input_tokens=2` on 200KB+ bodies).
  - `savings-headline-semantics.md`: per-request savings re-applied to re-sent history repeat by construction — sum across turns double-counts. Headline stays transform-efficiency; net math stays in `savings_verdict`/`wire_verdict`.
  - `unbooked-share-of-wire.md`: unbooked are 17% of wire bytes — booked-only ratios cover 83%, say so.
- **Neighbours (complementary, not duplicates):** `history-rollup-read-cost.md` (read-cost leg on steep-discount models) — this is the section leg. `recache-landing-natural-kinds.md` + `recache-blind-spot-watch.md` supply the waste ontology/witnesses this table will join to. `cost-saves-measured.md` sets the prior: writes 55% of bill, compression capped ~1.5% — expect the table to confirm it, not overturn it.
- **Exit:** close when the table exists and one ranked proposal cites it.
- **Tool:** `crates/headroom-proxy/src/bin/section_cost_baseline.rs` (offline; capture dir in, three tables out; groups by session+model, stable run = read at pricing ratios, first turns reported separately, `--write-tier 5m|1h`).

## Findings 2026-09-24 — two August windows (`section_cost_baseline`, write tier 1h)

| leg (billed-equiv share) | netvalue 08-23→24 (2,648 turns / 46 sess) | blindguard 08-17→18 (7,839 turns / 247 sess) |
|---|---|---|
| result:file | 10.6% | **28.7%** |
| result:command | **29.0%** | 8.3% |
| msg:thinking | 12.2% | 15.0% |
| tools:builtin | 10.5% | 7.2% |
| msg:user_text | 18.1% | 21.2% |
| tools:mcp | 3.2% | 3.8% |
| msg:tool_use | 10.7% | 9.2% |
| system | 1.8% | 2.4% |

Both: static ~19–21k tokens/req, stable-prefix share ~96.5% (excl. first turns), first turns ~24% of write tokens. Netvalue lived in Bash (2,614/2,648 turns); blindguard lived in Read (6,810/7,839).

- **Mix is workload-dependent: no single biggest leg.** File/command flip between windows. Stable across both: tools ~10–11% combined, system ~2%. Prefix discipline is already good — wins are in what the prefix carries, not its stability.
- **Thinking shares above are overstated:** `rejected/prior-thinking-billing-question.md` (67/67 invariant) says prior thinking moves billed reads ~0. Wire tokens ≠ billed cost for that leg; discount it to ~zero when ranking.
- **Error-rate outliers (taxonomy seeds):** blindguard WebSearch 37.4%, `search_code` 23.9%, TaskOutput 13.3%, TaskStop 12.5% vs Read 0.4%, Write 0.0%. Netvalue Agent 21.6%, `search_code` 33.7%.
- **Method limits:** simulated prefix economics, not `usage` truth (no ledger join — follow-up); August windows, client has moved since; fresh capture `~/headroom-capture-baseline-202609` armed 2026-09-24 to re-run on current traffic.
