# meter-fit

Fits Anthropic's subscription meter (`unified_utilization_sample`, 1% steps)
against per-turn token classes from `turn_cost_ledger`. Findings:
`docs/notes/learnings/subscription-meter-weights.md`.

Run from a scratch directory (the scripts read and write `data.jsonl` in the
current directory) with numpy and scipy available:

```bash
python3 extract.py data.jsonl ~/headroom-proxy.log.4 ... ~/headroom-proxy.log   # oldest first
python3 fit.py 5h      # hypotheses vs a free non-negative fit
python3 stability.py   # held-out windows and bootstrap spread
python3 ttl.py         # forced 1h TTL vs 5m, in meter %
```

`ttl.py` hard-codes the weights from one `stability.py` run; update them when
you refit.
