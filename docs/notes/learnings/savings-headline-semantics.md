# Learning: savings headline semantics (per-request, selected tokens)

- **Source:** `docs/notes/proxy-experiments-closures.md` (items 1/2/15)
- **Claim:** a correct per-request saving re-applied to re-sent history *should*
  repeat (36 turns, identical 56,369 tokens — the client re-sends the same
  blocks). Summing it across turns double-counts by construction. Headline
  `headroom savings` = transform efficiency on successful compression events
  (denominator: pre-compression input selected by those transforms — not the
  whole prompt); net proxy math lives in `/stats.savings_verdict` (minus busts)
  and `/stats.wire_verdict` (whole-request view).
- **Evidence:** 1,314,871 − 380,254 = 934,617 exact across 335 joined turns;
  ledger rows 1:1 with PERF.
