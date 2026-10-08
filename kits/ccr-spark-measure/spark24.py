import sys, json
from statistics import median
MX='2026-10-08T11:34:48Z'; LO='2026-10-07T11:34:48Z'
CUT='2026-10-02T12:37:53Z'
rows=[]
for line in sys.stdin:
    line=line.replace('\x00','')
    try: d=json.loads(line)
    except: continue
    ts=d['timestamp']
    if not (LO<=ts<=MX): continue
    m=d['fields'].get('message','')
    f=dict(w.split('=',1) for w in m.split()[2:] if '=' in w)
    if 'spark' not in f.get('model',''): continue
    try:
        rows.append((int(f['tok_after']),float(f['opt_ms']),float(f['ttfb_ms'])))
    except (KeyError,ValueError): continue
print('spark turns in window:',len(rows))
def pct(v,p):
    v=sorted(v); return v[min(len(v)-1,int(p*len(v)))] if v else None
all_share=[]
for name,lo,hi in [('under 50k',0,50000),('50-200k',50000,200000),('over 200k',200000,10**18)]:
    b=[r for r in rows if lo<=r[0]<hi]
    if not b: print(name,'n=0'); continue
    opt=[r[1] for r in b]; ttfb=[r[2] for r in b if r[2]>0]
    sh=[o/t for o,t in zip(opt,[r[2] for r in b]) if t>0]; all_share+=sh
    print(f'{name} | n={len(b)} | opt p50={median(opt):.0f} p90={pct(opt,0.9):.0f} max={max(opt):.0f} | ttfb p50={median(ttfb):.0f} | share p50={median(sh)*100:.1f}%')
print('overall share p50:', f'{median(all_share)*100:.1f}%' if all_share else None)
