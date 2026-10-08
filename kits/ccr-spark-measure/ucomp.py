import sys, json
from statistics import median
CUT='2026-10-02T12:37:53Z'
markers=set(); skipped=set(); usages=[]
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{}); ev=f.get('event')
    if ev in ('ccr_continuation_markers','ccr_continuation_upstream'):
        if f.get('request_id'): markers.add(f['request_id'])
    elif ev=='usage_baseline_skipped':
        if f.get('request_id'): skipped.add(f['request_id'])
    elif ev=='ccr_continuation_usage':
        usages.append((d['timestamp'],f))
print('usage events:',len(usages))
groups={}
for ts,f in usages:
    r=f.get('request_id')
    kind='ccr-path' if r in markers else ('server-tool' if r in skipped else 'memory-only')
    key=(kind,'post' if ts>=CUT else 'pre')
    groups.setdefault(key,[]).append(f)
for k in sorted(groups):
    ws=[f['cache_write_tokens'] for f in groups[k]]; rs=[f['rounds'] for f in groups[k]]
    wpr=[w/max(r,1) for w,r in zip(ws,rs)]
    print(k,'n=',len(ws),'| write/turn p50',median(ws),'mean',sum(ws)//len(ws),'| rounds p50',median(rs),'| write/round p50',median(wpr),'mean',sum(wpr)//len(wpr))
