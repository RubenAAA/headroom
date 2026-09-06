"""Where does billed cache creation go, and what is recoverable?

    python3 _wastewhere.py <capture_dir> <ledger.log>

Reads the bill, not the simulator. After a turn finishes, the provider holds
everything up to that turn's last breakpoint, which is `read + create`; its
`input_tokens` is by definition the part that fell outside and was never cached.
So if the next turn's `cache_read_input_tokens` is short of `read + create`, the
difference is content the provider had and we paid to write again.

Getting `held` wrong by including `input_tokens` spreads the same total thinly
over every turn (1,542 each across 1,884 turns) instead of concentrating it where
it belongs (84 turns of 1,950). The concentrated version is the one that names a
cause.

Build the ledger by concatenating `~/headroom-proxy.log.*` oldest-first, then the
live file — it rotates on restart, and reading only the live file loses most of
the window.
"""
import collections
import json
import re
import sys
from datetime import datetime

from cachesim import load_corpus, load_ledger, ledger_usage

corpus, log = sys.argv[1], sys.argv[2]

# The proxy names its own replay declines; anything else is unattributed.
skip, starts = {}, []
for line in open(log, errors="replace"):
    if '"prefix_replay_not_replayed"' in line:
        rid = re.search(r'"request_id":"([0-9a-f-]{36})"', line)
        rsn = re.search(r'"reason":"([a-z_]+)"', line)
        if rid:
            skip[rid.group(1)] = rsn.group(1) if rsn else "?"
    elif '"listening' in line:
        try:
            starts.append(datetime.fromisoformat(
                json.loads(line)["timestamp"].replace("Z", "+00:00")).timestamp())
        except (json.JSONDecodeError, KeyError, ValueError):
            pass
starts.sort()

turns = load_corpus(corpus)
ledger = load_ledger(log)
scopes = collections.defaultdict(list)
for t in turns:
    if t.request_id in ledger:
        scopes[(t.session_key, t.body.get("model"))].append(t)

rows, total_create = [], 0
for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    prev = None
    for t in ts:
        u = ledger_usage(ledger[t.request_id])
        if prev is not None:
            pu = prev
            total_create += u.create_tokens
            short = pu.read_tokens + pu.create_tokens - u.read_tokens
            if short > 0:
                since = min((t.ts - s for s in starts if s <= t.ts), default=None)
                rows.append((short, skip.get(t.request_id, "replayed, unexplained"),
                             since, t.request_id))
        prev = u

pairs = sum(len(ts) - 1 for ts in scopes.values() if len(ts) > 1)
total_short = sum(r[0] for r in rows)
print(f"{pairs} consecutive turn pairs; creation billed {total_create:,.0f}")
print(f"failed re-use {total_short:,.0f} = {total_short/max(total_create,1):.0%} "
      f"of creation, concentrated on {len(rows)} turns\n")

agg = collections.defaultdict(lambda: [0, 0.0, []])
for short, reason, since, _ in rows:
    a = agg[reason]
    a[0] += 1
    a[1] += short
    if since is not None:
        a[2].append(since)

print(f"{'cause':>24}{'turns':>7}{'tokens':>12}{'share':>7}{'per turn':>10}"
      f"{'median s since restart':>24}")
for k, (n, s, sinces) in sorted(agg.items(), key=lambda kv: -kv[1][1]):
    sinces.sort()
    med = f"{sinces[len(sinces)//2]:,.0f}" if sinces else "-"
    print(f"{k:>24}{n:>7}{s:>12,.0f}{s/max(total_short,1):>7.0%}{s/n:>10,.0f}"
          f"{med:>24}")

print("\n`no_previous_turn` is the replay store being empty after a restart — the"
      "\nturns land within minutes of a proxy start. `replayed, unexplained` is"
      "\nturns whose forwarded bytes are a byte-perfect append-only extension of"
      "\nwhat the provider held, served nothing above the system block anyway.")
