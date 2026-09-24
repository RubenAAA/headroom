#!/usr/bin/env python3
"""Mine volatile spans for the relocation idea (harness-volatile-relocation.md).

Ranks `volatile_content_detected` locations (confirmed changes only, never
shape suspicions) by co-occurring `cache_recache_observed` waste on the same
conversation_key. Co-occurrence, not causation: a location that fires where
waste happens is a relocation candidate to investigate, not a proven cause.

Usage:
    python3 scripts/volatile_relocation_mine.py ~/headroom-proxy.log* [--min-waste 1000]

Reads JSON log lines from each file (glob-expanded by the shell or not —
both work), joins on conversation_key, prints a ranked table. Stdlib only.
"""

import glob
import json
import sys
from collections import defaultdict


def iter_files(patterns):
    seen = set()
    for pat in patterns:
        for path in sorted(glob.glob(pat)):
            if path not in seen:
                seen.add(path)
                yield path


def main(argv):
    patterns = [a for a in argv if not a.startswith("--")]
    if not patterns:
        print(__doc__)
        return 2
    min_waste = 0
    for a in argv:
        if a.startswith("--min-waste="):
            min_waste = int(a.split("=", 1)[1])
        elif a == "--min-waste" and argv.index(a) + 1 < len(argv):
            min_waste = int(argv[argv.index(a) + 1])

    # location -> {kind, fires, convs:set}
    locs = {}
    # conv -> {recache_events, wasted}
    recache = defaultdict(lambda: [0, 0])
    total_waste = 0
    total_recache = 0
    lines = 0

    for path in iter_files(patterns):
        try:
            fh = open(path, errors="replace")
        except OSError as e:
            print(f"skip {path}: {e}", file=sys.stderr)
            continue
        with fh:
            for line in fh:
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                lines += 1
                f = e.get("fields", e)
                ev = f.get("event")
                if ev == "volatile_content_detected":
                    loc = f.get("location", "?")
                    conv = f.get("conversation_key", "")
                    rec = locs.setdefault(
                        loc, {"kind": f.get("kind", "?"), "fires": 0, "convs": set()}
                    )
                    rec["fires"] += 1
                    if conv:
                        rec["convs"].add(conv)
                elif ev == "cache_recache_observed":
                    conv = f.get("conversation_key", "")
                    try:
                        w = int(f.get("wasted_tokens", 0))
                    except (TypeError, ValueError):
                        w = 0
                    if conv:
                        recache[conv][0] += 1
                        recache[conv][1] += w
                    total_recache += 1
                    total_waste += w

    print(f"lines: {lines}  recache events: {total_recache}  waste: {total_waste}")
    rows = []
    for loc, rec in locs.items():
        ev = sum(recache[c][0] for c in rec["convs"] if c in recache)
        w = sum(recache[c][1] for c in rec["convs"] if c in recache)
        rows.append((w, ev, rec["fires"], len(rec["convs"]), rec["kind"], loc))
    rows.sort(reverse=True)

    print(f"\n{'waste':>12} {'recache_ev':>10} {'fires':>7} {'convs':>7}  kind/location")
    print("-" * 90)
    shown = 0
    for w, ev, fires, nconvs, kind, loc in rows:
        if w < min_waste:
            continue
        share = (w / total_waste * 100.0) if total_waste else 0.0
        print(f"{w:>12} {ev:>10} {fires:>7} {nconvs:>7}  {kind}:{loc} ({share:.1f}% of waste)")
        shown += 1
        if shown >= 40:
            break
    if not shown:
        print("(no locations above the floor — volatile changes are not where the waste is)")
    print(
        "\nRead: locations are candidates, not causes. A top row earns a relocation"
        "\nprototype (one span per window, live A/B) only after confirming the span"
        "\nactually moves between turns — see volatile-shape-needs-change.md."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
