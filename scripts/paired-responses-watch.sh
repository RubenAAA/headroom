#!/bin/bash
# Emit paired-Responses-turn count every 5 min; exit when it reaches 1000.
while true; do
  n=$(python3 -c "
import json,glob,os
D=os.path.expanduser('~/headroom-capture-netvalue/')
try:
    outs=set(os.listdir(D+'out/'))
except OSError:
    outs=set()
n=0
for f in glob.glob(D+'req-*.json'):
    try:
        e=json.load(open(f))
    except Exception:
        continue
    if e.get('endpoint')=='openai_responses' and e.get('request_id','')+'.json' in outs:
        n+=1
print(n)")
  echo "paired_responses_turns=$n"
  [ "$n" -ge 1000 ] && break
  sleep 300
done
