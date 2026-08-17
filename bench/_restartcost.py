"""What does a proxy restart cost, and did persisting prefixes fix it?

    python3 _restartcost.py <capture_dir> <full_proxy_log>

The replay store used to be memory-only, so a restart made the first turn of
every live conversation report `no_previous_turn` and rebuild its history. This
reports the creation billed on turns shortly after a proxy start, against turns
of the same conversation depth elsewhere — so the answer is not just conversation
depth again.

Run it after the next restart on `--replay-store-dir` to check the fix held:
`no_previous_turn` turns should stop appearing within minutes of a start, and
`prefix_replay_rehydrated` should appear instead.
"""
import collections
import json
import re
import sys
from datetime import datetime

from cachesim import load_corpus, load_ledger, ledger_usage

corpus, log = sys.argv[1], sys.argv[2]

starts, skip, rehydrated = [], {}, set()
for line in open(log, errors="replace"):
    if '"listening' in line:
        try:
            starts.append(datetime.fromisoformat(
                json.loads(line)["timestamp"].replace("Z", "+00:00")).timestamp())
        except (json.JSONDecodeError, KeyError, ValueError):
            pass
    elif '"prefix_replay_not_replayed"' in line:
        rid = re.search(r'"request_id":"([0-9a-f-]{36})"', line)
        rsn = re.search(r'"reason":"([a-z_]+)"', line)
        if rid:
            skip[rid.group(1)] = rsn.group(1) if rsn else "?"
    elif '"prefix_replay_rehydrated"' in line:
        rehydrated.add(line)
starts.sort()

turns = load_corpus(corpus)
ledger = load_ledger(log)
scopes = collections.defaultdict(list)
for t in turns:
    if t.request_id in ledger:
        scopes[(t.session_key, t.body.get("model"))].append(t)

BINS = [(0, 50), (50, 150), (150, 400), (400, 10**9)]
WINDOW = 300  # seconds after a start that count as "just restarted"

rows = []
for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    for i, t in enumerate(ts):
        if i == 0:
            continue                       # no prefix to reuse; never comparable
        since = min((t.ts - s for s in starts if s <= t.ts), default=None)
        rows.append((since is not None and since <= WINDOW,
                     len(t.body.get("messages") or []),
                     ledger_usage(ledger[t.request_id]).create_tokens,
                     skip.get(t.request_id)))

print(f"{len(starts)} proxy starts; {len(rehydrated)} prefixes rehydrated from disk")
print(f"{sum(1 for r in rows if r[0])} of {len(rows)} turns fall within "
      f"{WINDOW}s of a start\n")

print(f"{'depth':>10}{'settled n':>11}{'create/turn':>13}"
      f"{'just restarted n':>18}{'create/turn':>13}{'change':>9}")
for lo, hi in BINS:
    a = [r[2] for r in rows if not r[0] and lo <= r[1] < hi]
    b = [r[2] for r in rows if r[0] and lo <= r[1] < hi]
    label = f"{lo}-{hi}" if hi < 10**9 else f"{lo}+"
    ma = sum(a) / len(a) if a else 0
    mb = sum(b) / len(b) if b else 0
    chg = f"{mb/ma-1:>+9.0%}" if a and b else " " * 9
    print(f"{label:>10}{len(a):>11}{ma:>13,.0f}{len(b):>18}{mb:>13,.0f}{chg}")

near = [r for r in rows if r[0] and r[3] == "no_previous_turn"]
print(f"\n`no_previous_turn` within {WINDOW}s of a start: {len(near)} turns, "
      f"{sum(r[2] for r in near):,} tokens created")
print("Once persistence is live this should trend to zero.")
