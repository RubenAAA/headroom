#!/usr/bin/env python3
"""Does `--ctx-offload` pay for the retrievals and re-reads it causes?

Prices both sides of offload on the Anthropic path, in input-token equivalents
(tokens x price weight) and in hours the user waits, then sweeps
`--ctx-offload-min-bytes` to show where the balance turns.

Savings. Every tool_result over the threshold is sent as a digest on every
later turn until the session compacts. Each of those turns saves the block's
bytes minus the preview, priced as a cache read. That is a lower bound: a block
first converted in the live tail also skips one cache write. Built from the
transcripts, and checked against the proxy's own `ctx_offload_accounting`
total for the same window.

Costs, two kinds:
  retrieval  a `headroom_retrieve` round the proxy answers without the client
             seeing it (`ccr_continuation_upstream`). Priced as the parent
             turn's prompt read again from cache, plus the retrieved bytes
             written. Rates per block size come from joining
             `ccr_retrieval_call` hashes to `ccr.db`.
  re-read    the model reads the file again instead, a visible extra turn.
             Estimated from the jump in the same-file follow-up rate across
             the current threshold (just under: sent whole; just over: digest).
             The sweep assumes the same jump at every size above the cut.

Usage:
  scripts/offload-ledger.py                 # all ~/headroom-proxy.log*
  scripts/offload-ledger.py --weights subscription --sweep 2000,4000,8000,16000
"""

import argparse
import collections
import datetime as dt
import glob
import json
import os
import re
import sqlite3
import statistics
import sys

WEIGHTS = {
    # Anthropic list prices relative to one uncached input token, with the
    # 1h TTL the proxy forces. Subscription weights are the fitted ones in
    # docs/notes/learnings (write_1h 1.45, band 1.0-2.0).
    "api": {"read": 0.10, "write": 2.0, "input": 1.0, "output": 5.0},
    "subscription": {"read": 0.10, "write": 1.45, "input": 1.0, "output": 5.0},
}
BUCKETS = [0, 1000, 2000, 3000, 4000, 6000, 8000, 12000, 16000, 24000, 32000, 48000, 64000, 10**12]
FILE_READ = re.compile(r"^\s*(sed -n|cat|head|tail|nl|awk)\b")
PATH = re.compile(r"[\w./-]+\.(?:rs|py|ts|tsx|js|md|toml|go|sh|json|yaml|yml|prisma|sql)\b")


def bucket(n):
    for i in range(len(BUCKETS) - 1):
        if BUCKETS[i] <= n < BUCKETS[i + 1]:
            return i
    return len(BUCKETS) - 2


def label(i):
    hi = BUCKETS[i + 1]
    return f"{BUCKETS[i] // 1000}-{hi // 1000}K" if hi < 10**12 else f"{BUCKETS[i] // 1000}K+"


