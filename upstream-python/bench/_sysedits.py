"""Each system-block edit, its cause, and whether a proxy restart explains it."""
import collections, difflib, sys
from cachesim import load_corpus, load_ledger, ledger_usage

corpus, log = sys.argv[1], sys.argv[2]
turns = load_corpus(corpus)
ledger = load_ledger(log)

def blocks(body):
    s = body.get("system")
    return [b.get("text","") for b in s] if isinstance(s, list) else []

scopes = collections.defaultdict(list)
for t in turns:
    scopes[(t.session_key, t.body.get("model"))].append(t)

for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    for prev, cur in zip(ts, ts[1:]):
        a, b = blocks(prev.body), blocks(cur.body)
        if a == b:
            continue
        real = ledger.get(cur.request_id)
        create = ledger_usage(real).create_tokens if real else -1
        print(f"ts {cur.ts:.0f}  gap {cur.ts-prev.ts:.0f}s  create {create:,}")
        for i in range(min(len(a), len(b))):
            if a[i] == b[i]:
                continue
            sm = difflib.SequenceMatcher(None, a[i], b[i], autojunk=False)
            for tag, i1, i2, j1, j2 in sm.get_opcodes():
                if tag != "equal":
                    print(f"   block {i} {tag}: {a[i][i1:i2]!r} -> {b[i][j1:j2]!r}")
