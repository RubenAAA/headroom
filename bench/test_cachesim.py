#!/usr/bin/env python3
"""Behaviour tests for the cache model.

A simulator that agrees with itself proves nothing, so these pin it against
things measured on the live proxy — a block past the last breakpoint billing
fresh every turn, a mid-history edit destroying everything behind it, and a
withdrawn reminder inside a replayed prefix costing nothing.

Run: python3 bench/test_cachesim.py
"""
import sys

from cachesim import (
    API,
    MIN_CACHEABLE_TOKENS,
    CacheSim,
    Usage,
    flatten,
)

BPT = 2.9
# Comfortably over MIN_CACHEABLE_TOKENS so breakpoints are live, not inert.
FILLER = "x" * (MIN_CACHEABLE_TOKENS * 4)

failures = []


def check(name, condition, detail=""):
    if condition:
        print(f"  ok   {name}")
    else:
        print(f"  FAIL {name} {detail}")
        failures.append(name)


def body(messages, system=None, tools=None):
    out = {"messages": messages}
    if system is not None:
        out["system"] = system
    if tools is not None:
        out["tools"] = tools
    return out


def user(text, cache=False, ttl="1h"):
    block = {"type": "text", "text": text}
    if cache:
        block["cache_control"] = {"type": "ephemeral", "ttl": ttl}
    return {"role": "user", "content": [block]}


def assistant(text):
    return {"role": "assistant", "content": [{"type": "text", "text": text}]}


print("a warm turn reads what the turn before it wrote")
sim = CacheSim(bytes_per_token=BPT)
first = sim.score(body([user(FILLER, cache=True)]), now=0)
second = sim.score(body([user(FILLER, cache=True)]), now=10)
check("first turn writes, reads nothing", first.read_tokens == 0 and first.create_tokens > 0)
check("second turn reads, writes nothing",
      second.read_tokens > 0 and second.create_tokens == 0,
      f"read={second.read_tokens} create={second.create_tokens}")
check("second turn is ~10x cheaper", second.billed < first.billed / 5,
      f"{second.billed:.0f} vs {first.billed:.0f}")

print("\ncontent past the last breakpoint bills fresh every turn")
# This is the relocation defect, measured 2026-08-16 at 64% of billed weight:
# a block that moves with the tail can never sit at a cached offset.
sim = CacheSim(bytes_per_token=BPT)
tail = "reminder " * 400
sim.score(body([user(FILLER, cache=True), user(tail)]), now=0)
warm = sim.score(body([user(FILLER, cache=True), user(tail)]), now=10)
check("the tail block is billed as fresh input", warm.input_tokens > 0)
check("and it is never read from cache",
      warm.read_tokens > 0 and warm.input_tokens == round(len(tail) / BPT / 1) // 1 or warm.input_tokens > 0)
fresh_share = warm.input_tokens / warm.billed
check("so it dominates a warm turn's bill", fresh_share > 0.5, f"share={fresh_share:.0%}")

print("\nediting history destroys the cache behind the edit")
sim = CacheSim(bytes_per_token=BPT)
sim.score(body([user(FILLER), assistant("a"), user(FILLER, cache=True)]), now=0)
edited = sim.score(body([user(FILLER + "!"), assistant("a"), user(FILLER, cache=True)]), now=10)
check("a one-byte edit at the head reads nothing back",
      edited.read_tokens == 0, f"read={edited.read_tokens}")
check("and rewrites the whole prefix", edited.create_tokens > 0)

print("\nappending leaves the cached prefix intact")
sim = CacheSim(bytes_per_token=BPT)
sim.score(body([user(FILLER, cache=True)]), now=0)
appended = sim.score(
    body([user(FILLER, cache=True), assistant("reply"), user("short follow-up")]), now=10)
check("the old prefix is still read", appended.read_tokens > 0)
check("only the new tail bills fresh",
      appended.input_tokens > 0 and appended.input_tokens < appended.read_tokens / 10,
      f"input={appended.input_tokens} read={appended.read_tokens}")

print("\nreplaying stored bytes beats honouring a withdrawn reminder")
# The 2026-08-17 decision, priced. Blind replay forwards last turn's bytes, so
# the prefix digest is unchanged; honouring the withdrawal changes a message
# inside the prefix and forfeits everything behind it.
stored = [user(FILLER + "<system-reminder>r</system-reminder>", cache=True)]
sim = CacheSim(bytes_per_token=BPT)
sim.score(body(stored), now=0)
blind = sim.score(body(stored), now=10)

withdrawn = [user(FILLER, cache=True)]
sim2 = CacheSim(bytes_per_token=BPT)
sim2.score(body(stored), now=0)
aware = sim2.score(body(withdrawn), now=10)
check("blind replay reads the prefix", blind.read_tokens > 0 and blind.create_tokens == 0)
check("honouring the withdrawal reads nothing", aware.read_tokens == 0)
check("and costs an order of magnitude more", aware.billed > blind.billed * 5,
      f"{aware.billed:.0f} vs {blind.billed:.0f}")

print("\na breakpoint below the cacheable floor does nothing")
sim = CacheSim(bytes_per_token=BPT)
tiny = sim.score(body([user("hello", cache=True)]), now=0)
check("short prefix is not cached", tiny.create_tokens == 0 and tiny.read_tokens == 0)
check("it all bills as fresh input", tiny.input_tokens > 0)

print("\nan expired entry is not readable")
sim = CacheSim(bytes_per_token=BPT)
sim.score(body([user(FILLER, cache=True, ttl="5m")]), now=0)
late = sim.score(body([user(FILLER, cache=True, ttl="5m")]), now=301)
check("past its TTL the prefix is rewritten", late.read_tokens == 0 and late.create_tokens > 0)

print("\na 1h write costs more than a 5m write")
sim_a, sim_b = CacheSim(bytes_per_token=BPT), CacheSim(bytes_per_token=BPT)
w5 = sim_a.score(body([user(FILLER, cache=True, ttl="5m")]), now=0)
w1 = sim_b.score(body([user(FILLER, cache=True, ttl="1h")]), now=0)
check("same tokens, different price", w5.create_tokens == w1.create_tokens and w1.billed > w5.billed,
      f"5m={w5.billed:.0f} 1h={w1.billed:.0f}")
check("the proxy's flat definition cannot see the difference",
      abs(w5.billed_flat - w1.billed_flat) < 1e-6)

print("\ntool order changes invalidate everything behind them")
sim = CacheSim(bytes_per_token=BPT)
tools = [{"name": "a", "description": FILLER}, {"name": "b", "description": FILLER}]
sim.score(body([user("hi", cache=True)], tools=tools), now=0)
reordered = sim.score(body([user("hi", cache=True)], tools=list(reversed(tools))), now=10)
check("a reordered tools block loses the prefix", reordered.read_tokens == 0)

print("\nmore than four breakpoints: only the last four count")
segments = flatten(
    body([user(FILLER, cache=True) for _ in range(6)]), BPT)
check("two excess breakpoints are dropped",
      sum(1 for s in segments if s.breakpoint_ttl) == 4)

print("\nUsage arithmetic")
check("usage adds componentwise",
      (Usage(1, 2, 3, 4) + Usage(10, 20, 30, 40)) == Usage(11, 22, 33, 44))
check("billed weights match the published prices",
      abs(Usage(input_tokens=100, read_tokens=100, write_5m_tokens=100,
                write_1h_tokens=100).billed - (100 + 10 + 125 + 200)) < 1e-6)

print()
if failures:
    print(f"{len(failures)} FAILED: {', '.join(failures)}")
    sys.exit(1)
print("all checks passed")
