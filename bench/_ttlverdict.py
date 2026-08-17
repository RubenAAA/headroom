"""Is the split TTL better or worse in production, at equal conversation depth?

Creation per turn rises with depth, so a raw before/after mean says nothing: the
window after the switch is also the window with the deepest conversations. Bins
by inbound message count and reports actual creation per turn in each bin, so
the two configurations are compared like for like.

The first turn of each conversation is excluded. It has no prefix to read and
writes the lot, and counting it inverts the answer — the same trap that
`cache-measure-excludes-first-turns` records.
"""
import collections
import sys

from cachesim import load_corpus, load_ledger, ledger_usage

corpus, log = sys.argv[1], sys.argv[2]
turns = load_corpus(corpus)
ledger = load_ledger(log)

rows = []
for t in turns:
    real = ledger.get(t.request_id)
    if real:
        rows.append((t.ts, len(t.body.get("messages") or []), ledger_usage(real),
                     t.session_key))
rows.sort()

switch = next((ts for ts, _, u, _ in rows if u.write_5m_tokens > 0), None)
if switch is None:
    sys.exit("no turn in this corpus reports a 5-minute write")

# Drop each conversation's first observed turn.
first = {}
for ts, n, u, key in rows:
    first.setdefault(key, ts)
rows = [r for r in rows if r[0] != first[r[3]]]

BINS = [(0, 20), (20, 50), (50, 100), (100, 200), (200, 400), (400, 10**9)]


def label(lo, hi):
    return f"{lo}-{hi}" if hi < 10**9 else f"{lo}+"


groups = collections.defaultdict(lambda: collections.defaultdict(list))
for ts, n, u, _ in rows:
    for lo, hi in BINS:
        if lo <= n < hi:
            groups[(lo, hi)]["after" if ts >= switch else "before"].append(u)
            break

print(f"{'depth (msgs)':>14}{'before n':>10}{'create/turn':>13}"
      f"{'after n':>10}{'create/turn':>13}{'change':>10}")
tot_b = tot_a = 0
for lo, hi in BINS:
    b = groups[(lo, hi)]["before"]
    a = groups[(lo, hi)]["after"]
    mb = sum(u.create_tokens for u in b) / len(b) if b else 0
    ma = sum(u.create_tokens for u in a) / len(a) if a else 0
    chg = f"{ma/mb-1:>+10.0%}" if b and a else " " * 10
    print(f"{label(lo,hi):>14}{len(b):>10}{mb:>13,.0f}{len(a):>10}{ma:>13,.0f}{chg}")
    if b and a:
        tot_b += mb * min(len(b), len(a))
        tot_a += ma * min(len(b), len(a))

if tot_b:
    print(f"\ndepth-standardised creation per turn: {tot_a/tot_b-1:+.0%} "
          f"after the switch")


# Warm-up or steady state? A restart empties the prefix-replay store and
# re-latches the pinned build id, so the first turns of a new process create
# more whatever the TTL is. If the regression is warm-up it decays; if it is the
# TTL it does not.
after = [(ts, n, u) for ts, n, u, _ in rows if ts >= switch]
after.sort()
if after:
    span = after[-1][0] - after[0][0]
    print(f"\nafter-window span {span/60:.0f} min, {len(after)} turns")
    q = max(1, len(after) // 4)
    print(f"{'quarter':>10}{'turns':>7}{'median depth':>14}{'create/turn':>13}")
    for i in range(4):
        part = after[i*q:(i+1)*q] if i < 3 else after[3*q:]
        if not part:
            continue
        depths = sorted(n for _, n, _ in part)
        print(f"{i+1:>10}{len(part):>7}{depths[len(depths)//2]:>14}"
              f"{sum(u.create_tokens for _, _, u in part)/len(part):>13,.0f}")
