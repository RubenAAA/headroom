#!/usr/bin/env bash
# Anomaly scan over the proxy JSON log. Local, read-only, no network.
#
# Built after 2026-09-22, when 15 memory/CCR continuation 403s sat in the
# log grouped under no event at all while a sweep reported "no bugs".
# Sections:
#   1. volume + level counts
#   2. WARN/ERROR by event — the '?' bucket (no event field) is exploded
#      in full, never sampled: an unseen '?' sample is how the 403s hid
#   3. upstream status distribution on `forwarded` turns
#   4. continuation rejections (memory + CCR) by status, model, minute —
#      matches both the new event names and the legacy message text so old
#      logs stay scannable
#   5. unknown SSE event names seen on the wire
#   6. recache waste by reason + top conversations
#   7. vs-stock aggregate (proxy effective vs stock effective)
#
# Usage: bash scripts/scan-log.sh [--log PATH] [--hours N]

set -euo pipefail

LOG="$HOME/headroom-proxy.log"
HOURS=6

while [[ $# -gt 0 ]]; do
    case "$1" in
        --log) LOG="$2"; shift 2 ;;
        --hours) HOURS="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

python3 - "$LOG" "$HOURS" <<'EOF'
import json, sys, datetime, collections

log_path, hours = sys.argv[1], float(sys.argv[2])
now = datetime.datetime.now(datetime.timezone.utc)
cutoff = now.timestamp() - hours * 3600

levels = collections.Counter()
warn_ev = collections.Counter()
unknown_samples = []
status = collections.Counter()
rej = []  # (ts, kind, status, model, req)
sse_names = collections.Counter()
rec_reason = collections.Counter()
rec_waste = collections.Counter()
rec_conv = collections.Counter()
rec_conv_waste = collections.Counter()
vs_o = vs_s = vs_n = vs_neg = 0
n = 0

def ts_of(o):
    try:
        return datetime.datetime.fromisoformat(
            o.get('timestamp', '').replace('Z', '+00:00')).timestamp()
    except ValueError:
        return None

with open(log_path) as f:
    for line in f:
        try:
            o = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        t = ts_of(o)
        if t is None or t < cutoff:
            continue
        n += 1
        lv = o.get('level', '')
        levels[lv] += 1
        fl = o.get('fields') or {}
        ev = fl.get('event', '?')

        if lv in ('WARN', 'ERROR'):
            warn_ev[(lv, ev)] += 1
            if ev == '?':
                unknown_samples.append(line[:300].rstrip())

        msg = fl.get('message', '')
        if msg == 'forwarded' and 'upstream_status' in fl:
            status[fl['upstream_status']] += 1

        if ev in ('memory_continuation_rejected', 'ccr_continuation_rejected') or \
                'upstream returned error during continuation' in msg:
            kind = 'memory' if 'memory' in ev or msg.startswith('memory') else 'ccr'
            rej.append((t, kind, str(fl.get('status', '?')),
                        fl.get('model', ''), fl.get('request_id', '')[:8]))

        if ev == 'sse_unknown_event':
            sse_names[fl.get('event_name', '?')] += 1

        if ev == 'cache_recache_observed':
            r = fl.get('attribution_reason', '?')
            w = fl.get('wasted_tokens', 0) or 0
            rec_reason[r] += 1
            rec_waste[r] += w
            c = fl.get('conversation_key', '?')
            rec_conv[c] += 1
            rec_conv_waste[c] += w

        if ev == 'vs_stock_turn':
            a, b = fl.get('ours_effective'), fl.get('stock_effective')
            if isinstance(a, (int, float)) and isinstance(b, (int, float)):
                vs_o += a
                vs_s += b
                vs_n += 1
                if a > b:
                    vs_neg += 1

print(f'== scan: last {hours}h of {log_path} ==')
print(f'lines={n} levels={dict(levels)}')
print('--- WARN/ERROR by event ---')
for (lv, ev), c in sorted(warn_ev.items(), key=lambda kv: -kv[1]):
    print(f'  {c:5d} {lv} {ev}')
if unknown_samples:
    print(f'--- NO-EVENT lines ({len(unknown_samples)} — full list, never sampled) ---')
    for s in unknown_samples:
        print(f'  {s}')
print('--- forwarded upstream_status ---')
print('  ' + (dict(status).__str__() if status else 'no forwarded lines in window'))
print('--- continuation rejections ---')
if rej:
    by_status = collections.Counter((k, s) for _, k, s, _, _ in rej)
    print('  by kind+status:', dict(by_status))
    by_min = collections.Counter(
        (datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime('%H:%M'), k, s)
        for t, k, s, _, _ in rej)
    print('  worst minutes:', dict(sorted(by_min.items(), key=lambda kv: -kv[1])[:8]))
    for t, k, s, m, r in rej[:12]:
        print(f'  {datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%H:%M:%S")} {k} {s} {m} {r}')
else:
    print('  none')
print('--- unknown SSE names ---')
print('  ' + (dict(sse_names).__str__() if sse_names else 'none'))
print('--- recache ---')
for r, c in sorted(rec_reason.items(), key=lambda kv: -rec_waste[kv[0]])[:8]:
    print(f'  {c:4d} events waste={rec_waste[r]:9d} {r}')
print('  top conversations by waste:')
for c, w in sorted(rec_conv_waste.items(), key=lambda kv: -kv[1])[:6]:
    print(f'    waste={w:8d} events={rec_conv[c]:3d} {c}')
print('--- vs-stock ---')
if vs_n and vs_s:
    print(f'  turns={vs_n} saving={(1 - vs_o / vs_s) * 100:.1f}% negative_turns={vs_neg}')
else:
    print('  no compared turns in window')
EOF
