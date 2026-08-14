#!/usr/bin/env python3
"""Reproduce the figures in docs/TODO-proxy-observation-2026-08-08.md.

Every number in that document came from one of the checks below, run against
a live proxy log. The log is JSON lines and is never rotated, so each check
must be scoped to a single process run -- pass --since with the process start
timestamp, or the counts will span months and many restarts.

Usage:
    python scripts/proxy_log_audit.py all --log ~/headroom-proxy.log \
        --since 2026-08-08T10:33:08

    python scripts/proxy_log_audit.py recache --log ~/headroom-proxy.log \
        --since 2026-08-08T21:20:00

Each subcommand prints derived counts only, never raw log lines.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

# PERF lines pack their numbers into the message string rather than into
# structured fields, so they have to be parsed back out.
PERF_KV = re.compile(r"(\w+)=(-?[\d.]+)")
PERF_ID = re.compile(r"^\[([0-9a-f-]{36})\] PERF")


def load(log: Path, since: str):
    """Yield (timestamp, level, fields) for lines at or after `since`."""
    bad = 0
    with log.open(errors="replace") as fh:
        for line in fh:
            try:
                d = json.loads(line)
            except ValueError:
                bad += 1
                continue
            ts = d.get("timestamp", "")
            if since and ts < since:
                continue
            yield ts, d.get("level", ""), d.get("fields", {})
    if bad:
        print(f"  ({bad} unparseable lines skipped)", file=sys.stderr)


def perf_rows(log: Path, since: str):
    """Yield (timestamp, request_id, {field: float}) for every PERF line."""
    for ts, _lvl, f in load(log, since):
        msg = str(f.get("message", ""))
        if " PERF " not in msg:
            continue
        m = PERF_ID.match(msg)
        kv = {k: float(v) for k, v in PERF_KV.findall(msg)}
        model = re.search(r"model=(\S+)", msg)
        if model:
            kv["_model"] = model.group(1)
        yield ts, (m.group(1) if m else ""), kv


# --- item 1: tok_after goes negative -----------------------------------------


def check_negative_tokens(log: Path, since: str) -> None:
    """Item 1/1a: turns where tok_after < 0, split by transform."""
    total = 0
    neg = 0
    sigs = Counter()
    by_transform = defaultdict(lambda: [0, 0])  # name -> [negative, total]

    # The transform name lives on the "compression applied" line as
    # `strategies`, keyed by request_id; PERF lines don't carry it.
    transform = {}
    for _ts, _lvl, f in load(log, since):
        rid = f.get("request_id")
        names = f.get("strategies") or f.get("live_zone_strategies")
        if rid and names:
            transform[rid] = ",".join(re.findall(r"\w+_\w+", str(names))) or str(names)

    for _ts, rid, kv in perf_rows(log, since):
        if "tok_after" not in kv:
            continue
        total += 1
        name = transform.get(rid, "unknown")
        by_transform[name][1] += 1
        if kv["tok_after"] < 0:
            neg += 1
            by_transform[name][0] += 1
            sigs[(kv.get("tok_before"), kv.get("tok_saved"))] += 1

    pct = 100.0 * neg / total if total else 0.0
    print(f"negative tok_after: {neg}/{total} turns ({pct:.1f}%)")
    print(f"distinct (tok_before, tok_saved) signatures: {len(sigs)}")
    for sig, n in sigs.most_common(8):
        print(f"  before={sig[0]} saved={sig[1]}: x{n}")
    print("by transform (negative/total):")
    for name, (bad, tot) in sorted(by_transform.items(), key=lambda x: -x[1][0]):
        share = 100.0 * bad / tot if tot else 0.0
        print(f"  {name}: {bad}/{tot} ({share:.1f}%)")


def check_saved_monotonic(log: Path, since: str) -> None:
    """Item 1c: PERF `tok_saved` is cumulative while `tokens_freed` is per-turn.

    Comparing consecutive PERF rows cannot settle this -- PERF lines carry no
    conversation key, so any time-ordered sequence interleaves conversations.
    The decisive test needs no key: for a single request_id the "compression
    applied" line reports that turn's own `tokens_freed`, and the PERF line
    reports `tok_saved`. If the two disagree, and tok_saved is the larger,
    PERF is reporting a running total.
    """
    freed = {}
    for _ts, _lvl, f in load(log, since):
        rid = f.get("request_id")
        if rid and f.get("tokens_freed") is not None:
            freed[rid] = (
                float(f["tokens_freed"]),
                float(f.get("tokens_before", 0) or 0),
            )

    agree = disagree = higher = 0
    examples = []
    for _ts, rid, kv in perf_rows(log, since):
        if rid not in freed or "tok_saved" not in kv:
            continue
        per_turn, turn_before = freed[rid]
        if kv["tok_saved"] == per_turn:
            agree += 1
        else:
            disagree += 1
            if kv["tok_saved"] > per_turn:
                higher += 1
            if len(examples) < 6:
                examples.append(
                    f"    {rid[:8]}: this turn freed {per_turn:.0f} of {turn_before:.0f},"
                    f" PERF reported tok_saved={kv['tok_saved']:.0f}"
                )

    total = agree + disagree
    print(f"requests with both lines: {total}")
    print(f"  tok_saved == this turn's tokens_freed: {agree}")
    print(f"  disagree: {disagree} (of which tok_saved is larger: {higher})")
    print("\n".join(examples))
    if disagree and higher == disagree:
        print("  every disagreement is upward: PERF is reporting a running total")


# --- item 3 / 11: recache waste ----------------------------------------------


def check_recache(log: Path, since: str) -> None:
    """Items 3, 3a, 11: waste by conversation, and the alternating-stream test."""
    rows = []
    for ts, _lvl, f in load(log, since):
        if f.get("event") != "cache_recache_observed":
            continue
        rows.append(
            (
                ts,
                f.get("conversation_key"),
                str(f.get("drift_dims", "")),
                int(f.get("wasted_tokens", 0) or 0),
                int(f.get("expected_cache_read", 0) or 0),
                int(f.get("actual_cache_read", 0) or 0),
            )
        )

    expected = sum(r[3] for r in rows if not r[2])
    drift = [r for r in rows if r[2]]
    print(f"recache events: {len(rows)} ({len(drift)} with drift_dims)")
    print(f"  tokens booked as expected (empty dims, excluded): {expected}")
    print(f"  tokens booked as waste: {sum(r[3] for r in drift)}")

    by_conv = defaultdict(int)
    for r in drift:
        by_conv[r[1]] += r[3]
    print(f"  distinct conversations: {len(by_conv)}")
    for key, tot in sorted(by_conv.items(), key=lambda x: -x[1])[:6]:
        print(f"    {key}: {tot} tokens")

    print("  drift dimensions:")
    for dims, n in Counter(r[2] for r in drift).most_common():
        print(f"    {dims}: {n}")

    # Item 11: a stream whose actual_cache_read does not grow with the
    # conversation is matching only the leading system+tools block.
    pinned = Counter()
    for r in drift:
        pinned[(r[1], r[5])] += 1
    repeats = [(k, n) for k, n in pinned.items() if n >= 3]
    if repeats:
        print("  identical actual_cache_read repeated within one conversation:")
        for (key, acr), n in sorted(repeats, key=lambda x: -x[1]):
            print(f"    {key}: actual_cache_read={acr} seen x{n}")
        print("  (repeated constant while the conversation grows = item 11)")


def check_cold_starts(log: Path, since: str, window_s: int = 120) -> None:
    """Item 3c: how much 'waste' sits within `window_s` of a cold start."""
    firsts = []
    drift = []
    for ts, _lvl, f in load(log, since):
        ev = f.get("event")
        if ev == "cache_drift_first_request":
            firsts.append(ts)
        elif ev == "cache_recache_observed" and str(f.get("drift_dims", "")):
            drift.append((ts, int(f.get("wasted_tokens", 0) or 0)))

    def secs(a: str, b: str) -> float:
        fmt = "%Y-%m-%dT%H:%M:%S"
        return abs(time.mktime(time.strptime(a[:19], fmt)) - time.mktime(time.strptime(b[:19], fmt)))

    near = [d for d in drift if any(secs(d[0], ts) <= window_s for ts in firsts)]
    tot = sum(d[1] for d in drift)
    near_tot = sum(d[1] for d in near)
    share = 100.0 * near_tot / tot if tot else 0.0
    print(f"drift events: {len(drift)}, of which within {window_s}s of a cold start: {len(near)}")
    print(f"waste: {tot} total, {near_tot} near a cold start ({share:.0f}%)")


# --- item 4: volatile prefix warnings ----------------------------------------


def check_volatile(log: Path, since: str) -> None:
    """Item 4: how many volatile-prefix warnings fire on values that never change."""
    per_location = defaultdict(set)
    total = 0
    for _ts, _lvl, f in load(log, since):
        if "volatile content in cached prefix" not in str(f.get("message", "")):
            continue
        total += 1
        loc = str(f.get("location", ""))
        per_location[loc].add(str(f.get("sample", "")))

    static = {k: v for k, v in per_location.items() if len(v) == 1}
    print(f"volatile-prefix warnings: {total} across {len(per_location)} locations")
    print(f"  locations whose sample never varies: {len(static)}")
    for loc, samples in list(static.items())[:8]:
        print(f"    {loc} :: {next(iter(samples))}")


# --- item 6 / 7: retries and unbooked requests -------------------------------


def check_retries(log: Path, since: str) -> None:
    """Items 6 and 7: retry outcomes, and whether Retry-After is ever read."""
    delays = defaultdict(list)
    outcomes = Counter()
    saw_retry_after = 0
    for _ts, _lvl, f in load(log, since):
        msg = str(f.get("message", ""))
        if "retry_after" in f or "retry_after" in msg.lower():
            saw_retry_after += 1
        if "retrying" in msg:
            kind = str(f.get("error_type", "") or f.get("status", ""))
            d = f.get("delay_ms") or re.search(r"delay_ms=(\d+)", msg)
            if d is not None:
                delays[kind].append(int(d if not hasattr(d, "group") else d.group(1)))
        for word in ("EXHAUSTED", "dropped", "survived every retry"):
            if word in msg:
                outcomes[word] += 1

    for kind, ds in delays.items():
        print(f"  {kind}: {len(ds)} retries, delays {sorted(set(ds))[:10]}")
    print(f"  retry_after seen anywhere: {saw_retry_after} (0 means the header is ignored)")
    for word, n in outcomes.items():
        print(f"  {word}: {n}")


def check_unbooked(log: Path, since: str) -> None:
    """Item 9: requests forwarded upstream that never produce a PERF line."""
    forwarded = set()
    booked = set()
    for _ts, _lvl, f in load(log, since):
        rid = f.get("request_id")
        if not rid:
            continue
        msg = str(f.get("message", ""))
        if "forward" in msg or f.get("event") == "request_forwarded":
            forwarded.add(rid)
        if " PERF " in msg:
            booked.add(rid)
    for _ts, rid, _kv in perf_rows(log, since):
        if rid:
            booked.add(rid)
    missing = forwarded - booked
    pct = 100.0 * len(missing) / len(forwarded) if forwarded else 0.0
    print(f"forwarded: {len(forwarded)}, booked: {len(booked & forwarded)}")
    print(f"no completion record: {len(missing)} ({pct:.0f}%)")


# --- item 10: ledger pricing --------------------------------------------------


def check_ledger(events: Path) -> None:
    """Item 10: are saved tokens priced at fresh-input or cache-read rates?"""
    if not events.exists():
        print(f"  {events} not found")
        return
    rates = Counter()
    pairs = Counter()
    booked = 0
    with events.open(errors="replace") as fh:
        for line in fh:
            try:
                d = json.loads(line)
            except ValueError:
                continue
            saved = d.get("saved") or 0
            cost = d.get("cost_usd") or 0
            booked += saved
            if saved:
                rates[round(cost / saved * 1_000_000, 2)] += 1
            pairs[(d.get("before"), d.get("saved"))] += 1
    once = sum(s or 0 for _b, s in pairs)
    print(f"  events: {sum(pairs.values())}, distinct (before, saved) pairs: {len(pairs)}")
    print(f"  tokens booked: {booked}, counted once per distinct pair: {once}")
    if once:
        print(f"  repeat factor: {booked / once:.1f}x")
    print("  implied $/M tokens (fresh input is 15.00, cache read is 1.50):")
    for rate, n in rates.most_common(5):
        print(f"    ${rate}/M: {n} events")


# --- item 13: CCR store integrity ---------------------------------------------


def check_ccr(db: Path) -> None:
    """Item 13: are offloaded entries present, non-empty and unexpired?"""
    if not db.exists():
        print(f"  {db} not found")
        return
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    q = lambda sql: conn.execute(sql).fetchone()  # noqa: E731
    print(f"  rows: {q('select count(*) from ccr_entries')[0]}")
    print("  original length min/max/avg: %s/%s/%s" % q(
        "select min(length(original)),max(length(original)),cast(avg(length(original)) as int)"
        " from ccr_entries"
    ))
    empty = q("select count(*) from ccr_entries where original is null or original = ''")[0]
    print(f"  null or empty: {empty}")
    now = int(time.time())
    print(f"  expired: {q(f'select count(*) from ccr_entries where created_at+ttl_seconds < {now}')[0]}")
    print(f"  never read back: {q('select count(*) from ccr_entries where last_accessed<=created_at')[0]}")


CHECKS = {
    "negtokens": ("items 1/1a", lambda a: check_negative_tokens(a.log, a.since)),
    "cumulative": ("item 1c", lambda a: check_saved_monotonic(a.log, a.since)),
    "recache": ("items 3/3a/11", lambda a: check_recache(a.log, a.since)),
    "coldstart": ("item 3c", lambda a: check_cold_starts(a.log, a.since)),
    "volatile": ("item 4", lambda a: check_volatile(a.log, a.since)),
    "retries": ("items 6/7", lambda a: check_retries(a.log, a.since)),
    "unbooked": ("item 9", lambda a: check_unbooked(a.log, a.since)),
    "ledger": ("item 10", lambda a: check_ledger(a.events)),
    "ccr": ("item 13", lambda a: check_ccr(a.ccr)),
}


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("check", choices=["all", *CHECKS], help="which finding to reproduce")
    p.add_argument("--log", type=Path, default=Path.home() / "headroom-proxy.log")
    p.add_argument("--since", default="", help="ISO timestamp of process start; REQUIRED for honest counts")
    p.add_argument("--events", type=Path, default=Path.home() / ".headroom" / "savings_events.jsonl")
    p.add_argument("--ccr", type=Path, default=Path.home() / ".claude-work" / "context-mode" / "ccr.db")
    args = p.parse_args()

    if not args.since and args.check not in ("ledger", "ccr"):
        print("warning: no --since given; counts will span every restart in the log", file=sys.stderr)

    names = list(CHECKS) if args.check == "all" else [args.check]
    for name in names:
        label, fn = CHECKS[name]
        print(f"\n=== {name} ({label}) ===")
        fn(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
