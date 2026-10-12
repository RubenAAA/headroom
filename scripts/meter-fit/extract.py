"""Extract per-turn Anthropic usage, model, and subscription meter samples
from the proxy logs into compact JSONL. Usage: extract.py OUT LOG..."""
import json, sys, subprocess

out, logs = sys.argv[1], sys.argv[2:]
pat = '"turn_cost_ledger"|"unified_utilization_sample"|"savings_pricing_counterfactual"'
rows = []
model = {}
for log in logs:
    p = subprocess.Popen(['grep', '-h', '-E', pat, log], stdout=subprocess.PIPE,
                         env={'LC_ALL': 'C'}, text=True, errors='replace')
    for l in p.stdout:
        try:
            d = json.loads(l)
        except Exception:
            continue
        x = d['fields']; e = x.get('event'); ts = d['timestamp']
        if e == 'savings_pricing_counterfactual':
            model[x.get('request_id')] = x.get('model')
        elif e == 'turn_cost_ledger':
            rows.append({'k': 'turn', 'ts': ts, 'rid': x.get('request_id'),
                         'in': x.get('input_tokens', 0), 'read': x.get('cache_read_input_tokens', 0),
                         'w5': x.get('cache_write_5m_tokens', 0), 'w1h': x.get('cache_write_1h_tokens', 0),
                         'write': x.get('cache_creation_input_tokens', 0), 'out': x.get('output_tokens', 0),
                         'r_in': x.get('rounds_input_tokens', 0), 'r_read': x.get('rounds_cache_read_tokens', 0),
                         'r_write': x.get('rounds_cache_write_tokens', 0), 'conv': x.get('conversation_key')})
        elif e == 'unified_utilization_sample':
            rows.append({'k': 'util', 'ts': ts, 'rid': x.get('request_id'), 'win': x.get('window'),
                         'u': x.get('utilization'), 'reset': x.get('reset_unix'), 'status': x.get('status')})
    p.wait()
with open(out, 'w') as f:
    for r in sorted(rows, key=lambda r: r['ts']):
        if r['k'] == 'turn':
            r['model'] = model.get(r['rid'])
        f.write(json.dumps(r) + '\n')
print(sum(r['k'] == 'turn' for r in rows), 'turns', sum(r['k'] == 'util' for r in rows), 'samples',
      sum(1 for r in rows if r['k'] == 'turn' and model.get(r['rid'])), 'with model')
