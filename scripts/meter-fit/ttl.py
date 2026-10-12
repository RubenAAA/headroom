"""Forced 1h TTL vs a 5m TTL, priced in 5h-meter % with the fitted weights.
Per conversation, a turn more than 5 min after the previous one would find its
prefix expired under 5m and re-write what it read. Prefix sharing across
conversations (subagent fan-out) is ignored, which flatters 1h slightly."""
import json, collections, datetime
W = {  # 5h meter % per 1M tokens, free fit medians (stability.py)
    'opus': {'w1h': 2.694, 'read': 0.070, 'out': 18.65},
    'sonnet': {'w1h': 2.831, 'read': 0.051, 'out': 9.377},
}
fam = lambda m: 'opus' if m and 'opus' in m else 'sonnet' if m and 'sonnet' in m else None
turns = [json.loads(l) for l in open('data.jsonl')]
turns = [t for t in turns if t['k'] == 'turn' and fam(t['model']) and t.get('conv')]
ts = lambda s: datetime.datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp()
days = (ts(turns[-1]['ts']) - ts(turns[0]['ts'])) / 86400

for ratio_name, r5 in (('5m write = 5/8 of 1h (API ratio)', 1.25 / 2), ('5m write = 1h write', 1.0)):
    last = {}
    actual = sim = 0.0
    gaps = collections.Counter(); rewrite_tok = 0
    for t in turns:
        w = W[fam(t['model'])]
        writes = t['w1h'] + t['w5'] + t['r_write'] + max(0, t['write'] - t['w1h'] - t['w5'])
        reads = t['read'] + t['r_read']
        actual += (writes * w['w1h'] + reads * w['read']) / 1e6
        g = ts(t['ts']) - last.get(t['conv'], -1e18)
        last[t['conv']] = ts(t['ts'])
        expired = 300 < g <= 3600
        gaps['<=5m' if g <= 300 else '5-60m' if g <= 3600 else '>60m/first'] += 1
        if expired:
            rewrite_tok += reads
            sim += ((writes + reads) * w['w1h'] * r5) / 1e6
        else:
            sim += (writes * w['w1h'] * r5 + reads * w['read']) / 1e6
    print(f'{ratio_name}: 1h actual {actual:7.1f}%  5m simulated {sim:7.1f}%  '
          f'delta {sim - actual:+7.1f}% ({(sim - actual) / days:+.1f}% of a 5h window per day)')
print(dict(gaps), f'rewrite tokens under 5m: {rewrite_tok / 1e6:.0f}M over {days:.1f} days')
