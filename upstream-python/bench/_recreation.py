"""How much of the bill is content being written a second time?

Reads price at ~0.01x, so the bill is cache creation. Creation splits in two:
tokens a turn genuinely appends, which nothing can avoid, and tokens that were
already written on an earlier turn and had to be written again. Only the second
kind is recoverable, and it is the only thing that can put the proxy below plain
Claude Code rather than level with it.

Attributes every creation token to one bucket or the other, for both arms.
"""
import collections
import sys

from cachesim import (load_corpus, with_forwarded_bodies, score_corpus, flatten,
                      DEFAULT_BYTES_PER_TOKEN as BPT)

P = sys.argv[1]
raw = load_corpus(P)
fwd = {t.request_id: t for t in with_forwarded_bodies(raw, P)}
raw = [t for t in raw if t.request_id in fwd]


def survey(turns, label):
    _, per_turn = score_corpus(turns, BPT)
    create = {t.request_id: u.create_tokens for t, u in per_turn}

    scopes = collections.defaultdict(list)
    for t in turns:
        scopes[(t.session_key, t.body.get("model"))].append(t)

    fresh = rewrite = 0
    worst = []
    for _, ts in scopes.items():
        ts.sort(key=lambda t: t.ts)
        # Every digest this scope has ever put on the wire.
        ever = set()
        for t in ts:
            segs = flatten(t.body, BPT)
            new = sum(s.tokens for s in segs if s.digest not in ever)
            got = create[t.request_id]
            # Creation is bounded by what the breakpoints covered, so a turn can
            # write less than it appended. Only the excess is a second writing.
            again = max(0, got - new)
            fresh += min(got, new)
            rewrite += again
            if again:
                worst.append((again, t.request_id, got, new, len(segs)))
            ever.update(s.digest for s in segs)

    total = fresh + rewrite
    print(f"{label:>12}{total:>12,}{fresh:>12,}{rewrite:>12,}"
          f"{rewrite / total if total else 0:>9.1%}")
    return worst


print(f"{'arm':>12}{'created':>12}{'first time':>12}{'again':>12}{'share':>9}")
worst_c = survey(raw, "claude code")
worst_p = survey([fwd[t.request_id] for t in raw], "proxy")

print("\nclaude code's own rewrite, by size of event")
print(f"{'again':>10}{'created':>10}{'appended':>10}{'segs':>7}  turns  cumulative")
worst_c.sort(reverse=True)
run = 0
for again, rid, got, new, n in worst_c[:10]:
    run += again
    print(f"{again:>10,}{got:>10,}{new:>10,}{n:>7}{'':>7}{run:>12,}")
tail = sum(w[0] for w in worst_c[10:])
print(f"{'rest':>10}{'':>10}{'':>10}{'':>7}{len(worst_c) - 10:>7}{run + tail:>12,}")

sizes = collections.Counter(w[0] for w in worst_c)
print("\nthe recurring sizes — a fixed block rewritten turn after turn")
for size, n in sizes.most_common(6):
    print(f"  {size:>8,} tokens x {n:>4} turns = {size * n:>10,}")
