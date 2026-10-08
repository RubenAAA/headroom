import json, sys

def load(path):
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows

obs_path, exc_path = sys.argv[1], sys.argv[2]
obs = load(obs_path)
exc = load(exc_path)
print("observed lines:", len(obs))
print("exchange lines:", len(exc))
print("--- observed[0] fields ---")
print(json.dumps(obs[0]["fields"], indent=1))
print("--- exchange[0] fields ---")
print(json.dumps(exc[0]["fields"], indent=1))
