#!/usr/bin/env python3
"""Which cache-key input changed on the turns that lost their cache?

Reads `turn_cache_fingerprint` (one per forwarded request, carrying every
property the provider's cache key depends on) and diffs each turn against the
previous turn of the same session. Then splits those diffs by whether the turn
was billed as a recache, so the field that explains the residue shows up as the
one that changes on recache turns and not on healthy ones.

The residue is the 58 turns / 1.29M tokens where replay was applied, the cache
died anyway, and no logged event told them apart from healthy turns.

    python3 bench/residue_diff.py [logfile]
"""
import sys, json, re, collections

LOG = sys.argv[1] if len(sys.argv) > 1 else "/home/user/headroom-proxy.log"

# Fields carried by the fingerprint. `markers` is the breakpoint map, e.g.
# "sys[0]:1h,m12:1h,m30.2:5m" — a moved or evicted breakpoint shows up here.
FIELDS = ["model", "system_digest", "tools_digest", "markers", "breakpoints", "beta_digest", "auth_digest"]

fp = {}            # request_id -> fingerprint fields
order = collections.defaultdict(list)   # session -> [(ts, request_id)]
recache = {}       # request_id -> attribution reason
tokens = {}        # request_id -> cache_creation_input_tokens
preamble_ev = set()

for raw in open(LOG, errors="replace"):
    raw = re.sub(r"^\d+:", "", raw.strip())
    if not raw.startswith("{"):
        continue
    try:
        d = json.loads(raw)
    except Exception:
        continue
    f = d.get("fields", {})
    ev, rid = f.get("event"), f.get("request_id")
    if not rid:
        continue
    if ev == "turn_cache_fingerprint":
        fp[rid] = {k: f.get(k) for k in FIELDS}
        fp[rid]["ts"] = d.get("timestamp", "")
        # Kept out of FIELDS on purpose: the ladder gains a checkpoint as the
        # conversation grows, so diffing it as a flat field would call every
        # turn "changed". The stability section below compares shared depths.
        fp[rid]["prefix_ladder"] = f.get("prefix_ladder")
        order[f.get("session_key_hash", "?")].append((d.get("timestamp", ""), rid))
    elif ev == "cache_recache_observed":
        recache[rid] = f.get("attribution_reason") or "unattributed"
        tokens[rid] = f.get("cache_creation_input_tokens") or 0
    elif ev == "forwarded_preamble_mutated_after_replay":
        preamble_ev.add(rid)

if not fp:
    sys.exit("no turn_cache_fingerprint events — is the new binary live?")

# Diff each turn against the previous turn of the same session.
changed = collections.defaultdict(lambda: collections.Counter())  # field -> {recache?: n}
per_reason = collections.defaultdict(lambda: collections.Counter())
group_n = collections.Counter()
lost = collections.Counter()

for sess, turns in order.items():
    turns.sort()
    for (_, prev), (_, cur) in zip(turns, turns[1:]):
        a, b = fp[prev], fp[cur]
        reason = recache.get(cur)
        group = reason if reason else "healthy"
        group_n[group] += 1
        lost[group] += tokens.get(cur, 0)
        for k in FIELDS:
            if a.get(k) != b.get(k):
                changed[k][group] += 1
                per_reason[reason or "healthy"][k] += 1

print(f"turns fingerprinted: {len(fp)}   sessions: {len(order)}")
print(f"consecutive pairs:   {sum(group_n.values())}")
print(f"preamble-mutation events: {len(preamble_ev)}\n")

groups = sorted(group_n, key=lambda g: -group_n[g])
w = max((len(g) for g in groups), default=8) + 2
print(f"{'group':<{w}}{'turns':>7}{'tokens':>12}   " + "".join(f"{k[:11]:>13}" for k in FIELDS))
for g in groups:
    row = f"{g:<{w}}{group_n[g]:>7}{lost[g]/1e6:>11.2f}M   "
    for k in FIELDS:
        n = changed[k][g]
        row += f"{n:>6} {n/group_n[g]:>5.0%}" if group_n[g] else f"{'':>13}"
    print(row)

print("\nRead: a field that changes far more often on a recache group than on")
print("`healthy` is the cause. A field that changes equally on both is noise.")

# The property everything rests on: the forwarded prefix is byte-stable turn
# over turn except for the appended tail. The ladder shares every checkpoint up
# to the first divergence, so the shallowest moved depth localizes it.
def ladder(s):
    out = {}
    for part in (s or "").split(","):
        if ":" in part:
            d, h = part.split(":", 1)
            out[int(d)] = h
    return out

first_move = collections.Counter()
stable = collections.Counter()
for sess, turns in order.items():
    for (_, prev), (_, cur) in zip(turns, turns[1:]):
        a, b = ladder(fp[prev].get("prefix_ladder")), ladder(fp[cur].get("prefix_ladder"))
        shared = sorted(set(a) & set(b))
        if not shared:
            continue
        moved = [d for d in shared if a[d] != b[d]]
        g = recache.get(cur) or "healthy"
        if moved:
            first_move[(g, min(moved))] += 1
        else:
            stable[g] += 1

if first_move or stable:
    print("\nforwarded-prefix stability, turn against previous turn:")
    for g in sorted(set([k[0] for k in first_move] + list(stable))):
        n = stable[g]
        moves = {d: c for (gg, d), c in first_move.items() if gg == g}
        detail = "  ".join(f"depth {d}: {c}" for d, c in sorted(moves.items())) or "-"
        print(f"  {g:<26} stable {n:>5}   first divergence -> {detail}")
    print("\n  A depth that moves on turns whose tail did NOT reach it is a real")
    print("  rewrite of already-cached bytes. Depths at or past the tail are the")
    print("  turn's own new messages and are expected.")
