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
import json
import inspect
import hashlib
import os
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


MEMORY_DIR = os.path.expanduser(
    "~/.claude-work/projects/-home-user-headroom/memory"
)
_INDEX_LINE = re.compile(r"^\s*- \[[^\]]+\]\([^)]+\.md\)", re.M)


def _memory_index_blocks(body: dict):
    """Every text block carrying the auto-memory index, with its index lines."""
    for _, block in _content_blocks(body):
        text = block.get("text")
        if not isinstance(text, str) or "MEMORY.md" not in text:
            continue
        lines = _INDEX_LINE.findall(text)
        if lines:
            yield block, text


@strategy("mem-drop-index")
def mem_drop_index(body):
    """Drop the MEMORY.md index lines — pointers we pay for on every turn."""
    for block, text in _memory_index_blocks(body):
        block["text"] = _INDEX_LINE.sub("", text)
    return body


@strategy("mem-recall-pinned")
def mem_recall_pinned(body):
    """Drop the index, pin four memory bodies where the index used to sit."""
    pinned = _pinned_memories()
    for block, text in _memory_index_blocks(body):
        block["text"] = _INDEX_LINE.sub("", text) + pinned
    return body


@strategy("mem-recall-no-rereads")
def mem_recall_no_rereads(body):
    """Pinned recall, and the memory-file Reads it makes redundant are digested.

    The optimistic arm: it assumes serving a memory up front stops the model
    fetching the same file by hand. `damage` will show the removed reads, and
    should be read before believing this one.
    """
    mem_recall_pinned(body)
    reads = {
        block.get("id")
        for _, block in _content_blocks(body)
        if block.get("type") == "tool_use"
        and "/memory/" in str((block.get("input") or {}).get("file_path", ""))
    }
    for _, block in _content_blocks(body):
        if block.get("type") == "tool_result" and block.get("tool_use_id") in reads:
            block["content"] = "[memory served up front; read elided]"
    return body


_PINNED_CACHE: list[str] = []


def _pinned_memories() -> str:
    """The four memories this project reads most, as one stable text run.

    Fixed rather than per-turn: recall that changes as the conversation grows
    rewrites a message that is already cached, which is the defect this design
    exists to avoid. A constant set is identical on every turn, so it enters the
    prefix once and stays there.
    """
    if _PINNED_CACHE:
        return _PINNED_CACHE[0]
    names = [
        "features-on-but-inert.md",
        "context-editing-api-facts.md",
        "server-side-tool-clearing.md",
        "prompt-composition-map.md",
    ]
    parts = []
    for name in names:
        try:
            with open(os.path.join(MEMORY_DIR, name)) as handle:
                parts.append(handle.read())
        except OSError:
            continue
    _PINNED_CACHE.append("\n\n".join(parts))
    return _PINNED_CACHE[0]


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


def _drop_first_system_marker(body) -> bool:
    """Free one marker from `system`, keeping the later one. True if it moved."""
    system = body.get("system")
    if not isinstance(system, list):
        return False
    for block in system:
        if isinstance(block, dict) and block.get("cache_control"):
            block.pop("cache_control", None)
            return True
    return False


@strategy("rebalance-1sys-3tail")
def rebalance_1sys_3tail(body):
    """Spend one of the two system markers on a third tail marker.

    Anthropic takes four markers and this client already spends all four: two on
    `system`, two on the message tail. `tail-breakpoints-3` appeared to beat that
    but asks for five, and the simulator quietly drops the earliest to fit —
    which means its score was never "three tail markers", it was "one system
    marker and three tail". That is a legal request nobody had named, so this
    names it and asks for exactly four.
    """
    _drop_first_system_marker(body)
    return _tail_breakpoints(body, 3)


@strategy("rebalance-1sys-2tail")
def rebalance_1sys_2tail(body):
    """Control: lose the system marker, do not spend it. Isolates the two halves."""
    _drop_first_system_marker(body)
    return _tail_breakpoints(body, 2)


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


