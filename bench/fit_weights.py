#!/usr/bin/env python3
"""Recover how the Claude subscription actually weights token classes.

Anthropic publishes the API weights (reads 0.1x, 5m writes 1.25x, 1h writes 2x)
but says nothing about how the 5h and 7d subscription windows meter usage. That
matters more than the API numbers if you are paying with a subscription, and one
possibility changes the whole optimisation target: on the API, cache reads do not
count toward rate limits at all. If the windows behave the same way, reads are
free rather than a tenth — and reads are over 90% of Claude Code's tokens.

The proxy already scrapes Anthropic's own meter
(`proxy_ratelimit_unified_utilization`), and the log already records each turn's
token classes. Sampling one against the other recovers the weights by
regression. No requests are sent; this reads meters that exist anyway.

    fit_weights.py sample [--interval 60] [--out ~/hr-usage-samples.jsonl]
    fit_weights.py fit [--window 5h] [--samples ...] [--log ...]

Run `sample` in the background for a few hours of ordinary work, then `fit`.
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import re
import sys
import time
import urllib.request

import numpy as np

METRICS_URL = os.environ.get("HEADROOM_METRICS_URL", "http://127.0.0.1:8787/metrics")
DEFAULT_SAMPLES = os.path.expanduser("~/hr-usage-samples.jsonl")
DEFAULT_LOG = os.path.expanduser("~/headroom-proxy.log")

# Order matters: it is the column order of the design matrix and of every
# reported coefficient.
CLASSES = ("input_tokens", "cache_read_input_tokens",
           "cache_write_5m_tokens", "cache_write_1h_tokens")
LABELS = ("fresh input", "cache read", "write 5m", "write 1h")
API_WEIGHTS = (1.0, 0.1, 1.25, 2.0)

GAUGE = re.compile(
    r'^proxy_ratelimit_unified_(utilization|reset_seconds)\{window="([^"]+)"\}\s+(\S+)')


# --- sampling ---------------------------------------------------------------


def read_gauges() -> dict:
    with urllib.request.urlopen(METRICS_URL, timeout=5) as response:
        text = response.read().decode("utf-8", "replace")
    out: dict[str, dict[str, float]] = {}
    for line in text.splitlines():
        m = GAUGE.match(line)
        if m and m.group(2) != "__init__":
            out.setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))
    return out


def sample(interval: int, path: str) -> int:
    """Append a timestamped utilization reading forever.

    Written as an append-only JSONL so a crash or restart costs one line, not
    the run. Duplicate consecutive readings are kept: a flat stretch is real
    evidence that nothing was consumed, and dropping it would bias the fit
    toward busy intervals.
    """
    print(f"sampling {METRICS_URL} every {interval}s -> {path}", file=sys.stderr)
    while True:
        try:
            row = {"ts": time.time(), "windows": read_gauges()}
            with open(path, "a") as handle:
                handle.write(json.dumps(row) + "\n")
        except Exception as exc:  # a transient scrape failure must not end the run
            print(f"scrape failed: {exc}", file=sys.stderr)
        time.sleep(interval)


# --- fitting ----------------------------------------------------------------


def load_samples(path: str, window: str):
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path) as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            w = (row.get("windows") or {}).get(window)
            if w and "utilization" in w:
                rows.append((row["ts"], w["utilization"], w.get("reset_seconds", 0)))
    rows.sort()
    return rows


def log_paths(path: str):
    """The live log plus its rotations.

    A sampling run of a few hours outlives a rotation easily, and reading only
    the live file drops every turn older than the last roll. That leaves the
    design matrix all zeros while the sample side still looks healthy.
    """
    return [path] + sorted(glob.glob(path + ".[0-9]*"))


def load_turns(paths):
    """(unix ts, token counts) for every booked turn across `paths`."""
    import datetime

    turns = []
    for path in paths:
        if not os.path.exists(path):
            continue
        with open(path, errors="replace") as handle:
            for line in handle:
                i = line.find("{")
                if i < 0:
                    continue
                try:
                    entry = json.loads(line[i:])
                except ValueError:
                    continue
                f = entry.get("fields", {})
                if f.get("event") != "turn_cost_ledger":
                    continue
                stamp = entry.get("timestamp")
                if not stamp:
                    continue
                ts = datetime.datetime.fromisoformat(
                    stamp.replace("Z", "+00:00")).timestamp()
                turns.append((ts, [float(f.get(c) or 0) for c in CLASSES]))
    turns.sort()
    return turns


def build_intervals(samples, turns):
    """Per-interval (token counts, utilization delta).

    Intervals that cross a window reset are dropped: utilization falls back to
    zero there, so the delta measures the reset rather than any consumption.
    """
    rows, deltas, dropped = [], [], 0
    times = np.array([t for t, _ in turns])
    counts = np.array([c for _, c in turns]) if turns else np.zeros((0, len(CLASSES)))
    for (t0, u0, r0), (t1, u1, r1) in zip(samples, samples[1:]):
        if r1 != r0 or u1 < u0:
            dropped += 1
            continue
        mask = (times > t0) & (times <= t1)
        if not mask.any():
            # A quiet interval is still information: zero tokens, zero delta.
            rows.append(np.zeros(len(CLASSES)))
        else:
            rows.append(counts[mask].sum(axis=0))
        deltas.append(u1 - u0)
    return np.array(rows), np.array(deltas), dropped


def r_squared(x, y):
    """R^2 of a through-origin fit of y on the single predictor x."""
    denom = float((x * x).sum())
    if denom <= 0:
        return float("-inf"), 0.0
    slope = float((x * y).sum()) / denom
    residual = y - slope * x
    total = float((y * y).sum())
    return (1 - float((residual * residual).sum()) / total if total else float("-inf")), slope


def fit(samples_path: str, log_path: str, window: str) -> int:
    samples = load_samples(samples_path, window)
    if len(samples) < 3:
        print(f"only {len(samples)} usable samples for window {window} — "
              f"run `fit_weights.py sample` for a few hours first")
        return 1
    paths = log_paths(log_path)
    turns = load_turns(paths)
    design, delta, dropped = build_intervals(samples, turns)

    span = (samples[-1][0] - samples[0][0]) / 3600
    booked = int((design.sum(axis=1) > 0).sum())
    print(f"window {window}: {len(samples)} samples over {span:.1f}h, "
          f"{len(delta)} intervals ({dropped} dropped at resets), "
          f"{len(turns)} turns in {len(paths)} log file(s), "
          f"{booked} intervals carrying turns")
    moved = int((delta > 0).sum())
    print(f"  intervals where utilization moved: {moved}")
    if booked == 0:
        stamp = lambda t: time.strftime("%Y-%m-%dT%H:%MZ", time.gmtime(t))
        print(f"\n  No turn lands inside the sampled span, so every predictor is "
              f"all zeros\n  and no fit is possible. Samples cover "
              f"{stamp(samples[0][0])}..{stamp(samples[-1][0])};")
        if turns:
            print(f"  the logs cover {stamp(turns[0][0])}..{stamp(turns[-1][0])}.")
        else:
            print(f"  no turn_cost_ledger line was found in {log_path} or its "
                  f"rotations.")
        print("  Sample and log have to run over the same hours — restart "
              "`sample`\n  and let it run alongside ordinary work.")
        return 1
    if moved < 8:
        print("\n  Not enough movement to fit anything. The meter reports two "
              "decimals,\n  so it only ticks after real work — keep sampling.")
        return 1

    # The decisive question first, and it needs no fitting: does the meter track
    # a predictor that counts cache reads, or one that ignores them? Comparing
    # R^2 of two fixed predictors is scale-free, so the unknown window size
    # cancels and coarse quantisation hurts both candidates equally.
    print(f"\n  {'predictor':<34} {'R^2':>8}")
    candidates = {
        "api weights (reads at 0.1x)": API_WEIGHTS,
        "reads free (0x), writes as api": (1.0, 0.0, 1.25, 2.0),
        "proxy definition (writes flat 1.25)": (1.0, 0.1, 1.25, 1.25),
        "every token equal": (1.0, 1.0, 1.0, 1.0),
        "reads only": (0.0, 1.0, 0.0, 0.0),
    }
    best = None
    for name, weights in candidates.items():
        score, slope = r_squared(design @ np.array(weights), delta)
        print(f"  {name:<34} {score:>8.3f}")
        if best is None or score > best[1]:
            best = (name, score, slope)

    # Then the open fit. Non-negative least squares would be ideal; with four
    # columns a coarse grid is honest about how little the data can resolve.
    grid = np.arange(0.0, 2.05, 0.05)
    top = []
    for read in np.arange(0.0, 0.31, 0.01):
        for w1 in grid[grid >= 1.0]:
            weights = np.array([1.0, read, 1.25, w1])
            score, _ = r_squared(design @ weights, delta)
            top.append((score, read, w1))
    top.sort(reverse=True)
    score, read, w1 = top[0]
    print(f"\n  best free fit (fresh pinned at 1.0, 5m write at 1.25):")
    print(f"    cache read {read:.2f}x, 1h write {w1:.2f}x, R^2 {score:.3f}")

    # Anything within a whisker of the best is indistinguishable given the
    # meter's resolution, so report the range rather than a false point estimate.
    close = [(r, w) for s, r, w in top if s > score - 0.01]
    if close:
        print(f"    indistinguishable at this resolution: "
              f"read {min(r for r, _ in close):.2f}-{max(r for r, _ in close):.2f}x, "
              f"1h write {min(w for _, w in close):.2f}-{max(w for _, w in close):.2f}x")

    # The slope inverts into something directly useful: how big the window is.
    name, _, slope = best
    if slope > 0:
        print(f"\n  implied {window} window size, priced by `{name}`: "
              f"{1 / slope / 1e6:.1f}M fresh-equivalent tokens")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("mode", choices=("sample", "fit"))
    parser.add_argument("--interval", type=int, default=60)
    parser.add_argument("--samples", default=DEFAULT_SAMPLES)
    parser.add_argument("--log", default=DEFAULT_LOG)
    parser.add_argument("--window", default="5h")
    args = parser.parse_args()
    if args.mode == "sample":
        return sample(args.interval, args.samples)
    return fit(args.samples, args.log, args.window)


if __name__ == "__main__":
    sys.exit(main())
