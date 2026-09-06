"""Why did Claude Code re-create a whole prefix it had already cached?

Three turns carry 99.7% of the client's own rewrite. Each one is either an
expiry the proxy could hold open, an edit to history the proxy could replay
around, or a change to the system/tools block the proxy could stabilise — and
the three want completely different fixes.
"""
import collections
import sys

from cachesim import (load_corpus, with_forwarded_bodies, score_corpus, flatten,
                      _blocks, DEFAULT_BYTES_PER_TOKEN as BPT)

P = sys.argv[1]
raw = load_corpus(P)
fwd = {t.request_id: t for t in with_forwarded_bodies(raw, P)}
raw = [t for t in raw if t.request_id in fwd]

_, per_turn = score_corpus(raw, BPT)
create = {t.request_id: u.create_tokens for t, u in per_turn}

scopes = collections.defaultdict(list)
for t in raw:
    scopes[(t.session_key, t.body.get("model"))].append(t)

events = []
for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    ever = set()
    for i, t in enumerate(ts):
        segs = flatten(t.body, BPT)
        new = sum(s.tokens for s in segs if s.digest not in ever)
        again = max(0, create[t.request_id] - new)
        if again > 10_000:
            events.append((again, i, ts))
        ever.update(s.digest for s in segs)

print(f"{len(events)} turns rewrote more than 10k tokens\n")
for again, i, ts in sorted(events, reverse=True):
    t, prev = ts[i], ts[i - 1] if i else None
    cur = flatten(t.body, BPT)
    kinds = [role for role, _ in _blocks(t.body)]
    print(f"rewrote {again:,} tokens — turn {i} of {len(ts)} in its scope")
    if prev is None:
        print("  first turn in scope: nothing was cached yet\n")
        continue
    old = flatten(prev.body, BPT)
    j = 0
    while j < min(len(old), len(cur)) and old[j].digest == cur[j].digest:
        j += 1
    print(f"  gap since previous turn: {t.ts - prev.ts:,.0f}s")
    print(f"  segments {len(old)} -> {len(cur)}, first difference at {j}"
          f" ({kinds[j] if j < len(kinds) else '(end)'})")
    print(f"  tokens before the difference {sum(s.tokens for s in cur[:j]):,},"
          f" after {sum(s.tokens for s in cur[j:]):,}")
    ttls = collections.Counter(s.breakpoint_ttl for s in old if s.breakpoint_ttl)
    print(f"  previous turn's breakpoints: {dict(ttls)}")
    # Did the same content sit in an EARLIER turn? Then it is not an edit, it
    # is an expiry or an eviction.
    seen_before = set()
    for k in range(i):
        seen_before.update(s.digest for s in flatten(ts[k].body, BPT))
    held = sum(s.tokens for s in cur[:j] if s.digest in seen_before)
    print(f"  of the matching prefix, {held:,} tokens had been on the wire"
          f" before the previous turn too\n")