def _emulate_proxy_tail(body, slots):
    """Strip the client's message markers and re-place `slots` at the tail.

    A faithful-enough stand-in for `place_tail_cache_breakpoints`, so the marker
    policy can be priced on its own against the client's untouched placement.
    """
    _clear_message_markers(body)
    if slots <= 0:
        return body
    tails = []
    for message in body.get("messages") or []:
        content = message.get("content")
        if isinstance(content, list) and content and isinstance(content[-1], dict):
            tails.append(content[-1])
    for block in tails[-slots:]:
        block["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


@strategy("tail-slots-0")
def tail_slots_0(body):
    """Drop every message breakpoint: what the client sent, minus its own markers."""
    return _emulate_proxy_tail(body, 0)


@strategy("tail-slots-1")
def tail_slots_1(body):
    """One proxy-placed tail breakpoint, client markers stripped."""
    return _emulate_proxy_tail(body, 1)


@strategy("tail-slots-2")
def tail_slots_2(body):
    """Two proxy-placed tail breakpoints — what ships today."""
    return _emulate_proxy_tail(body, 2)


THINKING = ("thinking", "redacted_thinking")

# ── Do not ship anything below this line ──────────────────────────────────────
#
# Reasoning is 31.6% of every token the proxy writes into the cache and the
# strategies here price at up to -27%, which makes them the best-scoring ideas
# in this file. They are all disqualified, and the reason is not a cache
# property the simulator can see.
#
# Anthropic's extended-thinking docs, on preservation by model: "Claude Opus 4.5
# and models numbered 4.6 and higher keep prior turns' thinking blocks in
# context and bill them as input, where Claude Sonnet 4.5, Claude Haiku 4.5, and
# earlier models stripped them." On the models this proxy actually serves, the
# reasoning of earlier turns is context the model reads. Removing it buys tokens
# by making the model worse at the conversation it is having, and the simulator
# scores answer quality at exactly zero.
#
# Manual thinking mode adds a second, harder barrier: the final assistant turn
# of a thinking-enabled request must begin with a thinking block, so
# `strip-all-thinking` is not merely unwise but rejected on the wire.
#
# They stay because an upper bound is worth knowing: they say how much of the
# bill is reasoning, which is how you know not to go looking for that 27%
# somewhere else.


@strategy("strip-old-thinking")
def strip_old_thinking(body):
    """Drop reasoning blocks from every assistant message but the last.

    Reasoning is 29% of the message tokens on this traffic and is re-sent in full
    on every turn thereafter. Anthropic requires the block only while its own
    tool loop is still open — the last assistant message — so everything behind
    that is re-sent for nothing.

    It is also cheap in cache terms, which is not obvious. A message is stripped
    once, on the turn it stops being last, and never changes again. That breaks
    the prefix at a point one turn from the tail, which is inside the span being
    rewritten anyway; the older prefix is untouched and keeps hitting.
    """
    messages = body.get("messages") or []
    last = max((i for i, m in enumerate(messages)
                if m.get("role") == "assistant"), default=-1)
    for i, message in enumerate(messages):
        if i == last or not isinstance(message.get("content"), list):
            continue
        message["content"] = [b for b in message["content"]
                              if not (isinstance(b, dict) and b.get("type") in THINKING)]
    return body


@strategy("strip-all-thinking")
def strip_all_thinking(body):
    """Drop every reasoning block, including the newest.

    The upper bound on what stripping can buy, and position-independent, so no
    message ever changes shape. Not shippable as-is: an open tool loop needs its
    signed block back or the API rejects the turn.
    """
    for message in body.get("messages") or []:
        if isinstance(message.get("content"), list):
            message["content"] = [b for b in message["content"]
                                  if not (isinstance(b, dict) and b.get("type") in THINKING)]
    return body


@strategy("strip-thinking-cache-behind")
def strip_thinking_cache_behind(body):
    """Strip reasoning except the open tool loop, and keep every breakpoint behind it.

    `strip-old-thinking` measured +285% and the reason generalises. Each cached
    prefix was written on the turn its own tail message was still carrying
    reasoning; strip that message a turn later and the entry is dead. Not the
    newest entry — every entry ever written, all the way back. The break is only
    six messages deep and still costs the whole conversation, which is why
    depth-of-break is a misleading thing to measure.

    So the last assistant message keeps its signed blocks, because the API wants
    them, and no breakpoint is allowed to sit at or after it. Nothing cached ever
    contains a block that will later be removed, so nothing is ever invalidated.
    What falls outside the last breakpoint bills fresh, but that is the newest
    turn, which bills fresh regardless.
    """
    messages = body.get("messages") or []
    last = max((i for i, m in enumerate(messages)
                if m.get("role") == "assistant"), default=-1)
    for i, message in enumerate(messages):
        if i == last or not isinstance(message.get("content"), list):
            continue
        message["content"] = [b for b in message["content"]
                              if not (isinstance(b, dict) and b.get("type") in THINKING)]

    _clear_message_markers(body)
    # The marker goes on the last block that is safe forever: the end of the
    # message before the preserved one.
    safe = [b for m, b in _content_blocks(body)
            if messages.index(m) < last] if last > 0 else []
    if safe:
        safe[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


@strategy("tail-5m-stable-1h")
def tail_5m_stable_1h(body):
    """1h on the system/tools prefix, 5m on the moving message tail.

    A message-tail breakpoint moves forward every turn, so what it writes is read
    by the next turn and then superseded. The median gap between turns is fifteen
    seconds. Buying an hour of retention for it costs 2.0x instead of 1.25x and
    is never claimed. The system prefix is the opposite case — stable for the
    whole session, read hundreds of times — so it keeps the long TTL.
    """
    system = body.get("system")
    if isinstance(system, list):
        for block in system:
            if isinstance(block, dict) and isinstance(block.get("cache_control"), dict):
                block["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    for _, block in _content_blocks(body):
        if isinstance(block.get("cache_control"), dict):
            block["cache_control"] = {"type": "ephemeral", "ttl": "5m"}
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


# --- offload floor -------------------------------------------------------
#
# `--ctx-offload-min-bytes` decides which `tool_result` blocks offload can even
# see. It was tuned when Read results dominated; on Bash-heavy traffic 292 of
# 301 blocks in a captured body sat UNDER the 4,000-byte floor, so 79% of the
# tool_result bytes were unreachable whatever the gate did.
#
# The digest shape is copied from `ctx_offload.rs`: keep a quarter of the block,
# clamped to [600, 3072] bytes, then a fixed retrieval footer. Blocks near the
# floor barely shrink, which is exactly what these arms are here to price.

# Anthropic accepts four cache_control markers per request, total.
MAX_BREAKPOINTS = 4

PREVIEW_CEILING = 3072
PREVIEW_FLOOR = 600
# `digest_footer` in the proxy: marker, hash and the retrieval pointer sentence.
FOOTER_BYTES = 180
# The proxy leaves the live tail alone; `--ctx-offload-stale-messages 4`.
STALE_AFTER = 4



def _tool_result_text(block):
    content = block.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(c.get("text", "") for c in content
                       if isinstance(c, dict) and c.get("type") == "text")
    return None


def _write_digest(block, text) -> bool:
    """Replace a tool_result with its preview + pointer. False if it would grow."""
    keep = min(max(len(text) // 4, PREVIEW_FLOOR), PREVIEW_CEILING)
    footer = "\n…[truncated — retrieval pointer below]\n<headroom:offloaded "
    digest = text[:keep] + footer + "x" * max(0, FOOTER_BYTES - len(footer))
    if len(digest) >= len(text):
        return False
    if isinstance(block.get("content"), str):
        block["content"] = digest
    else:
        block["content"] = [{"type": "text", "text": digest}]
    return True


def _offload_at(body, min_bytes):
    """Emulate ctx_offload with the floor moved to `min_bytes`."""
    messages = body.get("messages") or []
    cutoff = len(messages) - STALE_AFTER
    for i, message in enumerate(messages):
        if i >= cutoff or not isinstance(message.get("content"), list):
            continue
        for block in message["content"]:
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            content = block.get("content")
            if isinstance(content, list):
                parts = [c for c in content if isinstance(c, dict) and c.get("type") == "text"]
            elif isinstance(content, str):
                parts = None
            else:
                continue
            text = content if parts is None else "".join(c.get("text", "") for c in parts)
            if len(text) < min_bytes:
                continue
            keep = min(max(len(text) // 4, PREVIEW_FLOOR), PREVIEW_CEILING)
            # Carry the proxy's real marker, padded to the real footer length.
            # `cachesim.py damage` classifies a change by the signature it finds,
            # so a placeholder footer files the whole arm under "rewritten" and
            # makes it unreadable next to the live proxy.
            footer = f"\n…[truncated — retrieval pointer below]\n<headroom:offloaded "
            digest = text[:keep] + footer + "x" * max(0, FOOTER_BYTES - len(footer))
            if len(digest) >= len(text):
                continue          # never inflate a block
            if parts is None:
                block["content"] = digest
            else:
                block["content"] = [{"type": "text", "text": digest}]
    return body


@strategy("offload-floor-4000")
def offload_floor_4000(body):
    """The live setting, as the control for the arms below."""
    return _offload_at(body, 4000)


@strategy("offload-floor-2000")
def offload_floor_2000(body):
    """Halve the floor. A 2,000-byte block digests to ~780, so it still pays."""
    return _offload_at(body, 2000)


@strategy("offload-floor-1200")
def offload_floor_1200(body):
    """Near the point where the 600-byte preview floor stops shrinking anything."""
    return _offload_at(body, 1200)


@strategy("offload-floor-800")
def offload_floor_800(body):
    """Past it: most blocks here cannot shrink, so this should lose."""
    return _offload_at(body, 800)



# --- stepped thinking anchor --------------------------------------------
#
# `strip-old-thinking` scores +59.9% because its boundary moves every turn: the
# message that was last is stripped on the next turn, so the bytes change one
# turn behind the tail and the trailing segment is rewritten each time.
# `strip-all-thinking` scores -12.7% because nothing ever changes shape — but it
# is unshippable, an open tool loop needs its signed block back.
#
# The difference is not what is stripped, it is how often the strip line moves.
# Anchor it to a multiple of STEP and it moves once every STEP messages instead
# of every turn, so the prefix breaks once and then holds. The tail — including
# any open tool loop — is never touched.


def _stepped_thinking(body, step):
    messages = body.get("messages") or []
    anchor = (len(messages) // step) * step
    for i, message in enumerate(messages):
        if i >= anchor or not isinstance(message.get("content"), list):
            continue
        message["content"] = [b for b in message["content"]
                              if not (isinstance(b, dict) and b.get("type") in THINKING)]
    return body


@strategy("thinking-anchor-25")
def thinking_anchor_25(body):
    """Strip behind an anchor that advances every 25 messages."""
    return _stepped_thinking(body, 25)


@strategy("thinking-anchor-50")
def thinking_anchor_50(body):
    """As above, advancing every 50 — fewer breaks, more thinking retained."""
    return _stepped_thinking(body, 50)


@strategy("thinking-anchor-100")
def thinking_anchor_100(body):
    """Rarest breaks. Approaches noop on conversations shorter than 100."""
    return _stepped_thinking(body, 100)



# --- strip old thinking, with the breakpoints moved in front of the cut ---
#
# `strip-old-thinking` is +59.9% not because of what it removes but because of
# where the removal lands relative to the cache markers. A change at message N
# invalidates every breakpoint at or after N, and the forwarded body puts its
# tail breakpoints past the strip line, so each turn buys a full rebuild.
#
# Prefixes only care about what precedes them. Put the last breakpoint *before*
# the strip line and the edited region falls into the live zone, which is
# uncached every turn regardless. If the break is what costs, this recovers it;
# if the removal itself is the problem, this scores no better and the thinking
# lever is dead.


@strategy("strip-old-thinking-safe-marks")
def strip_old_thinking_safe_marks(body):
    """One marker immediately before the cut. The crude version, kept as the
    control for the arm below: it proves the break is what costs, at the price
    of leaving everything past that single point uncached."""
    last = _strip_behind_last_assistant(body)
    if last is None:
        return body
    _clear_message_markers(body)
    _mark_last_block_before(body, last)
    return body


@strategy("strip-old-thinking-spread-marks")
def strip_old_thinking_spread_marks(body):
    """Same cut, but spending the whole breakpoint budget in front of it.

    Anthropic allows four markers per request and the single-marker version
    wastes the rest, which is why it pushed uncached from 1.4% to 13.8%: one
    anchor caches one prefix, and everything after it is fresh every turn.
    Spreading the remainder back through the history gives shorter prefixes
    that still hit when an edit lands near the tail.

    Every marker stays strictly before the strip line. A marker after the cut
    is the +59.7% arm.
    """
    last = _strip_behind_last_assistant(body)
    if last is None:
        return body
    _clear_message_markers(body)
    messages = body.get("messages") or []
    eligible = []
    for message in messages[:last]:
        content = message.get("content")
        if not isinstance(content, list):
            continue
        eligible.extend(b for b in content if isinstance(b, dict))
    if not eligible:
        return body
    # The client's system markers are left alone and count against the same
    # limit of four, so only spend what they leave.
    budget = max(1, MAX_BREAKPOINTS - _system_marker_count(body))
    n = len(eligible)
    # Quarters, nearest the cut first — the spacing rule from `_tail_breakpoints`,
    # anchored to the cut instead of to the tail.
    for i in range(budget):
        index = n - 1 - round(i * n / (4 * budget))
        if 0 <= index < n:
            eligible[index]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


def _system_marker_count(body: dict) -> int:
    system = body.get("system")
    if not isinstance(system, list):
        return 0
    return sum(1 for b in system if isinstance(b, dict) and b.get("cache_control"))


@strategy("strip-old-thinking-tail-marks")
def strip_old_thinking_tail_marks(body):
    """Same cut, markers placed the way the client places them: at the tail.

    Holding every marker in front of the cut leaves the newest turn — the
    assistant message and the tool_results answering it — outside every cached
    prefix, which is the whole of that 13.8% uncached. But the cut only ever
    moves by one turn, so a tail marker's prefix is stable from the turn after
    it is written, exactly as it is for the client.
    """
    if _strip_behind_last_assistant(body) is None:
        return body
    return _tail_breakpoints(body, MAX_BREAKPOINTS - _system_marker_count(body))


def _strip_behind_last_assistant(body):
    """Drop reasoning behind the newest assistant message. Returns its index."""
    messages = body.get("messages") or []
    last = max((i for i, m in enumerate(messages)
                if m.get("role") == "assistant"), default=-1)
    if last < 0:
        return None
    for i, message in enumerate(messages):
        if i == last or not isinstance(message.get("content"), list):
            continue
        message["content"] = [b for b in message["content"]
                              if not (isinstance(b, dict) and b.get("type") in THINKING)]
    return last


def _mark_last_block_before(body, limit):
    messages = body.get("messages") or []
    for i in range(limit - 1, -1, -1):
        content = messages[i].get("content")
        if isinstance(content, list) and content and isinstance(content[-1], dict):
            content[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
            return True
    return False


# Whether a strategy is session-aware, by name. `inspect.signature` costs more
# than some strategies do and the answer never changes.
_TAKES_TURN: dict[str, bool] = {}


def apply(name: str, body: dict, turn=None, owned: bool = False) -> dict:
    """Run a strategy on a deep copy, so arms cannot contaminate each other.

    A strategy taking a second parameter is session-aware and gets the turn.
    Those carry state across turns, so callers must `reset()` between arms.

    Pass `owned` when the body was parsed for this call and nobody else holds a
    reference to it. The copy exists only to keep one arm out of another's way,
    so a caller that streams a fresh body per arm has already paid for that
    separation and the copy is pure waste — it was 44% of an experiment run.
    Nothing here keeps a reference to a body between turns, so an owned body is
    safe to rewrite where it lies.
    """
    fn = REGISTRY[name]
    takes_turn = _TAKES_TURN.get(name)
    if takes_turn is None:
        takes_turn = len(inspect.signature(fn).parameters) > 1
        _TAKES_TURN[name] = takes_turn
    if not owned:
        body = copy.deepcopy(body)
    return fn(body, turn) if takes_turn else fn(body)

# --- Boundary-gate arms -----------------------------------------------------
#
# `ctx_offload`'s PR-J4 gate defers a frozen block's FIRST conversion unless the
# turn is a rebuild boundary. These arms price that gate by modelling it, and
# then by modelling its absence, over the same corpus.
#
# Live config, from ~/.headroom-flags.sh: stale_margin 4, stale_window 4, so a
# block within 8 messages of the tail converts freely and anything deeper waits.
STALE_MARGIN = 4
STALE_WINDOW = 4

# Per-session state. `apply` resets it between arms; turns arrive in `seq`
# order, so per-session order holds even though sessions interleave.
_SEEN: dict[str, set] = {}
_PREV: dict[str, bool] = {}


def reset() -> None:
    _SEEN.clear()
    _PREV.clear()


def _is_rebuild_boundary(session_key, body) -> bool:
    """Did the drift detector call this turn a hot-zone rebuild?

    Not inferred from the body. A first guess — "any change at a position both
    turns share" — scored 98.5% of turns as boundaries, because Claude Code
    rewrites its own reminders constantly. The live counters say otherwise:
    over 2,571 logged turns `rebuild_boundary` was true 4 times (0.16%), while
    `blocks_deferred` summed to 2,494. First conversions arriving by boundary
    were 5; by the near-tail window, 216.

    So the honest model is that a boundary never comes. A session's first turn
    counts, since nothing is cached yet, and that alone reproduces the observed
    rate.
    """
    first = session_key not in _PREV
    _PREV[session_key] = True
    return first


# `headroom_core::tool_exclusion`, mirrored. Two lists, two strengths.
#
# VERBATIM results must stay byte-faithful at any distance from the tail: the
# retrieval tool's own output is here, and offloading it reopens the loop the
# retrieval closed.
VERBATIM_EXCLUDE = {"websearch", "webfetch", "web_search", "web_fetch",
                    "headroom_retrieve"}
# `--exclude-tools` default CSV. Weaker: `excluded_unless_prior = excluded &&
# !stale`, so this only protects a block while it is inside `stale_margin` of
# the tail. Past that the operator's "don't summarise what I'm working with"
# argument has expired.
EXCLUDE_TOOLS = {"read", "glob", "grep", "write", "edit", "websearch",
                 "webfetch", "headroom_retrieve"}


def _tool_aliases(name):
    """Bare name plus MCP wrapper spellings, as `tool_name_aliases` does."""
    out = {name.lower()}
    parts = name.split("__", 2)
    if len(parts) == 3 and parts[0].lower() == "mcp" and parts[1] and parts[2]:
        out.add(parts[2].lower())
        out.add(f"mcp_{parts[1]}_{parts[2]}".lower())
    return out


def _tool_names_by_id(body):
    """tool_use_id -> tool name, which is where the exclusion lists are read."""
    names = {}
    for message in body.get("messages") or []:
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_use":
                names[block.get("id")] = block.get("name") or ""
    return names


def _offload_gated(body, turn, min_bytes, honour_boundary: bool,
                   honour_exclusions: bool = True,
                   honour_verbatim: bool = True):
    messages = body.get("messages") or []
    if not messages:
        return body
    session = getattr(turn, "session_key", "")
    boundary = _is_rebuild_boundary(session, body)
    seen = _SEEN.setdefault(session, set())
    names = (_tool_names_by_id(body)
             if honour_exclusions or honour_verbatim else {})
    last_idx = len(messages) - 1
    for i, message in enumerate(messages):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        distance = last_idx - i
        is_live = distance == 0
        near_tail = distance < STALE_MARGIN + STALE_WINDOW
        for block in content:
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            text = _tool_result_text(block)
            if text is None or len(text) <= min_bytes:
                continue
            aliases = _tool_aliases(names.get(block.get("tool_use_id"), ""))
            if honour_verbatim and aliases & VERBATIM_EXCLUDE:
                continue                # byte-faithful at every distance
            key = hashlib.sha256(text.encode()).digest()
            prior = key in seen
            # `excluded_unless_prior`: an excluded tool is protected only while
            # it is still within `stale_margin` of the tail.
            stale = distance >= STALE_MARGIN
            if (honour_exclusions and not stale and not prior
                    and aliases & EXCLUDE_TOOLS):
                continue
            # The gate, verbatim from `offload_tool_result`: a first conversion
            # of a frozen block rides a boundary or it waits.
            if not prior and not is_live and not near_tail:
                if honour_boundary and not boundary:
                    continue
            seen.add(key)
            _write_digest(block, text)
    return body


@strategy("offload-gated-2000")
def offload_gated_2000(body, turn):
    """The live PR-J4 policy, modelled. The control for the arm below."""
    return _offload_gated(body, turn, 2000, honour_boundary=True)


@strategy("offload-ungated-2000")
def offload_ungated_2000(body, turn):
    """The same policy with the rebuild-boundary requirement dropped.

    Everything else the gate does is kept: the live tail and the near-tail
    window still convert freely, and the per-session set still makes a
    conversion monotonic, so nothing ever flips back from digest to raw.
    The only change is that a frozen block's first conversion no longer waits
    for a turn the client was rebuilding anyway.
    """
    return _offload_gated(body, turn, 2000, honour_boundary=False)

@strategy("offload-gated-2000-no-exclusions")
def offload_gated_2000_no_exclusions(body, turn):
    """The gated arm with the exclusion lists switched off — the earlier,
    over-optimistic model, kept so the two can be read side by side."""
    return _offload_gated(body, turn, 2000, honour_boundary=True,
                          honour_exclusions=False)


@strategy("offload-gated-2000-no-tool-list")
def offload_gated_2000_no_tool_list(body, turn):
    """Only the `--exclude-tools` half lifted; the verbatim list still holds.

    Splits the two claims in `no-exclusions`. The tool list already stops
    protecting a block `stale_margin` messages back, so lifting it can only
    reach blocks within four of the tail. Whatever the full arm saves beyond
    this line comes from the verbatim list, which is a different bet: those
    results break when their bytes change at any distance."""
    return _offload_gated(body, turn, 2000, honour_boundary=True,
                          honour_exclusions=False, honour_verbatim=True)


@strategy("offload-gated-4000")
def offload_gated_4000(body, turn):
    """The gated arm at the floor the capture actually ran under, for the
    like-for-like against the live proxy."""
    return _offload_gated(body, turn, 4000, honour_boundary=True)



# --- Where the second message breakpoint goes -------------------------------
#
# Live, both message breakpoints land within one message of the tail on every
# request measured (634 of 634, at 99.4% and 100% of history). Adjacent markers
# cache nearly the same prefix, so the second one buys almost nothing. These
# arms keep the count at two and leave the client's system markers alone — only
# the earlier marker moves back, by a fraction of the history.
def _spread_pair(body, back):
    _clear_message_markers(body)
    blocks = [b for _, b in _content_blocks(body)]
    if not blocks:
        return body
    blocks[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    index = int(round((len(blocks) - 1) * (1.0 - back)))
    if 0 <= index < len(blocks) - 1:
        blocks[index]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


for _back in (0.02, 0.05, 0.10, 0.17, 0.25, 0.35, 0.50):
    strategy(f"pair-back-{int(_back * 100):02d}")(
        (lambda b: lambda body: _spread_pair(body, b))(_back)
    )


def _spread_shipped(body, back):
    """What the proxy actually does, guards and all.

    Claude Code spends three of Anthropic's four markers: two on `system` and
    one on the last message block. The fourth is free, so this adds a second
    message marker a fraction of the history back and touches nothing else.
    It copies the client's own `cache_control` value rather than writing one,
    because `pair-back-*` above pins both markers to a 1h TTL and that is a
    separate lever whose effect would otherwise be read as this one's.

    Skips a request whose message markers are not exactly one: that is a
    placement this was never measured against, and the proxy refuses it too.
    """
    if not (0.0 < back < 1.0):
        return body
    positions, marked = [], []
    for message in body.get("messages") or []:
        content = message.get("content")
        if not isinstance(content, list):
            continue                    # a bare string carries no block
        for block in content:
            if not isinstance(block, dict):
                continue
            if "cache_control" in block:
                marked.append(len(positions))
            positions.append(block)
    if len(marked) != 1 or not positions:
        return body
    target = int(round((len(positions) - 1) * (1.0 - back)))
    if target >= marked[0]:
        return body                     # nothing earlier to add
    positions[target]["cache_control"] = copy.deepcopy(
        positions[marked[0]]["cache_control"]
    )
    return body


for _back in (0.02, 0.05, 0.10, 0.17, 0.25, 0.35, 0.50):
    strategy(f"shipped-back-{int(_back * 100):02d}")(
        (lambda b: lambda body: _spread_shipped(body, b))(_back)
    )


def _marked_positions(body):
    """Message content blocks in order, and which of them carry a marker."""
    positions, marked = [], []
    for message in body.get("messages") or []:
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict):
                continue
            if "cache_control" in block:
                marked.append(len(positions))
            positions.append(block)
    return positions, marked


@strategy("shipped-tail")
def shipped_tail(body):
    """Push the client's single message marker onto the final content block.

    `pair-back-*` does this as a side effect of clearing and re-placing, and
    scores 0.0% uncached everywhere. This arm isolates that half so the two
    can be told apart: whatever it recovers is not the fourth breakpoint.
    """
    positions, marked = _marked_positions(body)
    if len(marked) != 1 or marked[0] == len(positions) - 1:
        return body
    positions[-1]["cache_control"] = positions[marked[0]].pop("cache_control")
    return body


@strategy("shipped-tail-back-05")
def shipped_tail_back_05(body):
    """Both halves: marker on the final block, second one 5% back."""
    return _spread_shipped(shipped_tail(body), 0.05)


def _spread_wire(body, back):
    """Move the earlier of the two markers the wire carries back through history.

    `_spread_shipped` only fires on a request carrying exactly one message
    marker, which is what the *client* sends. What goes upstream carries two —
    the client's plus the proxy's tail one — and they land one block apart on
    97% of turns, which is the case its own comment calls worthless: adjacent
    markers cache nearly the same prefix. So on `--base forwarded` that arm
    skipped every request and scored identical to the live proxy, which read as
    "no effect" and was really "never ran".

    This works on the pair that actually goes out. The tail marker stays; the
    other moves back `back` of the history. TTL is carried, not rewritten, so
    this is one lever and not three.
    """
    if not (0.0 < back < 1.0):
        return body
    positions, marked = _marked_positions(body)
    if len(marked) != 2 or not positions:
        return body
    keep = marked[-1]
    target = keep - max(1, round(back * len(positions)))
    if target < 0 or target == marked[0]:
        return body
    ctl = positions[marked[0]].pop("cache_control", None)
    if ctl is None:
        return body
    positions[target]["cache_control"] = ctl
    return body


for _b in (0.02, 0.05, 0.10, 0.25, 0.50):
    strategy(f"spread-wire-{int(_b * 100):02d}")(
        (lambda b: lambda body: _spread_wire(body, b))(_b)
    )


def _anchor_1h(body, back):
    """Spend the free fourth breakpoint on a 1h anchor deep in history.

    The fourth slot buys nothing as a second read point: every turn writes an
    entry at its own tail, so a conversation already carries a ladder of
    readable prefixes from its past turns, and an extra marker lands on a rung
    that exists. What the ladder cannot survive is an idle gap — every rung is
    5m and only use refreshes it, so six quiet minutes wipes all of them and
    the next turn rewrites the whole history.

    A 1h marker at `back` through the history is the one rung that outlives
    that gap. It costs a 2.0x write of the span it closes, once, against a
    1.25x rewrite of everything on every cold turn.
    """
    positions, marked = _marked_positions(body)
    if len(marked) != 1 or not positions:
        return body
    target = int(round((len(positions) - 1) * (1.0 - back)))
    if target >= marked[0]:
        return body
    positions[target]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
    return body


for _back in (0.10, 0.25, 0.50, 0.75):
    strategy(f"anchor-1h-{int(_back * 100):02d}")(
        (lambda b: lambda body: _anchor_1h(body, b))(_back)
    )