def ts(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def read_log(paths):
    """Only the events the ledger needs, with a substring gate before JSON."""
    perf, saved, rounds, calls = {}, {}, [], []
    perf_re = re.compile(
        r"\[([0-9a-f-]{36})\] PERF model=(\S+) .*?cache_read=(\d+) cache_write=(\d+)"
    )
    first = None
    for p in paths:
        with open(p, errors="ignore") as fh:
            for line in fh:
                if "PERF model=" in line:
                    m = perf_re.search(line)
                    if m:
                        perf[m[1]] = (m[2], int(m[3]) + int(m[4]))
                elif '"ctx_offload_accounting"' in line or '"ccr_continuation_upstream"' in line \
                        or '"ccr_retrieval_call"' in line:
                    try:
                        d = json.loads(line)
                    except ValueError:
                        continue
                    # Non-tracing lines (restart-script notes, panics) share
                    # the file; one of them must not end the run.
                    f = d.get("fields") if isinstance(d, dict) else None
                    if not isinstance(f, dict) or "timestamp" not in d or "request_id" not in f:
                        continue
                    t = ts(d["timestamp"])
                    first = t if first is None else min(first, t)
                    ev = f.get("event")
                    if ev == "ctx_offload_accounting":
                        saved[f["request_id"]] = (t, f.get("tokens_saved", 0))
                    elif ev == "ccr_continuation_upstream":
                        rounds.append((t, f["request_id"], f.get("provider"), f.get("upstream_ms", 0)))
                    elif ev == "ccr_retrieval_call" and f.get("hash"):
                        calls.append((t, f["request_id"], f["hash"]))
    return first, perf, saved, rounds, calls


def read_store(path, since):
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    newest = con.execute("select max(created_at) from ccr_entries").fetchone()[0] or 0
    scale = 1000 if newest > 1e12 else 1
    rows = con.execute(
        "select hash, length(original) from ccr_entries where created_at >= ?",
        (int(since * scale),),
    )
    return dict(rows)


def read_transcripts(pattern, since, w):
    """Per tool_result: size, later turns until compaction, whether it was a
    file read, and whether the next two calls went back to the same file.
    Also every turn's priced cost and model latency, for the extra-turn price."""
    blocks, turn_cost, latency = [], [], []
    for fn in glob.glob(os.path.expanduser(pattern), recursive=True):
        if os.path.getmtime(fn) < since:
            continue
        calls, sizes, turn_at_call, turns, seen, family = [], {}, {}, [], set(), collections.Counter()
        segment = 0
        last_user = None
        with open(fn, errors="ignore") as fh:
            for line in fh:
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                if d.get("subtype") == "compact_boundary":
                    segment += 1
                m = d.get("message")
                t = d.get("timestamp")
                if not isinstance(m, dict) or not t or ts(t) < since:
                    continue
                if m.get("role") == "user":
                    last_user = ts(t)
                if m.get("role") == "assistant" and m.get("model", "<")[0] != "<":
                    family[m["model"]] += 1
                    mid = m.get("id")
                    if mid not in seen:
                        seen.add(mid)
                        turns.append(segment)
                        u = m.get("usage") or {}
                        turn_cost.append(
                            u.get("cache_read_input_tokens", 0) * w["read"]
                            + u.get("cache_creation_input_tokens", 0) * w["write"]
                            + u.get("input_tokens", 0) * w["input"]
                            + u.get("output_tokens", 0) * w["output"]
                        )
                        if last_user:
                            latency.append(ts(t) - last_user)
                            last_user = None
                for b in m.get("content") or []:
                    if not isinstance(b, dict):
                        continue
                    if b.get("type") == "tool_use":
                        i = b.get("input") or {}
                        if b["name"] == "Read":
                            files, is_read = {os.path.basename(i.get("file_path", ""))}, True
                        elif b["name"] == "Bash":
                            cmd = i.get("command", "")
                            files = {os.path.basename(p) for p in PATH.findall(cmd)}
                            is_read = bool(FILE_READ.search(cmd)) and bool(files)
                        else:
                            files, is_read = set(), False
                        calls.append((b["id"], files, is_read))
                        turn_at_call[b["id"]] = (len(turns), segment)
                    elif b.get("type") == "tool_result":
                        c = b.get("content")
                        if isinstance(c, list):
                            c = "".join(x.get("text", "") for x in c if isinstance(x, dict))
                        sizes[b.get("tool_use_id")] = len(str(c or ""))
        if not family or not family.most_common(1)[0][0].startswith("claude-"):
            continue
        for k, (cid, files, is_read) in enumerate(calls):
            if cid not in sizes or cid not in turn_at_call:
                continue
            at, seg = turn_at_call[cid]
            later = sum(1 for s in turns[at:] if s == seg)
            back = bool(files) and any(files & f for _, f, _ in calls[k + 1 : k + 3])
            blocks.append((sizes[cid], later, is_read, back))
    return blocks, turn_cost, latency


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--log", default="~/headroom-proxy.log*")
    ap.add_argument("--store", default="~/.claude-work/context-mode/ccr.db")
    ap.add_argument("--transcripts", default="~/.claude*/projects/**/*.jsonl")
    ap.add_argument("--weights", choices=WEIGHTS, default="api")
    ap.add_argument("--min-bytes", type=int, default=20000,
                    help="the --ctx-offload-min-bytes live during the log window "
                         "(20000 since 29a91202; pass 2000 for logs before it)")
    ap.add_argument("--preview-bytes", type=int, default=700, help="digest plus preview left in place")
    ap.add_argument("--sweep", default="1000,2000,3000,4000,6000,8000,12000,16000,32000,64000")
    a = ap.parse_args()
    w = WEIGHTS[a.weights]

    logs = sorted(glob.glob(os.path.expanduser(a.log)))
    first, perf, saved, rounds, calls = read_log(logs)
    if first is None:
        sys.exit("no offload or retrieval events in " + a.log)
    store = read_store(os.path.expanduser(a.store), first)
    blocks, turn_cost, latency = read_transcripts(a.transcripts, first, w)
    days = (max(t for t, *_ in rounds + calls) - first) / 86400 if rounds or calls else 0
    anth = lambda rid: perf.get(rid, ("",))[0].startswith("claude-") and "spark" not in perf[rid][0]

    # Realised ledger, Anthropic path only (Zen is free).
    save_tok = sum(v for rid, (_, v) in saved.items() if anth(rid))
    save_eq = save_tok * w["read"]
    ret_rounds = [(rid, ms) for _, rid, prov, ms in rounds if prov == "anthropic"]
    ret_calls = [(rid, h) for _, rid, h in calls if anth(rid)]
    ret_eq = sum(perf.get(rid, ("", 0))[1] * w["read"] for rid, _ in ret_rounds)
    ret_eq += sum(store.get(h, 0) / 4 * w["write"] for _, h in ret_calls)
    ret_hours = sum(ms for _, ms in ret_rounds) / 3.6e6

    T = a.min_bytes
    lo = [b for b in blocks if b[2] and 0.5 * T <= b[0] < 0.95 * T]
    hi = [b for b in blocks if b[2] and 1.05 * T <= b[0] < 1.5 * T]
    rate = lambda xs: sum(b[3] for b in xs) / len(xs) if xs else 0.0
    excess = max(0.0, rate(hi) - rate(lo))
    turn_eq = statistics.median(turn_cost) if turn_cost else 0.0
    turn_s = statistics.median(latency) if latency else 0.0
    offloaded_reads = sum(1 for b in blocks if b[2] and b[0] > T)
    reread_eq = excess * offloaded_reads * turn_eq
    reread_hours = excess * offloaded_reads * turn_s / 3600

    model_save = sum(max(0, s - a.preview_bytes) / 4 * later for s, later, _, _ in blocks if s > T) * w["read"]

    print(f"window {dt.datetime.fromtimestamp(first, dt.timezone.utc):%Y-%m-%d %H:%M}Z, {days:.1f} days, "
          f"weights={a.weights}, min-bytes={T}\n")
    print("Realised, Anthropic path (input-token equivalents):")
    print(f"  saved     {save_eq:>14,.0f}   proxy tokens_saved x read; transcript model gives {model_save:,.0f}")
    print(f"  retrieval {ret_eq:>14,.0f}   {len(ret_rounds)} hidden rounds, {len(ret_calls)} calls, "
          f"{ret_hours:.1f} h of waiting")
    print(f"  re-read   {reread_eq:>14,.0f}   {excess:.1%} excess x {offloaded_reads} offloaded file reads x "
          f"{turn_eq:,.0f}/turn; {reread_hours:.1f} h")
    print(f"            (follow-up rate {rate(lo):.1%} of {len(lo)} just under the cut, "
          f"{rate(hi):.1%} of {len(hi)} just over)")
    net = save_eq - ret_eq - reread_eq
    print(f"  net       {net:>14,.0f}   {'offload pays' if net > 0 else 'offload LOSES'}; "
          f"costs are {(ret_eq + reread_eq) / max(1, save_eq):.0%} of savings\n")

    # Per-size retrieval rate: rounds per stored block, Anthropic calls only.
    per_hash = collections.Counter(h for _, h in ret_calls)
    got = collections.Counter()
    for h, n in per_hash.items():
        if h in store:
            got[bucket(store[h])] += n
    n_blocks = collections.Counter(bucket(s) for s, *_ in blocks if s > T)
    cut = bucket(T + 1)
    rpb = {i: got[i] / n_blocks[i] for i in n_blocks if i >= cut and n_blocks[i]}
    round_eq = ret_eq / max(1, len(ret_calls))
    print("Retrieval calls per offloaded block, by size:")
    for i in sorted(rpb):
        print(f"  {label(i):>8}  {rpb[i]:5.2f}  ({got[i]} calls / {n_blocks[i]} blocks)")

    print(f"\nSweep ({round_eq:,.0f} per retrieval call, {turn_eq:,.0f} per extra turn, {turn_s:.0f} s per turn):")
    print(f"  {'min-bytes':>9} {'saved':>14} {'retrieval':>12} {'re-read':>12} {'net':>14} {'wait h':>7}")
    floor = rpb[min(rpb)] if rpb else 0.0
    best = (float("-inf"), None)
    for t in [int(x) for x in a.sweep.split(",")]:
        sel = [b for b in blocks if b[0] > t]
        s_eq = sum(max(0, s - a.preview_bytes) / 4 * later for s, later, _, _ in sel) * w["read"]
        calls_t = sum(rpb.get(bucket(s), floor) for s, *_ in sel)
        reads_t = sum(1 for b in sel if b[2])
        r_eq, x_eq = calls_t * round_eq, excess * reads_t * turn_eq
        hours = (calls_t * (ret_hours * 3600 / max(1, len(ret_calls))) + excess * reads_t * turn_s) / 3600
        net_t = s_eq - r_eq - x_eq
        mark = " extrapolated" if bucket(t + 1) < cut else ""
        print(f"  {t:>9} {s_eq:>14,.0f} {r_eq:>12,.0f} {x_eq:>12,.0f} {net_t:>14,.0f} {hours:>7.1f}{mark}")
        best = max(best, (net_t, t))
    print(f"\nBest threshold by net: {best[1]}. Rows marked extrapolated reuse the retrieval rate of the "
          "smallest measured size; the re-read jump is assumed the same at every size.")


if __name__ == "__main__":
    main()
