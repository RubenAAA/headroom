import sys, json
out = open('/tmp/ccr-spark-measure/aftershocks.jsonl','w')
n=0
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{})
    if f.get('event')!='cache_recache_observed': continue
    if f.get('attribution_reason')!='aftershock_of_continuation': continue
    r={'ts':d.get('timestamp'),'request_id':f.get('request_id'),'conversation_key':f.get('conversation_key'),
       'wasted_tokens':f.get('wasted_tokens'),'origin':f.get('origin'),'scope':f.get('scope'),
       'expected_cache_read':f.get('expected_cache_read'),'actual_cache_read':f.get('actual_cache_read'),
       'idle_seconds':f.get('idle_seconds'),'forward_model':f.get('forward_model')}
    out.write(json.dumps(r)+'\n'); n+=1
out.close()
print('aftershock events:',n)
