# To check — created 2026-09-11

Dated follow-ups from today's work. Check off with evidence, not memory.

## ~2026-09-12: Plan 2 gate read (parse/rewrite/post)

Instrumented release deployed 2026-09-11 ~14:48 UTC. After one day of log,
apply the ≥20 ms p50 gate per gap — pass opens that slice of the step-2
rewrite, fail retires it:

```bash
rg '"event":"stage_timings"' ~/headroom-proxy.log --no-filename > /tmp/stages.txt
python3 -c "
import json, statistics as st
gaps = {'parse':[], 'rewrite':[], 'post':[]}
for line in open('/tmp/stages.txt'):
    s = json.loads(line)['fields']['stages']; s = json.loads(s) if isinstance(s,str) else s
    for k in gaps:
        if s.get(k) is not None: gaps[k].append(s[k])
for k,v in gaps.items():
    print(k, 'n:', len(v), 'p50:', round(st.median(v),1) if v else None)
"
```

Note: `parse` contains `memory` — subtract it before judging the parse gap.

## Next deploy: shutdown drain stdout

Proxy side already passes (3× `shutdown_drained` 609–2031 ms, 1× bounded
30001 ms timeout). Still unchecked: the deploy script must not print
"still up after 40s, forcing". Look at deploy output, not the log.

## If load returns: memory re-check

2026-09-11 (n=869): `memory` p50 0.11 ms — Plan 1 stays OFF. Revisit only if
`memory` p50 matters again; then re-run the inversion table, not the stale
§0 one (see `memory-search-wide-pass-lever.md` §0c).

## Open policy calls (no code needed)

- CCR retention: 12,496 rows, 0.76% ever read, retrieval verified working —
  shorten TTL / offload less, or accept the waste deliberately.
- Savings slice #3 (tool-schema aggregation): no Rust producer exists and it
  conflicts with keep-layers-separate — likely reject; slice #2 (per-bucket
  output rollup) is dashboard-only value.
- Prior-thinking billing join after 2–3 days of `OutputSplit` data: decides
  the wider thinking-drop (4.6+ models only).
- Content-router port: only on a failing fixture, live divergence bug, or a
  decision to serve through `apply`.
