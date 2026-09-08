#!/usr/bin/env bash
# Live cache performance for the statusline: the recent hit rate the watchdog
# already computes, plus create/read — what share of the cached prefix had to be
# rebuilt. Lower is better.
#
# create/read comes from the log rather than /cache-health because the steady
# figure needs each turn's cold-start flag, and the endpoint reports only
# aggregates. `~/headroom-savings.py` prints the same two numbers per run.
set -u

LOG="${HEADROOM_PROXY_LOG:-$HOME/headroom-proxy.log}"
# ~17 log lines per turn, so this is a rolling window of roughly 230 turns —
# recent enough to react, long enough that one bad turn does not own the number.
WINDOW="${HEADROOM_PERF_WINDOW:-4000}"

health=$(curl -fsS --max-time 1 "${HEADROOM_CACHE_HEALTH_URL:-http://127.0.0.1:8787/cache-health}" 2>/dev/null || true)
[ -z "$health" ] && exit 0

cache_pct=$(printf '%s' "$health" | jq -r 'if .recent_hit_rate == null then empty else (.recent_hit_rate * 100 | floor) end')
[ -z "$cache_pct" ] && cache_pct="?"

# What share of the tokens written to the cache bought new cached ground, as
# against re-covering ground the conversation already held. Writes only: reads
# outnumber them about fifty to one and would pin this near 100% forever.
# Rounds down, so a "99%" here has really cleared 99%.
prod_pct=$(printf '%s' "$health" | jq -r 'if .productive_write_pct == null then empty else (.productive_write_pct | floor) end')

# Pre-filter before parsing: three event names out of ~17 lines a turn keeps
# this at a few hundred lines of JSON per statusline render.
cr=$(tail -n "$WINDOW" "$LOG" 2>/dev/null \
    | grep -E 'turn_cost_ledger|no_previous_turn|headroom-proxy starting' \
    | python3 -c '
import json, sys

lines = sys.stdin.readlines()
# Every counter resets on restart, so never average across one.
for i in range(len(lines) - 1, -1, -1):
    if "headroom-proxy starting" in lines[i]:
        lines = lines[i + 1:]
        break

turn = {}
cold = set()
for line in lines:
    try:
        entry = json.loads(line)
    except ValueError:
        continue
    f = entry.get("fields", {})
    rid = f.get("request_id")
    event = f.get("event")
    if event == "turn_cost_ledger":
        # Turns on a translated route record no client bytes and never reach
        # Anthropic cache. They would add to the numerator and nothing else.
        if f.get("client_request_bytes"):
            turn[rid] = (f.get("cache_creation_input_tokens") or 0,
                         f.get("cache_read_input_tokens") or 0,
                         f.get("input_tokens") or 0,
                         f.get("billed_fresh_equivalents") or 0)
    elif event == "prefix_replay_not_replayed" and "no_previous_turn" in str(f.get("reason", "")):
        # The first turn of a conversation has no prefix to read and writes the
        # whole thing. That cost scales with how many conversations start, not
        # with anything the proxy controls, and it is large enough to invert the
        # verdict — so it gets its own number rather than polluting the steady one.
        cold.add(rid)

if not turn:
    sys.exit(0)

def ratio(items):
    create = sum(t[0] for t in items)
    read = sum(t[1] for t in items)
    return create / read if read else 0.0

# What share of the bill never touched cache. c/r cannot see this: uncached
# input is neither creation nor read, so a prefix that re-sends the same bytes
# fresh every turn leaves c/r looking healthy while it owns most of the bill.
# Measured 2026-08-16 — c/r 0.033 with 61% of billed weight uncached.
def uncached_share(items):
    fresh = sum(t[2] for t in items)
    billed = sum(t[3] for t in items)
    return fresh / billed if billed else None

warm = [v for rid, v in turn.items() if rid not in cold]
share = uncached_share(warm if warm else list(turn.values()))
# Double quotes only below: this whole program is a single-quoted bash string,
# so one apostrophe would end it and truncate the script.
share_txt = "-" if share is None else str(round(share * 100))

# crude used to sum every turn since the last restart, so one huge cold turn
# from hours ago would swamp the total and the number would look frozen as
# new turns landed. Cap it to the most recent turns (dict is insertion-ordered)
# so it stays as responsive as steady while still including cold turns.
CRUDE_RECENT = 20
recent_all = list(turn.values())[-CRUDE_RECENT:]

print(f"{ratio(warm) if warm else 0.0:.3f} {ratio(recent_all):.3f} "
      f"{share_txt}")
' 2>/dev/null)

# How much of the subscription is gone. This replaced billed-weight-per-hour,
# which measured the proxy rather than the limit: it took a token count the
# proxy computes itself and divided by wall clock, so it moved with how hard
# the session was being driven and never said how close the wall was.
# Anthropic reports the windows directly, so use its accounting, not ours.
usage=$(curl -fsS --max-time 1 "${HEADROOM_METRICS_URL:-http://127.0.0.1:8787/metrics}" 2>/dev/null \
    | grep -E '^proxy_ratelimit_unified_(utilization|reset_seconds)\{window="(5h|7d)"\}' \
    | python3 -c '
import re, sys, time

# window -> {utilization, reset epoch}
w = {}
for line in sys.stdin:
    m = re.match(r"proxy_ratelimit_unified_(\w+)\{window=\"(\w+)\"\}\s+(\S+)", line)
    if m:
        w.setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))

SPAN = {"5h": 5 * 3600, "7d": 7 * 86400}
now = time.time()
out = []
for name in ("5h", "7d"):
    d = w.get(name) or {}
    util, reset = d.get("utilization"), d.get("reset_seconds")
    if util is None or not reset:
        continue
    text = f"{name} {util * 100:.0f}%"
    # Pace: consumed share over elapsed share. Projecting the total at reset
    # is the actionable half — 62% used means nothing without knowing whether
    # the window is nearly over or barely begun. Skip it early in a window,
    # where a small divisor makes the projection meaningless.
    elapsed = 1 - (reset - now) / SPAN[name]
    if 0.15 <= elapsed <= 1:
        out.append(f"{text}→{util / elapsed * 100:.0f}%")
    else:
        out.append(text)
print(" · ".join(out))
' 2>/dev/null)

line="cache ✓ ${cache_pct}%"
# Older proxies do not publish it; the segment stays byte-identical there.
[ -n "$prod_pct" ] && line="$line | prod ${prod_pct}%"
if [ -n "$cr" ]; then
    read -r steady crude _uncached <<<"$cr"
    line="$line | c/r ${steady} steady, ${crude} crude"
fi
# Last, because it is the one number that is not about the proxy at all: the
# three above say how well the cache is working, this says how much of the
# subscription is left to work with.
[ -n "$usage" ] && line="$line | $usage"
printf '%s\n' "$line"
