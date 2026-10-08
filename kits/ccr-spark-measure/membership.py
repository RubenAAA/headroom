import sys, json
ccr={}  # req -> list of (ts, ev, extra)
ledgers={}
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{}); ev=f.get('event')
    if ev in ('ccr_continuation_upstream','ccr_continuation_usage','ccr_continuation_markers'):
        r=f.get('request_id')
        if r: ccr.setdefault(r,[]).append((d['timestamp'],ev,f.get('provider',''),f.get('round',''),f.get('rounds','')))
    elif ev=='turn_cost_ledger':
        ledgers.setdefault(f.get('conversation_key'),[]).append((d['timestamp'],f.get('request_id')))
for v in ledgers.values(): v.sort()
ash=[json.loads(l) for l in open('/tmp/ccr-spark-measure/aftershocks.jsonl')]
ash.sort(key=lambda r:r['ts'])
for a in ash:
    if a['ts']<'2026-09-30': continue
    L=ledgers.get(a['conversation_key'],[])
    prior=[x for x in L if x[0]<a['ts']]
    pts,preq=prior[-2]
    evs=ccr.get(preq,[])
    print(a['ts'][:16],'wasted',a['wasted_tokens'],'prev',pts[11:19],preq[:8])
    for e in evs: print('   ',e[0][11:19],e[1],e[2],'round' if e[1]!='ccr_continuation_usage' else 'rounds', e[3] if e[1]!='ccr_continuation_usage' else e[4])
    if not evs: print('    NO-CCR-EVENTS')
