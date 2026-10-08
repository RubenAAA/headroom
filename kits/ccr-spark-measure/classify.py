import sys, json
from datetime import datetime
ledgers={}  # conv -> list of (ts, req)
ccr_reqs=set()
mem_calls=[]  # (ts, tool)
def ts(s): return datetime.fromisoformat(s.replace('Z','+00:00'))
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{}); ev=f.get('event')
    if ev=='turn_cost_ledger':
        ledgers.setdefault(f.get('conversation_key'),[]).append((d['timestamp'],f.get('request_id')))
    elif ev in ('ccr_continuation_upstream','ccr_continuation_usage','ccr_continuation_markers'):
        if f.get('request_id'): ccr_reqs.add(f['request_id'])
    elif ev=='memory_tool_call':
        mem_calls.append((d['timestamp'],f.get('tool')))
for v in ledgers.values(): v.sort()
mem_calls.sort()
print('convs:',len(ledgers),'ccr_reqs:',len(ccr_reqs),'mem_calls:',len(mem_calls), file=sys.stderr)
ash=[json.loads(l) for l in open('/tmp/ccr-spark-measure/aftershocks.jsonl')]
ash.sort(key=lambda r:r['ts'])
print('ts wasted conv prev_req ccr mem_kind')
import bisect
mts=[m[0] for m in mem_calls]
for a in ash:
    L=ledgers.get(a['conversation_key'],[])
    prior=[x for x in L if x[0]<a['ts']]
    if not prior:
        print(a['ts'],a['wasted_tokens'],a['conversation_key'][:8],'NO-PREV ? ?'); continue
    prev=prior[-1]; pp=prior[-2][0] if len(prior)>1 else '2000-01-01T00:00:00Z'
    ccr = 'CCR' if prev[1] in ccr_reqs else '-'
    lo=bisect.bisect_right(mts,pp); hi=bisect.bisect_right(mts,prev[0])
    inwin=mem_calls[lo:hi]
    mem = ('MEM(%d)'%len(inwin)) if inwin else '-'
    print(a['ts'],a['wasted_tokens'],a['conversation_key'][:8],(prev[1] or '?')[:8],ccr,mem)
