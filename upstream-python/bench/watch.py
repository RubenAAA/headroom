#!/usr/bin/env python3
"""Watch a live capture and emit one line per newly-found fault.

Built for the Monitor tool: every stdout line is an event, and the point is to
stay silent until something is worth reading. State lives in a small JSON file
so a fault is announced once, when it appears or when it gets materially worse,
rather than every pass.

    watch.py [--corpus DIR] [--interval 900] [--once]

It re-scores the whole corpus each pass, which is cheap and, more importantly,
never spends a token. Nothing here talks to the network.

Each fault carries the number that triggered it and where to look, so the
follow-up is a file to open rather than an investigation to start.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from cachesim import (  # noqa: E402
    API,
    DEFAULT_BYTES_PER_TOKEN,
    load_corpus,
    score_corpus,
    with_forwarded_bodies,
    _message_units,
    SIGNATURES,
)

DEFAULT_CORPUS = os.path.expanduser("~/headroom-capture-blindguard")
DEFAULT_STATE = os.path.expanduser("~/.headroom-bench-watch.json")

# A fault has to clear its threshold AND move by this much since it was last
# announced, or a metric hovering on the line would report forever.
RESTATE = 0.15

# Enough turns that a single cold conversation start cannot set the tone. Below
# this the corpus is reported as still filling and nothing else is judged.
MIN_TURNS = 40


def measure(corpus: str) -> dict:
    turns = load_corpus(corpus)
    forwarded = with_forwarded_bodies(turns, corpus)
    kept = {t.request_id for t in forwarded}
    client = [t for t in turns if t.request_id in kept]
    if len(client) < MIN_TURNS:
        return {"turns": len(client), "filling": True}

    cli_total, cli_each = score_corpus(client, DEFAULT_BYTES_PER_TOKEN)
    fwd_total, fwd_each = score_corpus(forwarded, DEFAULT_BYTES_PER_TOKEN)
    cli_billed = cli_total.billed_with(API)
    fwd_billed = fwd_total.billed_with(API)

    # Unexplained content change, counted the way `damage` counts it: a rewrite
    # that carries a feature's signature is deliberate and does not belong here.
    by_id = {t.request_id: t for t in client}
    unexplained, bloat = 0, []
    for turn in forwarded:
        origin = by_id.get(turn.request_id)
        if not origin:
            continue
        before, after = _message_units(origin.body), _message_units(turn.body)
        text = json.dumps(turn.body.get("messages"))
        signed = any(marker in text for marker in SIGNATURES)
        if len(before) != len(after) and not signed:
            unexplained += 1
        bloat.append(len(json.dumps(turn.body)) / max(len(json.dumps(origin.body)), 1))

    worst = max(
        ((f.billed_with(API) - by.billed_with(API), t.request_id)
         for (t, f), (_, by) in zip(fwd_each, cli_each)),
        default=(0, ""),
    )
    return {
        "turns": len(client),
        "ratio": fwd_billed / cli_billed if cli_billed else 1.0,
        "uncached": fwd_total.input_tokens / fwd_billed if fwd_billed else 0.0,
        "unexplained": unexplained / len(client),
        "bloat": sorted(bloat)[len(bloat) // 2] if bloat else 1.0,
        "worst_turn": worst[1],
        "worst_cost": worst[0],
        "disk_mb": sum(os.path.getsize(os.path.join(dp, f))
                       for dp, _, fs in os.walk(corpus) for f in fs) / 1e6,
    }


def faults(m: dict) -> list[tuple[str, float, str]]:
    """(key, level, message) for everything currently wrong."""
    out = []
    # Break-even is not a fault. The estimator is one bytes-per-token constant
    # and the client arm is itself shaped by the proxy's earlier offloads, so a
    # couple of percent either way is below what this can resolve.
    if m["ratio"] > 1.02:
        out.append(("ratio", m["ratio"],
                    f"the proxy costs {m['ratio']:.3f}x plain Claude Code on "
                    f"{m['turns']} turns — run `cachesim.py defects`"))
    if m["uncached"] > 0.05:
        out.append(("uncached", m["uncached"],
                    f"{m['uncached']:.1%} of the bill is uncached input; content is "
                    f"sitting past the last breakpoint (relocation's signature)"))
    if m["unexplained"] > 0.02:
        out.append(("unexplained", m["unexplained"],
                    f"{m['unexplained']:.1%} of turns change the message list with no "
                    f"feature signature — run `cachesim.py damage`"))
    # The tripwire written into the append-only guard: blind replay is only
    # cheap while forwarded bytes track client bytes.
    if m["bloat"] > 1.2:
        out.append(("bloat", m["bloat"],
                    f"forwarded body is {m['bloat']:.2f}x the client body (median); "
                    f"the reminder-blind guard's assumption has broken"))
    if m["worst_cost"] > 100_000:
        out.append(("worst", m["worst_cost"],
                    f"one turn cost {m['worst_cost']:,.0f} more than unproxied: "
                    f"out/{m['worst_turn']}.json"))
    if m["disk_mb"] > 20_000:
        out.append(("disk", m["disk_mb"],
                    f"capture is {m['disk_mb'] / 1000:.1f} GB — disarm it in "
                    f"restart-headroom.sh"))
    return out


def run_once(corpus: str, state_path: str) -> None:
    try:
        with open(state_path) as handle:
            state = json.load(handle)
    except (OSError, ValueError):
        state = {}

    m = measure(corpus)
    if m.get("filling"):
        if state.get("announced_filling") != m["turns"] // 20:
            print(f"corpus filling: {m['turns']}/{MIN_TURNS} paired turns")
            state["announced_filling"] = m["turns"] // 20
            _save(state_path, state)
        return

    seen = state.get("faults", {})
    current = {}
    for key, level, message in faults(m):
        current[key] = level
        was = seen.get(key)
        if was is None or abs(level - was) / max(abs(was), 1e-9) > RESTATE:
            print(f"FAULT {key}: {message}")
    for key in seen:
        if key not in current:
            print(f"cleared {key}: no longer above threshold")
    if not current and seen:
        print(f"all clear on {m['turns']} turns "
              f"(proxy at {m['ratio']:.2f}x plain Claude Code)")
    state["faults"] = current
    state["turns"] = m["turns"]
    _save(state_path, state)


def _save(path: str, state: dict) -> None:
    with open(path, "w") as handle:
        json.dump(state, handle)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--corpus", default=DEFAULT_CORPUS)
    parser.add_argument("--state", default=DEFAULT_STATE)
    parser.add_argument("--interval", type=int, default=900)
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args()
    while True:
        try:
            run_once(args.corpus, args.state)
        except Exception as exc:  # a bad pass must not end the watch
            print(f"watch error: {exc}", flush=True)
        sys.stdout.flush()
        if args.once:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    sys.exit(main())
