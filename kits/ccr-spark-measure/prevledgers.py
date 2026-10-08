import sys, json
ledgers={}
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{})
    ledgers.setdefault(f.get('conversation_key'),[]).append((d['timestamp'],f))
for v in ledgers.values(): v.sort(key=lambda x:x[0])
ash=[json.loads(l) for l in open('/tmp/ccr-spark-measure/aftershocks.jsonl')]
ash.sort(key=lambda r:r['ts'])
for a in ash:
    L=ledgers.get(a['conversation_key'],[])
    prior=[x for x in L if x[0]<a['ts']]
    if not prior: print(a['ts'],'NO-PREV'); continue
    ts,f=prior[-1]
    print(a['ts'],'| ash_wasted',a['wasted_tokens'],'| prev_ts',ts,'| prev_req',(f.get('request_id') or '?')[:8],
          '| rounds_in',f.get('rounds_input_tokens'),'rounds_read',f.get('rounds_cache_read_tokens'),
          '| cli_read',f.get('cache_read_input_tokens'),'cli_write',f.get('cache_creation_input_tokens'))
