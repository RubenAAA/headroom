#!/usr/bin/env python3
"""Candidate proxy behaviours, priced against a captured corpus for free.

`cachesim.py compare` can only score what the proxy actually did, so it answers
"was that build good" and never "would this idea be better". A strategy is that
missing half: a function from request body to request body, applied to every
turn of a corpus and scored the same way. No proxy is built, nothing is sent,
and a bad idea costs nothing to find out about.

    cachesim.py experiment <corpus_dir> [--base client|forwarded]

`--base client` asks what a fresh proxy would do to raw Claude Code traffic.
`--base forwarded` stacks the idea on top of what the proxy already does, which
is the honest question when the change is meant to ship as a patch.

A strategy must be pure and must not assume it is the only one running. Register
with @strategy and keep the docstring's first line short — it is what the report
prints.

The limit is worth stating twice: this prices cache structure. A strategy that
deletes half the conversation will look brilliant here. Run `damage` on anything
that wins before believing it.
"""
from __future__ import annotations

import copy
import re

REGISTRY: dict[str, callable] = {}


def strategy(name):
    def register(fn):
        REGISTRY[name] = fn
        return fn
    return register


def _content_blocks(body: dict):
    """Every message content block, as a mutable reference in place.

    String content is converted to a one-element text block in place first, so a
    strategy sees one shape and only one. Claude Code flips a message between
    the two as its cache marker moves on and off it, and a strategy that only
    handles the list shape silently applies to a message on one turn and not the
    next — which changes that message's digest between turns and destroys the
    cached prefix at exactly the point it edited. That bug cost
    `split-volatile-counter` a 267% score before it was found.
    """
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, str):
            message["content"] = content = [{"type": "text", "text": content}]
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict):
                    yield message, block


def _clear_message_markers(body: dict) -> None:
    for _, block in _content_blocks(body):
        block.pop("cache_control", None)


@strategy("noop")
def noop(body):
    """Forward unchanged — the control arm, and a check the harness is honest."""
    return body


@strategy("tail-breakpoints-1")
def tail_1(body):
    """One cache_control on the last message block."""
    return _tail_breakpoints(body, 1)


@strategy("tail-breakpoints-2")
def tail_2(body):
    """Two trailing cache_control markers (what the proxy ships today)."""
    return _tail_breakpoints(body, 2)


@strategy("tail-breakpoints-3")
def tail_3(body):
    """Three trailing markers, spending more of the budget of four on messages."""
    return _tail_breakpoints(body, 3)


def _tail_breakpoints(body, count):
    _clear_message_markers(body)
    blocks = [b for _, b in _content_blocks(body)]
    # Space them out rather than bunching at the very end: adjacent markers cache
    # nearly the same prefix, so the second one buys almost nothing. Quarters of
    # the history give the older marker a chance of surviving an edit near the
    # tail, which is where this client edits.
    if not blocks:
        return body
    for i in range(count):
        index = len(blocks) - 1 - round(i * len(blocks) / (4 * max(count, 1)))
        if 0 <= index < len(blocks):
            blocks[index]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


@strategy("strip-system-breakpoints")
def strip_system(body):
    """Drop the client's system markers — measured live as expensive, kept as a check."""
    system = body.get("system")
    if isinstance(system, list):
        for block in system:
            if isinstance(block, dict):
                block.pop("cache_control", None)
    return body


@strategy("force-ttl-5m")
def ttl_5m(body):
    """Every breakpoint at 5m: cheaper writes, but they expire between turns."""
    return _force_ttl(body, "5m")


@strategy("force-ttl-1h")
def ttl_1h(body):
    """Every breakpoint at 1h: 2x the write, survives a coffee break."""
    return _force_ttl(body, "1h")


def _force_ttl(body, ttl):
    system = body.get("system")
    blocks = list(system) if isinstance(system, list) else []
    blocks += [b for _, b in _content_blocks(body)]
    for block in blocks:
        if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
            block["cache_control"] = {"type": "ephemeral", "ttl": ttl}
    return body


# The counter Claude Code appends to the tail. It changes every single turn, so
# any prefix that contains it dies every single turn.
COUNTER = re.compile(r"<total_tokens>\s*\d+\s*tokens left</total_tokens>")


@strategy("split-volatile-counter")
def split_counter(body):
    """Isolate the per-turn token counter so the stable text around it can cache.

    The tail block carries ~12 KB of stable reminder text welded to a counter
    that changes on every turn, so the whole block is uncacheable for the sake of
    a few digits. Splitting them puts the stable part inside the prefix and
    leaves only the counter past the last breakpoint. Proposed after the
    relocation post-mortem and never tested — this is what tests it.
    """
    for message, block in list(_content_blocks(body)):
        text = block.get("text")
        if not isinstance(text, str) or not COUNTER.search(text):
            continue
        stable = COUNTER.sub("", text)
        volatile = " ".join(COUNTER.findall(text))
        if not stable.strip():
            continue
        content = message["content"]
        index = content.index(block)
        keep = {k: v for k, v in block.items() if k != "cache_control"}
        keep["text"] = stable
        tail = {"type": "text", "text": volatile}
        if isinstance(block.get("cache_control"), dict):
            # The marker belongs on the stable half, which is the part worth
            # caching; the counter trails it and bills fresh, which is correct
            # and costs a few tokens instead of twelve thousand.
            keep["cache_control"] = block["cache_control"]
        content[index:index + 1] = [keep, tail]
    return body


@strategy("cache-behind-counter")
def cache_behind_counter(body):
    """Split the counter off AND put a breakpoint before it, so the stable half caches.

    The post-mortem's actual proposal. Splitting alone does nothing when the
    whole block already sits past the last breakpoint — both halves still bill
    fresh. What buys anything is a marker between them, which pulls the stable
    kilobytes inside the cached prefix and leaves only the digits outside.

    Costs one of the four breakpoints. `flatten` drops the earliest marker when
    that pushes past the cap, so this trades the oldest cached prefix for the
    newest — worth it only if the tail block is large.
    """
    split_counter(body)
    blocks = [b for _, b in _content_blocks(body)]
    # Only the NEWEST counter gets a marker. History carries one per turn, and
    # marking them all blows the budget of four — `flatten` then keeps the last
    # four and silently evicts the system breakpoints, which are the floor under
    # every miss. That mistake alone read as a 148% regression.
    for i in range(len(blocks) - 2, -1, -1):
        text = blocks[i + 1].get("text")
        if isinstance(text, str) and COUNTER.fullmatch(text.strip()):
            blocks[i]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
            break
    return body


def apply(name: str, body: dict) -> dict:
    """Run a strategy on a deep copy, so arms cannot contaminate each other."""
    return REGISTRY[name](copy.deepcopy(body))
