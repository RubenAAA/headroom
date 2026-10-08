import sys, json, bisect
ledgers={}
ccr_reqs=set()
mem_calls=[]
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{}); ev=f.get('event')
    if ev=='turn_cost_ledger':
        ledgers.setdefault(f.get('conversation_key'),[]).append((d['timestamp'],f))
    elif ev in ('ccr_continuation_upstream','ccr_continuation_usage','ccr_continuation_markers'):
        if f.get('request_id'): ccr_reqs.add(f['request_id'])
    elif ev=='memory_tool_call':
        mem_calls.append((d['timestamp'],f.get('tool')))
for v in ledgers.values(): v.sort(key=lambda x:x[0])
mem_calls.sort()
mts=[m[0] for m in mem_calls]
ash=[json.loads(l) for l in open('/tmp/ccr-spark-measure/aftershocks.jsonl')]
ash.sort(key=lambda r:r['ts'])
print('ts wasted prev_ts prev_req ccr mem rounds_in rounds_read')
for a in ash:
    L=ledgers.get(a['conversation_key'],[])
    prior=[x for x in L if x[0]<a['ts']]
    if len(prior)<2:
        print(a['ts'],a['wasted_tokens'],'ONLY-SELF-OR-NONE'); continue
    pts,pf=prior[-2]
    pp=prior[-3][0] if len(prior)>2 else '2000-01-01T00:00:00Z'
    ccr='CCR' if (pf.get('request_id') in ccr_reqs) else '-'
    lo=bisect.bisect_right(mts,pp); hi=bisect.bisect_right(mts,pts)
    inwin=mem_calls[lo:hi]
    mem=('MEM(%d)'%len(inwin)) if inwin else '-'
    print(a['ts'],a['wasted_tokens'],pts,(pf.get('request_id') or '?')[:8],ccr,mem,
          pf.get('rounds_input_tokens'),pf.get('rounds_cache_read_tokens'))
