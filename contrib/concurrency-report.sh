#!/usr/bin/env bash
# Verdict on the 2026-09-15 concurrency fixes, from the proxy log.
#
#   concurrency-report.sh            # since the proxy last started
#   concurrency-report.sh --all      # whole current log file
#
# Three lines say whether the stalls are gone, one says whether
# --cache-stampede-gate is earning its keep. Rerun after a day of normal use.
set -euo pipefail
LOG="${HEADROOM_PROXY_LOG:-$HOME/headroom-proxy.log}"
python3 - "$LOG" "${1:-}" <<'PY'
import json, re, sys, collections
log, mode = sys.argv[1], sys.argv[2]
lines = open(log, errors="replace").read().splitlines()
if mode != "--all":
    for i in range(len(lines) - 1, -1, -1):
        if '"headroom-proxy starting"' in lines[i]:
            lines = lines[i:]
            break
c = collections.Counter()
stall = 0
turns = 0
followers = collections.Counter()
follower_ids = {}
reads_after = 0
for line in lines:
    if '"event"' not in line and "PERF " not in line:
        continue
    try:
        f = json.loads(line).get("fields", {})
    except ValueError:
        continue
    ev = f.get("event", "")
    msg = f.get("message", "")
    if "] PERF " in msg:
        rid = msg[1:msg.index("]")]
        m = re.search(r"cache_read=(\d+)", msg)
        if rid in follower_ids and m and int(m.group(1)) > 0:
            reads_after += 1
            follower_ids.pop(rid, None)
        continue
    if ev == "sidecar_fallback" and f.get("status") == 404:
        c["sidecar_404"] += 1
    elif ev == "sidecar_direct_skipped":
        c["sidecar_skipped"] += 1
    elif ev == "zen_concurrency_cap_exceeded":
        c["zen_cap"] += 1
    elif ev == "stampede_follower_released":
        followers[f.get("release")] += 1
        follower_ids[f.get("request_id")] = True
    elif ev == "stage_timings":
        turns += 1
        try:
            st = json.loads(f.get("stages") or "{}")
        except ValueError:
            continue
        if (st.get("parse") or 0) > 5000 or (st.get("pre_forward") or 0) > 5000:
            stall += 1

def verdict(ok, text):
    return ("OK   " if ok else "BAD  ") + text

print(f"turns seen: {turns}   ({'whole file' if mode == '--all' else 'since last proxy start'})")
print(verdict(c["sidecar_404"] == 0,
      f"sidecar 404 fallbacks: {c['sidecar_404']}  (skipped early instead: {c['sidecar_skipped']})"))
print(verdict(c["zen_cap"] == 0, f"zen slot cap timeouts: {c['zen_cap']}"))
print(verdict(stall == 0, f"proxy-side stalls over 5 s: {stall}"))
total = sum(followers.values())
warm = followers.get("leader_warm", 0)
print(f"stampede gate: {total} followers held "
      f"(leader_warm {warm}, timeout {followers.get('timeout', 0)}, "
      f"stale_leader {followers.get('stale_leader', 0)}); "
      f"{reads_after} of them then read cache")
if turns < 200:
    print("  -> too few turns to judge the gate yet; rerun after a day")
elif total == 0:
    print("  -> no follower in this window: the gate did nothing; safe to remove"
          " --cache-stampede-gate from the flag file")
elif warm and reads_after:
    print("  -> followers waited and then read cache: keep the gate on")
else:
    print("  -> followers waited but did not read cache: gate is costing time"
          " for nothing; turn it off")
PY
