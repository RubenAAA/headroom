import json, sys

mx_in = 0
over_1_6MB = 0
n_body = 0
ct = []
with open(sys.argv[1]) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            continue
        fl = r.get("fields", {})
        e = fl.get("event")
        if e == "outbound_body_bytes":
            n_body += 1
            b = fl.get("bytes_in", 0)
            mx_in = max(mx_in, b)
            if b >= 1600000:
                over_1_6MB += 1
        elif e == "count_tokens_local":
            ct.append((fl.get("input_tokens", 0), fl.get("model", "?")))
print("outbound_body_bytes: n=%d max_bytes_in=%d (=%dk est tok) turns>=1.6MB: %d" % (
    n_body, mx_in, mx_in // 4000 if isinstance(mx_in, int) else 0, over_1_6MB))
ct.sort(reverse=True)
print("count_tokens_local: n=%d" % len(ct))
print("top 10 input_tokens:", [(t, m) for t, m in ct[:10]])
print("count_tokens over 400k:", sum(1 for t, m in ct if t >= 400000))
