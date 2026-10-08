import sys, json
from statistics import median
CUT='2026-10-02T12:37:53Z'
markers=set(); skipped=set(); rounduse=[]
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
    elif ev=='ccr_round_usage':
        rounduse.append((d['timestamp'],f))
print('round_usage events:',len(rounduse),'| markers-reqs:',len(markers),'| skipped-reqs:',len(skipped))
groups={}
for ts,f in rounduse:
    r=f.get('request_id')
    kind='ccr-path' if r in markers else ('server-tool' if r in skipped else 'memory-only')
    key=(kind,'post' if ts>=CUT else 'pre',f.get('kind'),f.get('has_usage'))
    groups.setdefault(key,[]).append((f.get('cache_creation_input_tokens'),f.get('cache_read_input_tokens'),f.get('round')))
for k in sorted(groups):
    ws=[w for w,r_,rnd in groups[k] if isinstance(w,int) and w>=0]
    rs=[r_ for w,r_,rnd in groups[k] if isinstance(r_,int) and r_>=0]
    if not ws: print(k,'n=',len(groups[k]),'NO-COUNTS'); continue
    ws.sort(); rs.sort()
    print(k,'n=',len(groups[k]),'| write p50',median(ws),'mean',sum(ws)//len(ws),'max',ws[-1],'| read p50',median(rs))
