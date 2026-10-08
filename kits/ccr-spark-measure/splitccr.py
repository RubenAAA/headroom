import sys, json
sets={'up':set(),'use':set(),'mark':set()}
for line in sys.stdin:
    line=line.replace('\x00','').strip()
    if not line: continue
    try: d=json.loads(line)
    except: continue
    f=d.get('fields',{})
    r=f.get('request_id')
    if not r: continue
    if f['event']=='ccr_continuation_upstream': sets['up'].add(r)
    elif f['event']=='ccr_continuation_usage': sets['use'].add(r)
    elif f['event']=='ccr_continuation_markers': sets['mark'].add(r)
print('up:',len(sets['up']),'use:',len(sets['use']),'mark:',len(sets['mark']))
print('up&use:',len(sets['up']&sets['use']),'up&mark:',len(sets['up']&sets['mark']),'use&mark:',len(sets['use']&sets['mark']))
