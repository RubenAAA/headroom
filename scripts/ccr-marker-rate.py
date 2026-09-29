#!/usr/bin/env python3
"""How often the model asks for compressed blocks back, per compressor.

Every compressed block that carries a `<<ccr:HASH>>` marker logs
`ccr_marker_offered` (hash, strategy, original and compressed tokens). Every
retrieval by hash logs `ccr_retrieval_call`. Joined on the hash, they give the
retrieval rate per strategy and the tokens the retrievals undo, so a new
compressor's markers can be judged against what they save.

Also reports the recache cost that hidden retrieval rounds leave behind
(`aftershock_of_continuation`), because that is what a retrieval costs beyond
the tokens it returns.

Read-only, aggregates only. Exit 0, or 1 when a strategy with at least
--min-offered markers is retrieved more often than --rate-limit, or when its
retrievals undo more tokens than the compression freed. Exit 2 without data.

    python3 scripts/ccr-marker-rate.py [--hours 24] [--log PATH ...]
    python3 scripts/ccr-marker-rate.py --self-test
"""

import argparse
import collections
import datetime
import json
import os
import sys
import tempfile

DEFAULT_LOGS = ["~/headroom-proxy.log.1", "~/headroom-proxy.log"]
KEYS = (
    "ccr_marker_offered",
    "ccr_retrieval_call",
    "ccr_content_not_found",
    "ccr_retrieve_unresolved",
    "ccr_continuation_usage",
    "aftershock_of_continuation",
)


def events(paths, cutoff):
    """Yield (event, fields) for the events this report reads, newest window only."""
    for path in paths:
        path = os.path.expanduser(path)
        if not os.path.exists(path):
            continue
        with open(path, errors="ignore") as fh:
            for line in fh:
                if not any(k in line for k in KEYS):
                    continue
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                if d.get("timestamp", "") < cutoff:
                    continue
                f = d.get("fields", d)
                name = f.get("event", "")
                if name == "cache_recache_observed" or "aftershock" in line:
                    if f.get("attribution_reason") == "aftershock_of_continuation":
                        yield "aftershock_of_continuation", f
                elif name in KEYS:
                    yield name, f


def analyse(evs):
    offered = {}
    retrieved = collections.Counter()  # hash -> retrieval calls
    other = collections.Counter()
    aftershock_tokens = 0
    for name, f in evs:
        if name == "ccr_marker_offered":
            offered.setdefault(f["hash"], f)
        elif name == "ccr_retrieval_call" and f.get("hash"):
            retrieved[f["hash"]] += 1
        elif name == "aftershock_of_continuation":
            other["aftershocks"] += 1
            aftershock_tokens += int(f.get("wasted_tokens") or 0)
        else:
            other[name] += 1
    per = collections.defaultdict(
        lambda: {"offered": 0, "freed": 0, "retrieved": 0, "calls": 0, "undone": 0}
    )
    for h, f in offered.items():
        s = per[f["strategy"]]
        s["offered"] += 1
        s["freed"] += int(f["original_tokens"]) - int(f["compressed_tokens"])
        if h in retrieved:
            s["retrieved"] += 1
            s["calls"] += retrieved[h]
            s["undone"] += int(f["original_tokens"])
    unmatched = sum(n for h, n in retrieved.items() if h not in offered)
    return per, unmatched, other, aftershock_tokens


def verdict(per, rate_limit, min_offered):
    flags = []
    for strategy, s in sorted(per.items()):
        if s["offered"] < min_offered:
            continue
        rate = s["retrieved"] / s["offered"]
        if rate > rate_limit:
            flags.append(f"{strategy}: retrieval rate {rate:.1%} > {rate_limit:.0%}")
        if s["undone"] > s["freed"]:
            flags.append(
                f"{strategy}: retrievals undo {s['undone']} tokens, compression freed {s['freed']}"
            )
    return flags


