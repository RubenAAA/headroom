"""Leave-one-window-out check and window bootstrap for the 5h fit.
Reuses fit.py's segment build by exec'ing its top half."""
import sys, collections
import numpy as np
from scipy.optimize import nnls

src = open('fit.py').read().split('def score')[0]
sys.argv = ['fit.py', sys.argv[1] if len(sys.argv) > 1 else '5h']
exec(src)

api = {}
for f, m in (('opus', 5), ('sonnet', 3), ('haiku', 1)):
    for c, r in (('in', 1), ('w5', 1.25), ('w1h', 2), ('read', 0.1), ('out', 5)):
        api[(f, c)] = m * r
A = np.array([api[k] for k in keys])
cols = [i for i, k in enumerate(keys) if tot[i] > 1e5 and k[0] != 'haiku']
wins = np.array([m[0] for m in meta])
uw = sorted(set(wins))

# held-out: per-window absolute error of predicted total rise
err_api, err_free = [], []
for w in uw:
    tr, te = wins != w, wins == w
    s = y[tr].sum() / (X[tr] @ A).sum()
    err_api.append(abs(y[te].sum() - s * (X[te] @ A).sum()))
    coef, _ = nnls(X[tr][:, cols] / 1e6, y[tr])
    err_free.append(abs(y[te].sum() - (X[te][:, cols] / 1e6 @ coef).sum()))
print(f'held-out window error, share of rise: API {sum(err_api) / y.sum():.1%}  free {sum(err_free) / y.sum():.1%}')

# bootstrap windows: coefficient spread, expressed per class relative to that model's fresh input at API (in=1)
rng = np.random.default_rng(0)
B = []
for _ in range(300):
    pick = rng.choice(uw, len(uw))
    idx = np.concatenate([np.where(wins == p)[0] for p in pick])
    coef, _ = nnls(X[idx][:, cols] / 1e6, y[idx])
    B.append(coef)
B = np.array(B)
print('free fit, meter % per 1M tokens: median [5%, 95%]   (API-price shape for comparison)')
s_api = y.sum() / (X @ A).sum() * 1e6
for j, c in enumerate(cols):
    lo, md, hi = np.percentile(B[:, j], [5, 50, 95])
    print(f'  {keys[c][0]:6s} {keys[c][1]:5s} {md:8.3f} [{lo:7.3f}, {hi:7.3f}]   api {A[c] * s_api:7.3f}')

# where the meter went, under each model
coef, _ = nnls(X[:, cols] / 1e6, y)
share_free = {keys[c]: (X[:, c] / 1e6 * coef[j]).sum() for j, c in enumerate(cols)}
share_api = {keys[c]: (X[:, c] * A[c]).sum() for c in cols}
for name, sh in (('free', share_free), ('api', share_api)):
    t = sum(sh.values()); by = collections.Counter()
    for (f, c), v in sh.items():
        by[c] += v / t
    print(f'meter share by class ({name}):', {k: f'{v:.0%}' for k, v in by.most_common()})
