"""What does a system-block edit actually cost, from the bill?

The simulator says holding the block still is worth ~2.4%. It also said the
split TTL was worth -38% and that cost +511%, so this asks the ledger instead:
on turns where the inbound `system` array differs from the previous turn of the
same conversation, what did the provider charge for creation, against turns of
the same depth where it did not?
"""
import collections
import sys

from cachesim import load_corpus, load_ledger, ledger_usage

corpus, log = sys.argv[1], sys.argv[2]
turns = load_corpus(corpus)
ledger = load_ledger(log)


def sysblocks(body):
    s = body.get("system")
    if isinstance(s, str):
        return [s]
    return [b.get("text", "") if isinstance(b, dict) else str(b) for b in (s or [])]


scopes = collections.defaultdict(list)
for t in turns:
    scopes[(t.session_key, t.body.get("model"))].append(t)

BINS = [(0, 20), (20, 50), (50, 100), (100, 200), (200, 400), (400, 10**9)]


def binof(n):
    return next((b for b in BINS if b[0] <= n < b[1]), BINS[-1])


rows = []          # (changed, depth, create)
edits = collections.Counter()
for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    for prev, cur in zip(ts, ts[1:]):
        real = ledger.get(cur.request_id)
        if not real:
            continue
        a, b = sysblocks(prev.body), sysblocks(cur.body)
        changed = a != b
        if changed:
            for i in range(min(len(a), len(b))):
                if a[i] != b[i]:
                    edits[i] += 1
        rows.append((changed, len(cur.body.get("messages") or []),
                     ledger_usage(real).create_tokens))

n_ch = sum(1 for c, _, _ in rows if c)
print(f"{len(rows)} ledger-joined transitions, {n_ch} with an edited system "
      f"block ({n_ch/max(len(rows),1):.1%}); by block index {dict(edits)}\n")

print(f"{'depth (msgs)':>14}{'unchanged n':>13}{'create/turn':>13}"
      f"{'changed n':>11}{'create/turn':>13}{'extra':>12}")
tot = paired = 0
for lo, hi in BINS:
    same = [c for ch, n, c in rows if not ch and lo <= n < hi]
    diff = [c for ch, n, c in rows if ch and lo <= n < hi]
    ms = sum(same) / len(same) if same else 0
    md = sum(diff) / len(diff) if diff else 0
    extra = f"{md - ms:>+12,.0f}" if same and diff else " " * 12
    label = f"{lo}-{hi}" if hi < 10**9 else f"{lo}+"
    print(f"{label:>14}{len(same):>13}{ms:>13,.0f}{len(diff):>11}{md:>13,.0f}{extra}")
    if same and diff:
        tot += (md - ms) * len(diff)
        paired += len(diff)

if paired:
    total_create = sum(c for _, _, c in rows)
    print(f"\n{tot:,.0f} extra creation over {paired} edited turns "
          f"= {tot/max(total_create,1):.1%} of all creation in the corpus")
