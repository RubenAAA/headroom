#!/usr/bin/env python3
"""Check 3: hash soak. Send marked multi-message requests, compare sha256 of
sent vs stub-received normalized messages.

Normalization: recursive strip of `cache_control` keys (E3 auto-placement is
a legitimate transform), canonical JSON (sort_keys, separators). Hash covers
the messages/input array only, so tool/schema passes can't cause mismatches.

Each request carries a unique marker in message content; the compare step
finds the stub capture containing the marker and diffs count+hash.

Usage:
  python3 -I soak_hash.py send <proxy_base> <sent_jsonl> <n>
  python3 -I soak_hash.py compare <sent_jsonl> <capture_dir>
"""
import hashlib
import json
import sys
import urllib.request

HDR = {"Content-Type": "application/json", "anthropic-version": "2023-06-01",
       "Authorization": "Bearer sk-ant-oat-sideprobe",
       "User-Agent": "claude-code/9.9-sideprobe"}


def strip_cc(o):
    if isinstance(o, dict):
        return {k: strip_cc(v) for k, v in o.items() if k != "cache_control"}
    if isinstance(o, list):
        return [strip_cc(v) for v in o]
    return o


def msg_hash(msgs):
    canon = json.dumps(strip_cc(msgs), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canon.encode()).hexdigest()[:16]


def shapes(i):
    m = f"mk{i:05d}"
    anth2 = {"model": "claude-sonnet-4-5", "max_tokens": 8,
             "system": [{"type": "text", "text": "hash-soak"}],
             "messages": [{"role": "user", "content": f"first {m}"},
                          {"role": "user", "content": f"second {m}"}],
             "tools": [{"name": "n", "description": "d",
                        "input_schema": {"type": "object", "properties": {}}}]}
    chat2 = {"model": "gpt-5-mini",
             "messages": [{"role": "system", "content": f"s {m}"},
                          {"role": "user", "content": f"hi {m}"}]}
    anth3 = {"model": "claude-sonnet-4-5", "max_tokens": 8,
             "system": "hash-soak",
             "messages": [{"role": "user", "content": f"q1 {m}"},
                          {"role": "assistant", "content": f"a1 {m}"},
                          {"role": "user", "content": f"q2 {m}"}]}
    return [(("/v1/messages", anth2), ("/v1/chat/completions", chat2),
             ("/v1/messages", anth3))[i % 3]]


def do_send(base, sent_path, n):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    out = open(sent_path, "w")
    oks = errs = 0
    for i in range(n):
        [(path, body)] = shapes(i)
        msgs = body["messages"]
        rec = {"i": i, "marker": f"mk{i:05d}", "path": path,
               "sent_n": len(msgs), "sent_hash": msg_hash(msgs),
               "sent_len": len(json.dumps(body).encode())}
        data = json.dumps(body).encode()
        req = urllib.request.Request(base + path, data=data, headers=HDR,
                                     method="POST")
        try:
            r = opener.open(req, timeout=60)
            rec["http"] = r.status
            r.read()
            oks += 1
        except Exception as e:
            rec["http"] = f"ERR {type(e).__name__}: {e}"[:100]
            errs += 1
        out.write(json.dumps(rec) + "\n")
        out.flush()
    print(f"sent ok={oks} err={errs}")
    # verify all sent records parse
    out.close()


def do_compare(sent_path, cap_dir):
    import glob
    sent = [json.loads(l) for l in open(sent_path)]
    caps = []
    for fn in glob.glob(cap_dir + "/*.json"):
        d = json.load(open(fn))
        d["_blob"] = json.dumps(d.get("body"))
        caps.append(d)
    by_marker = {}
    for c in caps:
        for rec in sent:
            if rec["marker"] in c["_blob"]:
                by_marker.setdefault(rec["marker"], []).append(c)
    mismatch = missing = multi = 0
    for rec in sent:
        got = by_marker.get(rec["marker"], [])
        if not got:
            missing += 1
            print(f"i={rec['i']} {rec['path']} http={rec['http']} NO-CAPTURE")
            continue
        if len(got) > 1:
            multi += 1
            print(f"i={rec['i']} {rec['path']} MULTI-CAPTURE n={len(got)} "
                  f"seqs={[c['seq'] for c in got]}")
        for c in got:
            b = c.get("body") or {}
            m = b.get("messages", b.get("input"))
            n = len(m) if isinstance(m, list) else "?"
            h = msg_hash(m) if isinstance(m, list) else "?"
            if n != rec["sent_n"] or h != rec["sent_hash"]:
                mismatch += 1
                print(f"i={rec['i']} seq={c['seq']} {rec['path']} "
                      f"sent_n={rec['sent_n']} got_n={n} "
                      f"sent_h={rec['sent_hash']} got_h={h}")
                print("  got body:", json.dumps(b)[:600])
    print(f"records={len(sent)} missing={missing} multi={multi} mismatches={mismatch}")


if __name__ == "__main__":
    if sys.argv[1] == "send":
        do_send(sys.argv[2], sys.argv[4], int(sys.argv[3]))
    elif sys.argv[1] == "compare":
        do_compare(sys.argv[2], sys.argv[3])
