"""What is it worth to hold the system block still?

Every large re-creation in the client arm begins at a system block, and the
edits that cause them are three characters of Claude Code build hash and a
working-directory line. Neither is content the model reasons with, but they sit
at the very front of the prefix, so a character costs the conversation.

Prices four ways of holding it still, on the client's own bodies, so the number
is what a proxy could win rather than what this one currently does.
"""
import collections
import copy
import re
import sys

from cachesim import (load_corpus, with_forwarded_bodies, score_corpus,
                      PROFILES, DEFAULT_BYTES_PER_TOKEN as BPT)
from dataclasses import replace

P = sys.argv[1]
raw = load_corpus(P)
keep = {t.request_id for t in with_forwarded_bodies(raw, P)}
raw = [t for t in raw if t.request_id in keep]

# `cc_version=2.1.233.8f7` — a build identifier, and the whole of the block it
# lives in is a billing header restated as prose.
VERSION = re.compile(r"(cc_version=)[0-9A-Za-z.]+")


def pin(body, first, which):
    """Replace system blocks with the first ones this conversation sent."""
    system = body.get("system")
    if not isinstance(system, list) or not isinstance(first, list):
        return body
    body = copy.deepcopy(body)
    for i, block in enumerate(body["system"]):
        if i >= len(first) or not isinstance(block, dict):
            continue
        if which == "all" or (which == "first" and i == 0):
            block["text"] = first[i]
    return body


def normalize(body, _first, _which):
    """Same idea, but only the build identifier, and to a constant."""
    system = body.get("system")
    if not isinstance(system, list):
        return body
    body = copy.deepcopy(body)
    for block in body["system"]:
        if isinstance(block, dict) and isinstance(block.get("text"), str):
            block["text"] = VERSION.sub(r"\1pinned", block["text"])
    return body


def arm(fn, which):
    scopes = collections.defaultdict(list)
    for t in raw:
        scopes[(t.session_key, t.body.get("model"))].append(t)
    out = []
    for _, ts in scopes.items():
        ts.sort(key=lambda t: t.ts)
        system = ts[0].body.get("system")
        first = [b.get("text") for b in system] if isinstance(system, list) else None
        for t in ts:
            out.append(replace(t, body=fn(t.body, first, which)))
    return sorted(out, key=lambda t: t.ts)


arms = {
    "claude code": raw,
    "pin build id only": arm(pin, "first"),
    "normalize build id": arm(normalize, None),
    "pin whole system": arm(pin, "all"),
}

print(f"{'arm':>20}{'create':>11}{'read':>13}{'fitted':>12}{'vs cc':>9}")
base = None
for name, turns in arms.items():
    total, _ = score_corpus(turns, BPT)
    fit = total.billed_with(PROFILES["subscription"])
    base = base or fit
    print(f"{name:>20}{total.create_tokens:>11,}{total.read_tokens:>13,}"
          f"{fit:>12,.0f}{fit / base - 1:>+9.1%}")
