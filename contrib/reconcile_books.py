#!/usr/bin/env python3
"""Reconcile the proxy's own token books against a number from outside it.

The proxy can only ever prove its buckets add up to its own total. A turn that
never reached an instrumented path is invisible to it by construction, so the
shortfall only shows up against something the proxy did not compute. Two such
numbers exist, and which one you have depends on how you authenticate:

  * API key (PAYG): Anthropic Console -> Usage -> export CSV. Pass it with
    --console-csv and this diffs it per model per day.
  * Subscription / OAuth: Console reports no usage for that traffic at all.
    What you get instead is Anthropic's unified utilization meter, returned on
    every response and logged as `unified_utilization_sample`. It is a
    percentage of a window rather than a token count, so it cannot give an
    absolute figure -- but tokens booked per point of utilization is a rate,
    and a window where that rate falls off is a window with traffic the books
    did not see.

Book side, joined on request id:
  * `turn_cost_ledger` -- the provider's own input/cache_read/cache_write,
    summed over every round the proxy ran. Authoritative, but carries no model.
  * the `PERF` line -- carries the model, and the output tokens for turns
    logged before the ledger carried them itself.
  * `stream_incomplete` -- turns dropped from the books, reported apart. Their
    counts are partial by definition, so they are a floor on the shortfall,
    never a subtraction from it.

Usage:
    reconcile_books.py --log ~/headroom-proxy.log --date 2026-09-07
    reconcile_books.py --log ~/headroom-proxy.log --date 2026-09-07 \
                       --console-csv ~/Downloads/anthropic-usage.csv
"""

import argparse
import collections
import csv
import json
import re
import sys

PERF = re.compile(
    r"\[(?P<rid>[0-9a-f-]{36})\] PERF model=(?P<model>\S+).*?"
    r"cache_read=(?P<cache_read>\d+) cache_write=(?P<cache_write>\d+).*?"
    r"tok_out=(?P<tok_out>\d+)"
)


def load(log_path, day):
    """Return (per-model book totals, dropped turns, join coverage, paths)."""
    models, ledger, seen = {}, {}, {}
    dropped = collections.Counter()
    util = []
    for line in open(log_path, errors="ignore"):
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if day and not rec.get("timestamp", "").startswith(day):
            continue
        f = rec.get("fields", {})
        msg = f.get("message", "")
        if (m := PERF.search(msg)) is not None:
            models[m["rid"]] = (m["model"], int(m["tok_out"]))
        event = f.get("event")
        if event == "stage_timings":
            # The widest per-request signal there is: every request that
            # reaches the pipeline logs one, booked or not. Without it the
            # denominator would be the books counting themselves.
            seen[f.get("request_id", "")] = f.get("path", "?")
        if event == "turn_cost_ledger":
            ledger[f.get("request_id", "")] = (
                int(f.get("input_tokens", 0)),
                int(f.get("cache_read_input_tokens", 0)),
                int(f.get("cache_creation_input_tokens", 0)),
                int(f.get("cache_write_1h_tokens", -1)),
                int(f.get("output_tokens", -1)),
            )
        elif event == "unified_utilization_sample":
            util.append(
                (
                    rec.get("timestamp", ""),
                    f.get("window", "?"),
                    float(f.get("utilization", 0) or 0),
                )
            )
        elif event == "stream_incomplete":
            dropped["turns"] += 1
            dropped["input"] += int(f.get("partial_input_tokens", 0))
            dropped["output"] += int(f.get("partial_output_tokens", 0))

    books = collections.defaultdict(collections.Counter)
    for rid, (inp, read, write, write_1h, out_billed) in ledger.items():
        model, out = models.get(rid, ("<no PERF line>", 0))
        # The ledger carries output itself now; the PERF line is the fallback
        # for turns logged before it did.
        if out_billed >= 0:
            out = out_billed
        row = books[model]
        row["turns"] += 1
        row["input"] += inp
        row["cache_read"] += read
        row["cache_write"] += write
        row["output"] += out
        if write_1h > 0:
            row["cache_write_1h"] += write_1h
    unjoined = sorted(set(models) - set(ledger))
    paths = collections.defaultdict(collections.Counter)
    for rid, path in seen.items():
        paths[path]["seen"] += 1
        if rid in ledger:
            paths[path]["booked"] += 1
    return books, dropped, unjoined, len(models), paths, util


def report_utilization(samples, books):
    """Anthropic's own meter, set beside the tokens the books recorded.

    Subscription traffic has no usage export, so this is the only external
    number available. It is a percentage, so the useful quantity is the rate:
    booked tokens per point of utilization. Compare the windows against each
    other -- one that consumed far more of the allowance per booked token had
    traffic the books did not see.
    """
    if not samples:
        print(
            "\nNO EXTERNAL ANCHOR\n"
            "  No `unified_utilization_sample` lines. Either this log predates\n"
            "  them or the traffic is API-key, in which case use --console-csv."
        )
        return
    billed = books["input"] + books["cache_read"] + books["cache_write"]
    print("\nAGAINST ANTHROPIC'S OWN METER (subscription windows)")
    by_window = collections.defaultdict(list)
    for _, window, value in samples:
        by_window[window].append(value)
    for window, values in sorted(by_window.items()):
        # Utilization resets at the window boundary, so summing the rises
        # counts consumption across however many windows the day spanned.
        climb = sum(
            max(0.0, b - a) for a, b in zip(values, values[1:])
        )
        if climb <= 0:
            continue
        print(
            f"  {window:<8}{climb * 100:>8.1f} points consumed"
            f"   {billed / (climb * 100):>14,.0f} booked tokens per point"
        )
    print(
        "  A point is worth what it is worth; the number to watch is whether it\n"
        "  holds steady day to day. A drop means the allowance went somewhere\n"
        "  the books cannot name."
    )


