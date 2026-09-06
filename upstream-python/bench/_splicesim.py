"""Price the splice fix before it ships.

Builds the corpus the patched proxy would have forwarded — stored prefix up to
the first divergence, this turn's own bytes after it — and scores it against
both the live proxy and plain Claude Code.

The splice is stateful, so it cannot be a `strategies.py` entry: each turn's
output depends on the previous turn's output, not just its own body.
"""
import collections
import copy
import json
import sys

from cachesim import (load_corpus, with_forwarded_bodies, score_corpus, flatten,
                      PROFILES, Turn, DEFAULT_BYTES_PER_TOKEN as BPT)
from dataclasses import replace

P = sys.argv[1]
turns = load_corpus(P)
fwd = {t.request_id: t for t in with_forwarded_bodies(turns, P)}
turns = [t for t in turns if t.request_id in fwd]


def digests(messages):
    """One digest per message, on the same canonical form the sim hashes."""
    body = {"model": "x", "messages": messages}
    return [s.digest for s in flatten(body, BPT) if s]


def splice_corpus(transform=None, mark=None):
    """What the patched proxy forwards, chained turn over turn.

    `transform` is applied before the splice, so the stored prefix the next turn
    replays is already transformed — which is what shipping the two together
    would actually do.
    """
    scopes = collections.defaultdict(list)
    for t in turns:
        scopes[(t.session_key, t.body.get("model"))].append(t)

    out = {}
    for _, ts in scopes.items():
        ts.sort(key=lambda t: t.ts)
        prev_orig = prev_out = None
        for t in ts:
            cur_orig = t.body.get("messages") or []
            cur_fwd = copy.deepcopy(fwd[t.request_id].body)
            if transform:
                cur_fwd = transform(cur_fwd) or cur_fwd
            cur_msgs = cur_fwd.get("messages") or []
            if prev_orig is not None:
                a, b = digests(prev_orig), digests(cur_orig)
                i = 0
                while i < min(len(a), len(b)) and a[i] == b[i]:
                    i += 1
                upto = min(i, len(prev_out), len(cur_msgs))
                cur_fwd["messages"] = prev_out[:upto] + cur_msgs[upto:]
            # Breakpoint placement runs after the overlay in the proxy
            # (`proxy.rs:5431`), and it has to: the replayed prefix carries the
            # previous turn's markers, so a rule that puts its marker behind the
            # tail would have it discarded every turn and never advance.
            if mark:
                cur_fwd = mark(cur_fwd) or cur_fwd
            # The splice hands the next turn the same message objects, so a
            # later `mark` would edit the body already stored here. Store a copy.
            out[t.request_id] = copy.deepcopy(cur_fwd)
            prev_orig = cur_orig
            prev_out = cur_fwd["messages"]
    return out


def retimed(body, ttl):
    """Same body, every cache_control breakpoint asking for `ttl`."""
    body = copy.deepcopy(body)
    stack = [body.get("system"), body.get("tools"), body.get("messages")]
    seen = []
    while stack:
        node = stack.pop()
        if isinstance(node, list):
            stack.extend(node)
        elif isinstance(node, dict):
            if isinstance(node.get("cache_control"), dict):
                node["cache_control"]["ttl"] = ttl
            stack.extend(v for v in node.values() if isinstance(v, (list, dict)))
    return body


import strategies

spliced = splice_corpus()
arms = {
    "claude code": [t for t in turns],
    "live proxy": [fwd[t.request_id] for t in turns],
    "proxy + splice": [replace(t, body=spliced[t.request_id]) for t in turns],
    "splice + 5m": [replace(t, body=retimed(spliced[t.request_id], "5m"))
                    for t in turns],
}
THINKING = strategies.THINKING


def strip_old_thinking(body):
    """Every assistant message but the last loses its reasoning."""
    messages = body.get("messages") or []
    last = max((i for i, m in enumerate(messages)
                if m.get("role") == "assistant"), default=-1)
    for i, message in enumerate(messages):
        if i == last or not isinstance(message.get("content"), list):
            continue
        message["content"] = [b for b in message["content"]
                              if not (isinstance(b, dict) and b.get("type") in THINKING)]
    return body


