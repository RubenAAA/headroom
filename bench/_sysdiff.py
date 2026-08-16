"""Which part of the system block churns, and how big is the blast radius?

Every large re-creation in the client arm starts at a system block, so the
system array is the whole target. This reports which index changes, how much of
it is stable, and what a change costs — structure and sizes only, no content.
"""
import collections
import difflib
import sys

from cachesim import load_corpus, with_forwarded_bodies, DEFAULT_BYTES_PER_TOKEN as BPT

P = sys.argv[1]
raw = load_corpus(P)
fwd = {t.request_id: t for t in with_forwarded_bodies(raw, P)}
raw = [t for t in raw if t.request_id in fwd]


def sysblocks(body):
    system = body.get("system")
    if isinstance(system, str):
        return [system]
    return [b.get("text", "") if isinstance(b, dict) else str(b)
            for b in (system or [])]


scopes = collections.defaultdict(list)
for t in raw:
    scopes[(t.session_key, t.body.get("model"))].append(t)

changes = collections.Counter()
churn_tokens = collections.Counter()
shapes = collections.Counter()
examples = {}

for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    for a, b in zip(ts, ts[1:]):
        x, y = sysblocks(a.body), sysblocks(b.body)
        shapes[(len(x), len(y))] += 1
        for i in range(min(len(x), len(y))):
            if x[i] != y[i]:
                changes[i] += 1
                churn_tokens[i] += len(y[i]) / BPT
                if i not in examples:
                    sm = difflib.SequenceMatcher(None, x[i], y[i], autojunk=False)
                    same = sum(n for _, _, n in sm.get_matching_blocks())
                    examples[i] = (len(x[i]), len(y[i]), same)

print(f"system array lengths seen (prev -> next): "
      f"{dict(list(shapes.most_common(5)))}\n")
print(f"{'index':>7}{'changes':>9}{'tokens at risk':>16}{'prev len':>10}"
      f"{'next len':>10}{'chars in common':>17}")
for i, n in sorted(changes.items()):
    a, b, same = examples[i]
    print(f"{i:>7}{n:>9}{int(churn_tokens[i]):>16,}{a:>10,}{b:>10,}{same:>17,}")

if not changes:
    print("  no system block ever changed within a conversation")

sizes = [len(s) / BPT for _, ts in scopes.items()
         for s in sysblocks(ts[0].body)]
print(f"\nsystem array: {len(sizes)} blocks across first turns, "
      f"{int(sum(sizes)):,} tokens total")
