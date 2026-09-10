"""Does the subagent-5m mapping hold, and did the skip buy anything?

    python3 _ttlsubagent.py [--epoch EPOCH] [logfiles...]

With no `--epoch`, reports the mapping: for each client TTL shape
(`client_ttl` on `vs_stock_turn` lines — AllOneHour, AllFiveMinutes,
Mixed, Unmarked, or unknown on lines predating the field), the
conversation shapes it appears in. The skip is safe to enable when
all-5m turns live in short conversations and short conversations are
all-5m: then "don't upgrade all-5m bodies" touches subagent traffic and
nothing else.

With `--epoch` (or defaulting to the first `ttl_1h_pin_skipped` line),
reports the A/B: depth-binned actual creation per turn, before vs after,
for subagent-shaped conversations (span under five minutes) and overall.
First-observed turn of each conversation excluded — it has no prefix to
read and writes the lot, and counting it inverts the answer.

Pass rotated logs oldest-first
(`headroom-proxy.log.4 headroom-proxy.log.3 ... headroom-proxy.log`);
conversations are grouped per file, so a rotation never joins two runs.

Reads what the provider actually billed (`turn_cost_ledger`), not what
any model predicts. Anything touching TTLs is settled here.
"""
import collections
import datetime
import sys

BINS = [(0, 20), (20, 50), (50, 100), (100, 200), (200, 400), (400, 10 ** 9)]
SHORT_SPAN = 300  # subagent-shaped: runs to completion in seconds


def ts(s):
    return datetime.datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp()


def label(lo, hi):
    return f"{lo}-{hi}" if hi < 10 ** 9 else f"{lo}+"


def main(paths, epoch):
    ledgers = {}
    depth = {}
    shape = {}
    turn_class = {}
    skips = []
    for path in paths:
        ledgers[path] = {}
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line.startswith('{'):
                    continue
                try:
                    e = __import__('json').loads(line)
                except ValueError:
                    continue
                flds = e.get('fields', {})
                ev = flds.get('event', '')
                t = ts(e['timestamp']) if e.get('timestamp') else 0
                if ev == 'turn_cost_ledger':
                    ledgers[path][flds.get('request_id')] = {
                        'create': flds.get('cache_creation_input_tokens') or 0,
                        'read': flds.get('cache_read_input_tokens') or 0,
                        'conv': flds.get('conversation_key'),
                        'ts': t,
                    }
                elif ev == 'messages_rewritten' and flds.get('total_messages') is not None:
                    depth[flds.get('request_id')] = flds['total_messages']
                elif ev == 'vs_stock_turn':
                    rid = flds.get('request_id')
                    shape[rid] = flds.get('client_ttl', 'unknown')
                    turn_class[rid] = flds.get('turn_class')
                elif ev == 'ttl_1h_pin_skipped' and t:
                    skips.append(t)

    if epoch is None and skips:
        epoch = min(skips)
        print(f"split at first ttl_1h_pin_skipped: {epoch:.0f}")
    if epoch is not None:
        print(f"skip events at/after split: {sum(1 for t in skips if t >= epoch)}")

    convs = collections.defaultdict(list)
    for path, led in ledgers.items():
        for rid, l in led.items():
            if l['conv']:
                convs[(path, l['conv'])].append((l['ts'], rid))
    for key in convs:
        convs[key].sort()
    span_of = {k: (v[-1][0] - v[0][0] if len(v) > 1 else 0) for k, v in convs.items()}
    first_of = {k: v[0][1] for k, v in convs.items()}

    # --- mapping: shape x conversation shape ---
    print(f"\n{'shape':>14}{'turns':>8}{'convs':>7}"
          f"{'in <5min conv':>15}{'short-conv share':>17}")
    by_shape = collections.defaultdict(list)
    for path, led in ledgers.items():
        for rid, l in led.items():
            if l['conv']:
                by_shape[str(shape.get(rid, 'unknown'))].append(((path, l['conv']), rid))
    for s, items in sorted(by_shape.items()):
        convkeys = {k for k, _ in items}
        n_short = 0
        for (path, ck), rid in items:
            if span_of.get((path, ck), 0) < SHORT_SPAN:
                n_short += 1
        print(f"{s:>14}{len(items):>8}{len(convkeys):>7}"
              f"{n_short:>15}{n_short / max(1, len(items)):>16.0%}")

    if epoch is None:
        print("\nno epoch: mapping only. Re-run with --epoch to A/B.")
        return

    # --- A/B: depth-binned creation, subagent-shaped class + overall ---
    first_seen = {}
    for key, turns in convs.items():
        first_seen[key] = turns[0][1]

    def is_short(path, ck):
        return span_of.get((path, ck), 0) < SHORT_SPAN

    print(f"\n{'depth':>10}{'class n(B/A)':>14}{'create/turn B':>14}"
          f"{'create/turn A':>14}{'change':>10}")
    tot_b = tot_a = 0.0
    for lo, hi in BINS:
        for cls, name in ((True, 'short'), (False, 'rest')):
            if not cls:
                continue  # short class first; overall below
            b, a = [], []
            for path, led in ledgers.items():
                for rid, l in led.items():
                    if not l['conv'] or first_seen.get((path, l['conv'])) == rid:
                        continue
                    if not is_short(path, l['conv']):
                        continue
                    d = depth.get(rid)
                    if d is None or not (lo <= d < hi):
                        continue
                    (a if l['ts'] >= epoch else b).append(l['create'])
            mb = sum(b) / len(b) if b else 0
            ma = sum(a) / len(a) if a else 0
            chg = f"{ma / mb - 1:>+9.0%}" if b and a and mb else " " * 10
            if b or a:
                print(f"{label(lo, hi):>10}{'short ' + str(len(b)) + '/' + str(len(a)):>14}"
                      f"{mb:>14,.0f}{ma:>14,.0f}{chg}")
                if b and a:
                    tot_b += mb * min(len(b), len(a))
                    tot_a += ma * min(len(b), len(a))
    if tot_b:
        print(f"\nshort-class depth-standardised creation per turn: {tot_a / tot_b - 1:+.0%}")

    # overall (both classes, same bins)
    ob = collections.defaultdict(list)
    oa = collections.defaultdict(list)
    for path, led in ledgers.items():
        for rid, l in led.items():
            if not l['conv'] or first_seen.get((path, l['conv'])) == rid:
                continue
            d = depth.get(rid, -1)
            (oa if l['ts'] >= epoch else ob)[d].append(l['create'])
    cb = sum(sum(v) for v in ob.values())
    nb = sum(len(v) for v in ob.values())
    ca = sum(sum(v) for v in oa.values())
    na = sum(len(v) for v in oa.values())
    if nb and na:
        print(f"overall creation/turn: {cb / nb:,.0f} -> {ca / na:,.0f} ({ca / na / (cb / nb) - 1:+.0%})")


if __name__ == '__main__':
    args = sys.argv[1:]
    epoch = None
    paths = []
    i = 0
    while i < len(args):
        if args[i] == '--epoch' and i + 1 < len(args):
            epoch = float(args[i + 1])
            i += 2
        else:
            paths.append(args[i])
            i += 1
    if not paths:
        import glob
        paths = sorted(glob.glob('/home/ruben/headroom-proxy.log*'),
                       key=lambda p: (len(p), p))
    main(paths, epoch)