def mark_behind_last_assistant(body):
    """One message breakpoint, sealed before the message that still has reasoning.

    Nothing cached ever contains a block that a later turn will remove, so no
    entry is ever invalidated. What falls outside bills fresh, but that is the
    newest turn, which bills fresh regardless.
    """
    messages = body.get("messages") or []
    last = max((i for i, m in enumerate(messages)
                if m.get("role") == "assistant"), default=-1)
    strategies._clear_message_markers(body)
    if last <= 0:
        return body
    blocks = [b for i, m in enumerate(messages) if i < last
              for b in (m.get("content") if isinstance(m.get("content"), list) else [])
              if isinstance(b, dict)]
    if blocks:
        blocks[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


def mark_tail(body):
    """The proxy's own rule: one marker on the last block of the last message."""
    messages = body.get("messages") or []
    strategies._clear_message_markers(body)
    for message in reversed(messages):
        content = message.get("content")
        blocks = [b for b in content if isinstance(b, dict)] if isinstance(content, list) else []
        if blocks:
            blocks[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
            break
    return body


import re

VERSION = re.compile(r"(cc_version=)[0-9A-Za-z.]+")


def normalize_build_id(body):
    """Pin Claude Code's build hash inside the system prompt.

    `cc_version=2.1.233.8f7` sits in a 70-character system block at the very
    front of the cacheable prefix. Three of its characters change when the CLI
    updates and the whole conversation is re-created: 129,119 tokens on one turn
    of this corpus. Nothing the model does depends on knowing its own build, so
    the string is held to a constant — and holding it to the same constant in
    every conversation lets a new one hit the prefix an old one already cached.
    """
    system = body.get("system")
    if isinstance(system, list):
        for block in system:
            if isinstance(block, dict) and isinstance(block.get("text"), str):
                block["text"] = VERSION.sub(r"\1pinned", block["text"])
    return body


def tail_5m_stable_1h(body):
    """1h on the system/tools prefix, 5m on the moving message tail.

    Nearly every created token is the tail: content this turn appends, written
    once and superseded by the next turn's marker seconds later. Buying an hour
    of retention for it costs 2.0x input against the 5-minute tier's 1.25x, and
    the hour is never claimed. The system and tools prefix is the opposite case
    — stable for the session, read hundreds of times — so it keeps the long TTL
    and carries the conversation across an idle gap.
    """
    for key in ("system", "tools"):
        node = body.get(key)
        if isinstance(node, list):
            for block in node:
                if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
                    block["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    for message in body.get("messages") or []:
        content = message.get("content")
        for block in content if isinstance(content, list) else []:
            if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
                block["cache_control"] = {"type": "ephemeral", "ttl": "5m"}
    return body


def one_tail_marker(body):
    """Keep only the last message breakpoint — what the client itself sends.

    The proxy places two. The second exists so a miss on the newest entry still
    finds the one behind it, which mattered when a diverged prefix declined
    outright; with the splice replaying through a divergence it buys less.
    """
    seen = []
    for message in body.get("messages") or []:
        content = message.get("content")
        for block in content if isinstance(content, list) else []:
            if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
                seen.append(block)
    for block in seen[:-1]:
        block.pop("cache_control", None)
    return body


for label, mark in (("tail 5m", tail_5m_stable_1h),
                    ("one tail marker", one_tail_marker)):
    got = splice_corpus(normalize_build_id, mark)
    arms[f"splice + normalize + {label}"] = [
        replace(t, body=got[t.request_id]) for t in turns
    ]

normalized = splice_corpus(normalize_build_id)
arms["splice + normalize build id"] = [
    replace(t, body=normalized[t.request_id]) for t in turns
]

# Hold the whole system array to the first one each conversation sent. The
# build id is only the commonest thing that churns in it; a `cd` rewrites the
# environment block and costs the same conversation-wide re-creation. This
# prices the ceiling — shipping it needs the changed text delivered somewhere
# harmless rather than dropped, since the model does need the current directory.
_first_system = {}


def pin_system(body):
    key = id(body)
    return body


def pinned_arm():
    scopes = collections.defaultdict(list)
    for t in turns:
        scopes[(t.session_key, t.body.get("model"))].append(t)
    out = {}
    for _, ts in scopes.items():
        ts.sort(key=lambda t: t.ts)
        first = ts[0].body.get("system")
        first = [b.get("text") for b in first] if isinstance(first, list) else None

        def hold(body, first=first):
            system = body.get("system")
            if isinstance(system, list) and first:
                for i, block in enumerate(system):
                    if i < len(first) and isinstance(block, dict):
                        block["text"] = first[i]
            return body

        got = splice_corpus(hold, one_tail_marker)
        for t in ts:
            out[t.request_id] = got[t.request_id]
    return out


def mark_behind_reminders(body):
    """One breakpoint, sealed before the newest `<system-reminder>`.

    The client hangs these off its newest message and withdraws them a turn or
    two later — half of the 56,988 reminder tokens in this corpus are taken back
    by the client itself. Cached, they cost the 1h write rate and then break the
    prefix when they vanish. Left outside the last breakpoint they bill as fresh
    input for the one turn they exist, and their withdrawal costs nothing.

    The model sees exactly what the client sent, on every turn. Only where the
    bytes sit in the cache changes.
    """
    messages = body.get("messages") or []

    def has_reminder(message):
        content = message.get("content")
        if isinstance(content, str):
            return "<system-reminder>" in content
        return any("<system-reminder>" in (b.get("text") or "")
                   for b in (content or []) if isinstance(b, dict))

    # Only the tail is in play. Old messages keep their reminders in the
    # replayed prefix, and sealing before one of those would leave the whole
    # conversation outside the cache — which is what happens if you take the
    # last reminder in the array rather than the last one in the live window.
    window = max(0, len(messages) - 3)
    seal = min((i for i, m in enumerate(messages[window:], start=window)
                if has_reminder(m)), default=len(messages))
    strategies._clear_message_markers(body)
    for message in reversed(messages[:seal] or messages):
        content = message.get("content")
        blocks = [b for b in content if isinstance(b, dict)] if isinstance(content, list) else []
        if blocks:
            blocks[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
            break
    return body


def pinned_arm_marked(mark):
    scopes = collections.defaultdict(list)
    for t in turns:
        scopes[(t.session_key, t.body.get("model"))].append(t)
    out = {}
    for _, ts in scopes.items():
        ts.sort(key=lambda t: t.ts)
        first = ts[0].body.get("system")
        first = [b.get("text") for b in first] if isinstance(first, list) else None

        def hold(body, first=first):
            system = body.get("system")
            if isinstance(system, list) and first:
                for i, block in enumerate(system):
                    if i < len(first) and isinstance(block, dict):
                        block["text"] = first[i]
            return body

        got = splice_corpus(hold, mark)
        for t in ts:
            out[t.request_id] = got[t.request_id]
    return out


rem = pinned_arm_marked(mark_behind_reminders)
arms["everything + seal behind reminders"] = [
    replace(t, body=rem[t.request_id]) for t in turns
]

held = pinned_arm()
arms["splice + hold system + 1 marker"] = [
    replace(t, body=held[t.request_id]) for t in turns
]
arms["claude code + normalize"] = [
    replace(t, body=normalize_build_id(copy.deepcopy(t.body))) for t in turns
]

def tail_5m_one_marker(body):
    """One message breakpoint at the tail, asking for the 5-minute tier.

    A 5m write costs 1.25x input against the 1h tier's 2.0x, and every cache
    read refreshes the entry for free. Turns arrive a median of 9 seconds apart,
    so the entry is renewed long before it can expire. The whole risk is the 1%
    of gaps longer than five minutes, where the conversation falls back to the
    system marker and rewrites its history.
    """
    one_tail_marker(body)
    for message in body.get("messages") or []:
        content = message.get("content")
        for block in content if isinstance(content, list) else []:
            if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
                block["cache_control"] = {"type": "ephemeral", "ttl": "5m"}
    return body


warm = pinned_arm_marked(tail_5m_one_marker)
arms["everything + 5m tail"] = [replace(t, body=warm[t.request_id]) for t in turns]

# The same arm, scored as if a scheduled refresh kept the 5-minute entries
# alive across the idle gaps. Anthropic documents the pattern: a `max_tokens: 0`
# request re-reads the prefix, which renews it at the cache-hit rate. This is
# the ceiling — it assumes the refresh itself is free, which is only true if
# reads are.
KEEP_WARM = dict(arms)


gaps = []
scopes = collections.defaultdict(list)
for t in turns:
    scopes[(t.session_key, t.body.get("model"))].append(t)
for _, ts in scopes.items():
    ts.sort(key=lambda t: t.ts)
    gaps += [b.ts - a.ts for a, b in zip(ts, ts[1:])]
gaps.sort()
over5 = sum(1 for g in gaps if g > 300)
print(f"inter-turn gaps: {len(gaps)}, median {gaps[len(gaps)//2]:.0f}s, "
      f"p90 {gaps[9*len(gaps)//10]:.0f}s, over 5 min: {over5} "
      f"({over5/len(gaps):.0%})\n")

print(f"{len(turns)} turns\n")
print(f"{'arm':>26}{'read':>13}{'create':>11}{'api $':>12}{'fitted':>12}"
      f"{'vs cc':>9}")
base = {}
for name, arm in arms.items():
    total, _ = score_corpus(sorted(arm, key=lambda t: t.ts), BPT)
    api = total.billed_with(PROFILES["api"])
    fit = total.billed_with(PROFILES["subscription"])
    base.setdefault("fit", fit)
    print(f"{name:>26}{total.read_tokens:>13,}{total.create_tokens:>11,}"
          f"{api:>12,.0f}{fit:>12,.0f}{fit / base['fit'] - 1:>+9.1%}")

import cachesim

held_5m = cachesim.TTL_SECONDS["5m"]
cachesim.TTL_SECONDS["5m"] = cachesim.TTL_SECONDS["1h"]
for name in ("everything + 5m tail",):
    total, _ = score_corpus(sorted(arms[name], key=lambda t: t.ts), BPT)
    fit = total.billed_with(PROFILES["subscription"])
    api = total.billed_with(PROFILES["api"])
    print(f"{name + ', kept warm':>26}{total.read_tokens:>13,}{total.create_tokens:>11,}"
          f"{api:>12,.0f}{fit:>12,.0f}{fit / base['fit'] - 1:>+9.1%}")
cachesim.TTL_SECONDS["5m"] = held_5m