def read_console(path):
    """Sum an Anthropic Console usage export by model.

    Column names have changed before now, so match on substrings rather than
    an exact header, and say which column answered for what.
    """
    want = {
        "input": ("uncached input", "input tokens"),
        "cache_read": ("cache read",),
        "cache_write": ("cache creation", "cache write"),
        "output": ("output tokens",),
    }
    totals = collections.defaultdict(collections.Counter)
    with open(path, newline="") as fh:
        for row in csv.DictReader(fh):
            lower = {(k or "").strip().lower(): v for k, v in row.items()}
            model = next(
                (v for k, v in lower.items() if k in ("model", "model name")), "?"
            )
            for field, needles in want.items():
                for key, value in lower.items():
                    if any(n in key for n in needles):
                        try:
                            totals[model][field] += int(float(value or 0))
                        except ValueError:
                            pass
                        break
    return totals


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default="/home/ruben/headroom-proxy.log")
    ap.add_argument("--date", help="YYYY-MM-DD; omit for the whole log")
    ap.add_argument("--console-csv")
    args = ap.parse_args()

    books, dropped, unjoined, perf_turns, paths, util = load(args.log, args.date)
    if not books:
        sys.exit("no turn_cost_ledger lines for that date; nothing to reconcile")

    print(f"BOOKS {args.date or 'all'}  (source: turn_cost_ledger x PERF)")
    head = ("model", "turns", "input", "cache_read", "cache_write", "1h_write", "output")
    print("  {:<34}{:>7}{:>13}{:>13}{:>13}{:>11}{:>11}".format(*head))
    grand = collections.Counter()
    for model, row in sorted(books.items(), key=lambda kv: -kv[1]["cache_write"]):
        print(
            "  {:<34}{:>7}{:>13,}{:>13,}{:>13,}{:>11,}{:>11,}".format(
                model[:34], row["turns"], row["input"], row["cache_read"],
                row["cache_write"], row["cache_write_1h"], row["output"],
            )
        )
        grand.update(row)
    print(
        "  {:<34}{:>7}{:>13,}{:>13,}{:>13,}{:>11,}{:>11,}".format(
            "TOTAL", grand["turns"], grand["input"], grand["cache_read"],
            grand["cache_write"], grand["cache_write_1h"], grand["output"],
        )
    )

    print(f"\nNOT IN THE BOOKS")
    print(
        f"  dropped streams          {dropped['turns']:>7} turns"
        f"  {dropped['input']:>12,} input  {dropped['output']:>10,} output (partial: a floor)"
    )
    print(
        f"  turns with no ledger line{len(unjoined):>7} turns"
        f"   -- reached PERF but not the observer, out of {perf_turns:,} PERF turns"
    )

    print("\nCOVERAGE BY PATH  (booked / reached the pipeline)")
    for path, row in sorted(paths.items(), key=lambda kv: -kv[1]["seen"]):
        seen, booked = row["seen"], row["booked"]
        pct = booked * 100.0 / seen if seen else 0.0
        print(f"  {path[:40]:<40}{booked:>7} / {seen:<7}{pct:>7.1f}%")
    print(
        "  count_tokens and health checks bill nothing, so 0% there is right.\n"
        "  On the message paths the gap is the seam: dropped streams, upstream\n"
        "  rejections that billed nothing, and anything left over."
    )

    report_utilization(util, grand)

    if not args.console_csv:
        if not util:
            print(
                "\nNo --console-csv and no meter samples: this run is the proxy\n"
                "checking its own arithmetic and settles nothing about coverage."
            )
        return

    console = read_console(args.console_csv)
    print(f"\nAGAINST CONSOLE ({args.console_csv})")
    print("  {:<34}{:>14}{:>14}{:>14}".format("model / field", "console", "books", "shortfall"))
    for model in sorted(set(console) | set(books)):
        for field in ("input", "cache_read", "cache_write", "output"):
            c, b = console[model][field], books[model][field]
            if not c and not b:
                continue
            print(f"  {model[:26]:<26}{field:>8}{c:>14,}{b:>14,}{c - b:>14,}")
    print(
        "\nA positive shortfall is traffic the Console billed and the proxy\n"
        "never booked: the uninstrumented paths (Bedrock, Vertex, the Anthropic\n"
        "batch handler, the Cursor bridge) plus the dropped streams above."
    )


if __name__ == "__main__":
    main()
