"""What would E1/E2/E3 change on captured client bodies? Prints counts only.

Usage: python3 scripts/e-pass-replay.py ~/headroom-capture-*
See docs/notes/ideas/rejected/enable-e1-e2-sort-ab.md.
"""
import json, os, sys, collections

def ser(x):
    return json.dumps(x, separators=(',', ':'), ensure_ascii=False)

def has_marker(b):
    def walk(x):
        if isinstance(x, dict):
            if 'cache_control' in x:
                return True
            return any(walk(v) for v in x.values())
        if isinstance(x, list):
            return any(walk(v) for v in x)
        return False
    return walk(b.get('tools')) or walk(b.get('system')) or walk(b.get('messages'))

c = collections.Counter()
prev = {}
for d in sys.argv[1:]:
    for f in sorted(os.listdir(d)):
        if not f.startswith('req-') or not f.endswith('.json'):
            continue
        try:
            r = json.load(open(os.path.join(d, f)))
        except Exception:
            c['unreadable'] += 1
            continue
        b = r.get('body')
        if isinstance(b, str):
            try:
                b = json.loads(b)
            except Exception:
                continue
        if not isinstance(b, dict) or 'messages' not in b:
            c['not_messages'] += 1
            continue
        kind = 'spark' if 'spark' in str(b.get('model', '')) else 'claude'
        c[kind + ':bodies'] += 1
        tools = b.get('tools') or []
        names = [t.get('name') for t in tools]
        if not has_marker(b):
            c[kind + ':no_marker(E3 would act)'] += 1
        if names != sorted(names, key=lambda n: n or ''):
            c[kind + ':tools_unsorted(E1 would reorder)'] += 1
        unsorted_schema = sum(1 for t in tools if ser(t) != json.dumps(t, separators=(',', ':'), ensure_ascii=False, sort_keys=True))
        if unsorted_schema:
            c[kind + ':schema_keys_unsorted(E2 would reorder)'] += 1
        sk = (r.get('session_key'), kind)
        p = prev.get(sk)
        if p is not None:
            c[kind + ':pairs'] += 1
            pn, pt = p
            if names != pn:
                c[kind + ':pair_tool_list_changed'] += 1
                if sorted(map(str, names)) == sorted(map(str, pn)):
                    c[kind + ':pair_same_set_new_order'] += 1
                elif names[:len(pn)] != pn:
                    c[kind + ':pair_change_not_append_only'] += 1
            tser = {t.get('name'): ser(t) for t in tools}
            same_def_diff_bytes = sum(1 for n, s in tser.items() if n in pt and pt[n] != s
                                      and json.loads(pt[n]) == json.loads(s))
            if same_def_diff_bytes:
                c[kind + ':pair_same_schema_new_key_order'] += 1
        prev[sk] = (names, {t.get('name'): ser(t) for t in tools})

for k in sorted(c):
    print(f'{c[k]:>7}  {k}')
