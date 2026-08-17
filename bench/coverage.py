#!/usr/bin/env python3
"""Score the proxy against the client it is proxying, without calling a model.

    python3 coverage.py <capture_dir> [--csv]

A capture holds both sides of every turn: `req-*.json` is what the client sent,
`out/<request_id>.json` is what we forwarded. Anthropic's billing rule for a
single request is arithmetic, so three of the four ways the proxy can cost money
are computable from those two bodies alone, with no request and no tokens spent:

1. **Uncovered tail** — everything after the last `cache_control` marker bills as
   fresh input at full price. Ours must never be larger than the client's: a
   marker placed earlier than the client's own forfeits coverage the client had.
   This is what the live A/B priced at 5,959 fresh tokens a conversation against
   the client's 18, which was 4.4 points of an 11.5% loss (2026-08-17).
2. **Inflation** — bytes we add to the prompt, which are then re-read every turn.
3. **Prefix stability** — whether message 0 holds still turn over turn. A change
   there re-creates the whole conversation; it is the single most expensive thing
   the proxy can do and it cost 688,893 tokens the day it was found.

The fourth way is behavioural: injected context changes which tools the model
reaches for, and an extra turn costs a full round trip. That one needs a model,
so it lives in `ab_live.sh`. Everything here is deterministic — same capture, same
numbers, forever — so it belongs in a regression check and `ab_live.sh` does not.

Exit status is 1 if any check fails, so this can gate a build.
"""
from __future__ import annotations

import collections
import glob
import json
import os
import sys

# Bytes per token, fitted against real billing on this traffic. `cachesim.py
# validate` reports the current best estimate; it is only used to put the byte
# counts on a scale that can be compared to a bill.
BYTES_PER_TOKEN = 3.34


def canon(value) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def last_marker(body: dict) -> tuple[int, int] | None:
    """(message index, block index) of the final `cache_control`, if any."""
    found = None
    for i, msg in enumerate(body.get("messages") or []):
        content = msg.get("content")
        if not isinstance(content, list):
            continue
        for j, block in enumerate(content):
            if isinstance(block, dict) and block.get("cache_control"):
                found = (i, j)
    return found


def uncovered_bytes(body: dict) -> int:
    """Bytes after the last marker — what the provider bills as fresh input.

    With no marker at all nothing in `messages` is cached, so the whole array
    counts. `system` and `tools` are excluded: they precede `messages`, so their
    own markers cover them and they are never in the uncovered tail.
    """
    messages = body.get("messages") or []
    mark = last_marker(body)
    if mark is None:
        return len(canon(messages))
    li, lj = mark
    total = 0
    for i, msg in enumerate(messages):
        if i < li:
            continue
        content = msg.get("content")
        if not isinstance(content, list):
            total += len(canon(msg))
            continue
        for j, block in enumerate(content):
            if i == li and j <= lj:
                continue
            total += len(canon(block))
    return total


def load(capture_dir: str):
    """Turns with both sides present, ordered as the proxy saw them."""
    turns = []
    out_dir = os.path.join(capture_dir, "out")
    for path in glob.glob(os.path.join(capture_dir, "req-*.json")):
        try:
            with open(path) as handle:
                env = json.load(handle)
        except (OSError, ValueError):
            continue
        body = env.get("body")
        if not isinstance(body, dict) or "messages" not in body:
            continue
        rid = env.get("request_id", "")
        forwarded_path = os.path.join(out_dir, rid + ".json")
        if not os.path.exists(forwarded_path):
            continue
        try:
            with open(forwarded_path) as handle:
                forwarded = json.load(handle)
        except (OSError, ValueError):
            continue
        turns.append(
            (env.get("ts_ms", 0), env.get("session_key", ""), rid, body, forwarded)
        )
    turns.sort()
    return turns


def first_block(body: dict) -> str:
    """Canonical form of message 0's first block — the prefix-stability probe.

    `cache_control` is stripped first. The provider keys its cache on message
    CONTENT, and the breakpoint is supposed to walk forward to the newest message
    every turn, so a marker leaving message 0 is correct behaviour rather than
    drift. Counting it made turn 2 of every conversation look like a bust.
    """
    messages = body.get("messages") or []
    if not messages:
        return ""
    content = messages[0].get("content")
    if isinstance(content, list) and content:
        block = content[0]
        if isinstance(block, dict):
            block = {k: v for k, v in block.items() if k != "cache_control"}
        return canon(block)
    return canon(content)


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    capture_dir = sys.argv[1]
    as_csv = "--csv" in sys.argv
    turns = load(capture_dir)
    if not turns:
        print(f"no paired turns in {capture_dir}")
        return 2

    worse_cover = []
    inflation = []
    by_session: dict[str, list] = collections.defaultdict(list)
    for ts, session, rid, client, forwarded in turns:
        cu, fu = uncovered_bytes(client), uncovered_bytes(forwarded)
        if fu > cu:
            worse_cover.append((rid, len(forwarded.get("messages") or []), cu, fu))
        inflation.append((len(canon(client)), len(canon(forwarded))))
        by_session[session].append((ts, rid, first_block(forwarded)))

    drifted = []
    for session, rows in by_session.items():
        rows.sort()
        for (_, _, prev), (_, rid, cur) in zip(rows, rows[1:]):
            if prev != cur:
                drifted.append((session, rid))

    if as_csv:
        print("metric,value")
        print(f"turns,{len(turns)}")
        print(f"turns_with_worse_coverage,{len(worse_cover)}")
        print(f"fresh_tokens_forfeited,{sum(f - c for _, _, c, f in worse_cover) / BYTES_PER_TOKEN:.0f}")
        print(f"message0_drift_events,{len(drifted)}")
        return 0 if not worse_cover and not drifted else 1

    cb = sum(c for c, _ in inflation)
    fb = sum(f for _, f in inflation)
    print(f"{len(turns)} paired turns in {os.path.basename(capture_dir)}, "
          f"{len(by_session)} conversations\n")

    print("1. UNCOVERED TAIL (fresh input at full price)")
    forfeited = sum(f - c for _, _, c, f in worse_cover) / BYTES_PER_TOKEN
    print(f"   turns where our coverage is worse than the client's: "
          f"{len(worse_cover)} of {len(turns)}")
    print(f"   fresh tokens forfeited: {forfeited:,.0f}")
    for rid, depth, c, f in sorted(worse_cover, key=lambda r: r[2] - r[3])[:5]:
        print(f"     {rid[:8]} depth {depth:<4} client {c:>9,}B  ours {f:>9,}B  "
              f"+{(f - c) / BYTES_PER_TOKEN:>8,.0f} tok")

    print("\n2. INFLATION (bytes we add, re-read every turn)")
    print(f"   client {cb / 1e6:.1f} MB -> forwarded {fb / 1e6:.1f} MB "
          f"({(fb / cb - 1) if cb else 0:+.1%})")
    print(f"   per turn: {(fb - cb) / len(turns) / BYTES_PER_TOKEN:+,.0f} tokens")

    print("\n3. PREFIX STABILITY (message 0 holding still)")
    print(f"   message-0 changes mid-conversation: {len(drifted)}")
    for session, rid in drifted[:5]:
        print(f"     {session.split(':')[-1][:8]} at {rid[:8]}")

    ok = not worse_cover and not drifted
    print(f"\n{'PASS' if ok else 'FAIL'}: "
          f"{'no forfeited coverage, no message-0 drift' if ok else 'see above'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
