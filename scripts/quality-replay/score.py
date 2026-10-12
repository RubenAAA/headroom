"""Summarize judged replays: does the forwarded (compressed) arm lose to the
original more often than one original run loses to another?

Usage: score.py RUN_DIR...
"""
import json, math, os, sys

c_win = c_loss = tie = 0
ctrl = {'a': 0, 'b': 0, 'tie': 0}
first_pos = n_pos = 0
spent = 0.0
pairs = 0
for d in sys.argv[1:]:
    for f in sorted(os.listdir(d)):
        r = json.load(open(os.path.join(d, f)))
        spent += r.get('meter_pct', 0) + r.get('judge_meter_pct', 0)
        j = r.get('judge')
        if not j:
            continue
        pairs += 1
        for k in ('ac', 'bc'):
            w = j[k]['winner']
            if w == 'c':
                c_win += 1
            elif w in ('a', 'b'):
                c_loss += 1
            elif w == 'tie':
                tie += 1
        w = j['ab']['winner']
        if w in ctrl:
            ctrl[w] += 1

decided = c_win + c_loss
rate = c_win / decided if decided else float('nan')
# Normal-approximation 95% interval; pairs share an arm, so this is optimistic.
half = 1.96 * math.sqrt(rate * (1 - rate) / decided) if decided else float('nan')
print(f'{pairs} pairs judged, est. {spent:.1f}% of a 5h window spent')
print(f'compressed vs original: {c_win} wins, {c_loss} losses, {tie} ties '
      f'-> win rate {rate:.0%} (+/- {half:.0%}); 50% = no quality change')
print(f"control (original vs original): a {ctrl['a']}, b {ctrl['b']}, tie {ctrl['tie']}")
