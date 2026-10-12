"""Fit Anthropic's subscription meter (1% steps) against per-turn token classes.

Segments: within one window (same reset), consecutive first crossings of a
new running-max level. Each segment's meter rise is regressed on the tokens of
the turns that landed between the two crossings."""
import json, sys, collections
import numpy as np
from scipy.optimize import nnls

WIN = sys.argv[1] if len(sys.argv) > 1 else '5h'
rows = [json.loads(l) for l in open('data.jsonl')]

FAM = lambda m: 'opus' if m and 'opus' in m else 'sonnet' if m and 'sonnet' in m else 'haiku' if m and 'haiku' in m else None
CLS = ['in', 'w5', 'w1h', 'read', 'out']


def vec(t):
    w = t['w5'] + t['w1h']
    extra_w = t['write'] - w if t['write'] > w else 0  # unsplit writes
    return {'in': t['in'] + t['r_in'], 'w5': t['w5'], 'w1h': t['w1h'] + extra_w + t['r_write'],
            'read': t['read'] + t['r_read'], 'out': t['out']}


turns = [r for r in rows if r['k'] == 'turn']
samples = [r for r in rows if r['k'] == 'util' and r['win'] == WIN]
by_win = collections.defaultdict(list)
for s in samples:
    by_win[s['reset']].append(s)

# token cumulative per family/class, indexed by time
keys = [(f, c) for f in ('opus', 'sonnet', 'haiku') for c in CLS]
ts = [t['ts'] for t in turns]
cum = np.zeros((len(turns) + 1, len(keys)))
unknown = np.zeros(len(turns) + 1)
for i, t in enumerate(turns):
    v = np.zeros(len(keys)); f = FAM(t['model'])
    if f:
        for j, c in enumerate(CLS):
            v[keys.index((f, c))] = vec(t)[c]
    cum[i + 1] = cum[i] + v
    unknown[i + 1] = unknown[i] + (0 if f else 1)

import bisect
def tokens_between(a, b):
    i, j = bisect.bisect_right(ts, a), bisect.bisect_right(ts, b)
    return cum[j] - cum[i], unknown[j] - unknown[i], j - i

X, y, meta = [], [], []
for reset, ss in by_win.items():
    ss.sort(key=lambda s: s['ts'])
    best, at = None, None
    for s in ss:
        if best is None:
            best, at = s['u'], s['ts']; continue
        if s['u'] > best + 1e-9:
            v, unk, n = tokens_between(at, s['ts'])
            if unk == 0 and n > 0:
                X.append(v); y.append((s['u'] - best) * 100); meta.append((reset, at, s['ts']))
            best, at = s['u'], s['ts']
X, y = np.array(X), np.array(y)
print(f'{WIN}: {len(y)} segments over {len(by_win)} windows; meter rise {y.sum():.0f}%')
tot = X.sum(0)
print('tokens in segments (M):', {f'{k[0]}.{k[1]}': round(tot[i] / 1e6, 1) for i, k in enumerate(keys) if tot[i]})


def score(w, name):
    pred = X @ w
    s = y.sum() / pred.sum()  # one free scale: tokens per 1%
    r = y - s * pred
    # aggregate error over windows, robust to 1% quantization per segment
    agg = collections.defaultdict(lambda: [0.0, 0.0])
    for (reset, _, _), yi, pi in zip(meta, y, s * pred):
        agg[reset][0] += yi; agg[reset][1] += pi
    a = np.array(list(agg.values()))
    werr = np.abs(a[:, 0] - a[:, 1]).sum() / a[:, 0].sum()
    print(f'{name:38s} seg RMSE {np.sqrt((r ** 2).mean()):.3f}  window abs err {werr:6.1%}  '
          f'1% = {1 / s / 1e6:.2f}M weighted tokens')


def weights(prices):
    return np.array([prices.get(k, 0.0) for k in keys])

api = {}
for f, m in (('opus', 5), ('sonnet', 3), ('haiku', 1)):
    for c, r in (('in', 1), ('w5', 1.25), ('w1h', 2), ('read', 0.1), ('out', 5)):
        api[(f, c)] = m * r
score(weights(api), 'API price (opus 5x, sonnet 3x, haiku 1x)')
score(weights({k: v for k, v in api.items() if k[1] != 'read'}), 'API price, cache reads free')
score(weights({k: v * (1 if k[1] != 'w1h' else 1.25 / 2) for k, v in api.items()}), 'API price, 1h writes at 5m rate')
score(weights({k: (v if k[0] != 'sonnet' else v * 5 / 3) for k, v in api.items()}), 'API price, sonnet = opus')
score(weights({k: 1.0 for k in keys}), 'raw tokens, all classes equal')
score(weights({k: 1.0 for k in keys if k[1] != 'read'}), 'raw tokens, reads free')
score(weights({k: (1.0 if k[1] == 'out' else 0) for k in keys}), 'output tokens only')

# free non-negative fit on the classes with mass, scaled to API units for reading
cols = [i for i, k in enumerate(keys) if tot[i] > 1e5]
w, _ = nnls(X[:, cols] / 1e6, y)
print('\nfree NNLS, meter % per 1M tokens:')
for c, wi in zip(cols, w):
    print(f'  {keys[c][0]:6s} {keys[c][1]:5s} {wi:8.4f}')
score(np.array([w[cols.index(i)] / 1e6 if i in cols else 0 for i in range(len(keys))]), 'free NNLS (in-sample)')