def report(per, unmatched, other, aftershock_tokens, hours, rate_limit, min_offered):
    print(f"window {hours}h")
    print(f"{'strategy':22} {'offered':>8} {'asked':>6} {'rate':>7} {'freed':>9} {'undone':>9} {'net':>9}")
    for strategy, s in sorted(per.items(), key=lambda kv: -kv[1]["offered"]):
        rate = s["retrieved"] / s["offered"] if s["offered"] else 0.0
        print(
            f"{strategy:22} {s['offered']:8d} {s['retrieved']:6d} {rate:7.1%} "
            f"{s['freed']:9d} {s['undone']:9d} {s['freed'] - s['undone']:9d}"
        )
    total = sum(s["offered"] for s in per.values())
    asked = sum(s["retrieved"] for s in per.values())
    print(f"all: {total} offered, {asked} asked ({asked / total:.1%})" if total else "all: none offered")
    print(
        f"retrieval calls for hashes not offered in the window: {unmatched} "
        "(older than the window, inline crusher markers, or unknown)"
    )
    print(
        f"lookups that failed: not_found={other['ccr_content_not_found']} "
        f"unresolved={other['ccr_retrieve_unresolved']}"
    )
    print(
        f"hidden-round turns: {other['ccr_continuation_usage']}; "
        f"aftershock recaches: {other['aftershocks']} costing {aftershock_tokens} tokens"
    )
    flags = verdict(per, rate_limit, min_offered)
    for flag in flags:
        print("FLAG", flag)
    print("verdict:", "REVIEW" if flags else "OK")
    return flags


def self_test():
    def offer(h, strategy, orig=1000, comp=300):
        return "ccr_marker_offered", {
            "hash": h, "strategy": strategy, "original_tokens": orig, "compressed_tokens": comp,
        }

    def ask(h):
        return "ccr_retrieval_call", {"hash": h}

    # Healthy: 1 of 20 asked back, freed far above undone.
    evs = [offer(f"a{i}", "smart_crusher") for i in range(20)] + [ask("a0"), ask("a0")]
    per, unmatched, _, _ = analyse(evs)
    assert per["smart_crusher"]["retrieved"] == 1 and per["smart_crusher"]["calls"] == 2
    assert not verdict(per, 0.25, 10)
    # Noisy: half asked back.
    evs = [offer(f"b{i}", "embedded_json") for i in range(20)] + [ask(f"b{i}") for i in range(10)]
    per, _, _, _ = analyse(evs)
    assert verdict(per, 0.25, 10)
    # Net negative: few asks but each undoes more than the block saved.
    evs = [offer(f"c{i}", "log_compressor", orig=1000, comp=990) for i in range(12)] + [ask("c0"), ask("c1")]
    per, _, _, _ = analyse(evs)
    assert any("undo" in f for f in verdict(per, 0.25, 10))
    # Too few offers to judge; unknown hash counted separately; a repeat offer counts once.
    evs = [offer("d0", "diff_compressor"), offer("d0", "diff_compressor"), ask("zz")]
    per, unmatched, _, _ = analyse(evs)
    assert per["diff_compressor"]["offered"] == 1 and unmatched == 1
    assert not verdict(per, 0.25, 10)

    # File path: the window cutoff and the aftershock branch.
    with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as fh:
        for ts, body in [
            ("2026-01-01T00:00:00Z", {"event": "ccr_marker_offered", "hash": "old", "strategy": "x",
                                       "original_tokens": 5, "compressed_tokens": 1}),
            ("2026-01-02T00:00:00Z", {"event": "ccr_marker_offered", "hash": "new", "strategy": "x",
                                       "original_tokens": 5, "compressed_tokens": 1}),
            ("2026-01-02T00:00:01Z", {"event": "cache_recache_observed",
                                       "attribution_reason": "aftershock_of_continuation",
                                       "wasted_tokens": 7}),
        ]:
            fh.write(json.dumps({"timestamp": ts, "fields": body}) + "\n")
        path = fh.name
    try:
        per, _, other, waste = analyse(events([path], "2026-01-01T12:00:00Z"))
    finally:
        os.unlink(path)
    assert per["x"]["offered"] == 1 and other["aftershocks"] == 1 and waste == 7
    print("self-test passed")


def main():
    ap = argparse.ArgumentParser(description=(__doc__ or "").split("\n\n")[0])
    ap.add_argument("--hours", type=float, default=24.0)
    ap.add_argument("--log", action="append", help="log path; repeatable")
    ap.add_argument("--rate-limit", type=float, default=0.25)
    ap.add_argument("--min-offered", type=int, default=20)
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    if a.self_test:
        self_test()
        return 0
    now = datetime.datetime.now(datetime.timezone.utc)
    cutoff = (now - datetime.timedelta(hours=a.hours)).strftime("%Y-%m-%dT%H:%M:%S")
    per, unmatched, other, waste = analyse(events(a.log or DEFAULT_LOGS, cutoff))
    if not per:
        print(
            "no ccr_marker_offered events in the window: the proxy is older than "
            "this event, or nothing was compressed with a marker"
        )
        return 2
    return 1 if report(per, unmatched, other, waste, a.hours, a.rate_limit, a.min_offered) else 0


if __name__ == "__main__":
    sys.exit(main())
