# Idea: cut the unearned writes that parallel sessions make in bursts

- **Status:** ANSWERED 2026-09-21 the same day it was opened. Concurrency is
  worth 401,095 tokens over six days (6.3% of unearned); depth outweighs it
  10×. The gate extension is rejected — the burst's four conversations do
  share a head, but that head is warm and already cached, so a wider gate has
  nothing to hold. Only the free option survives: fewer concurrent sessions.
  See Findings at the bottom.
- **Source:** 2026-09-21 reading of `productive_write_pct` on the live proxy.
  Code: `cache_stabilization/prefix_stampede.rs` (`head_key`, `admit`),
  `usage_observer.rs` (`split_cache_write`, `record_unearned_write`).
  Neighbours: `recache-residual-triage.md` (the race lane),
  `recache-commit-latency-proof.md` (the race itself, confirmed).
- **Value:** 119,329 unearned tokens in five minutes, from one burst. Over the
  same six days unearned writes above the floor total 4,633,847 — 9.9% of all
  cache creation. This is the one slice of it that our own scheduling causes.
- **Next:** nothing to build. If this reopens, it needs a *new* mechanism —
  do not re-run the three measurements below, do not propose widening
  `--cache-stampede-gate` again without evidence that a shared object exists
  behind the waste, and do not propose a proxy per session (measured and
  rejected in Findings 3; it moves no load off the provider and costs the
  cross-session prefix store).
- **Exit:** taken. Gate extension rejected on the numbers in Findings; the
  free option (fewer concurrent sessions) stands as the only lever.

## What was measured

Current proxy process, 2026-09-21T08:14:58Z onward, Anthropic models only.
`productive_write_pct` read 77.9% — `earned / (earned + unearned)` over cache
writes, where `earned = min(creation, footprint_growth)`. Per-turn
reconstruction (within 5% of the live counter):

| per-turn unearned | turns | tokens | share |
|---|---|---|---|
| >10k | 10 | 411,138 | 76% |
| 1k-10k | 32 | 100,518 | 19% |
| <=1,024 (sub-floor) | 48 | 26,329 | 5% |

Two of the ten big turns are TTL expiries (291,809 tokens, idle 99 and 110
minutes) and belong to a different problem. **Eight are one burst**, all
`provider_partial_of_previous_write`, all at zero idle gap, inside a
five-minute window:

```
 26,138  09:54:17  read=212,234 create= 26,138 prev_fp=251,030
 17,730  10:57:56  read=104,916 create= 17,730 prev_fp=189,415
 15,523  10:54:31  read=181,997 create= 15,523 prev_fp=242,991
 15,238  10:55:27  read=185,975 create= 15,238 prev_fp=266,799
 13,980  10:53:52  read= 81,958 create= 13,980 prev_fp=134,353
 10,520  10:54:07  read= 85,532 create= 21,278 prev_fp= 96,052
 10,132  10:53:12  read= 77,210 create= 10,918 prev_fp= 87,342
 10,068  10:53:51  read= 85,148 create= 10,904 prev_fp= 95,216
```

Load in that window: **204 turns in ten minutes across 5 conversation keys
and 5 distinct sessions.** Three of those sessions are Claude Code windows
open on this repo at once, each with its own subagents.

## Why the existing gate does not cover it

`--cache-stampede-gate true` and `--cache-stampede-wait-cap 10s` are live.
The gate never fires: `stage_timings.stampede_wait` reads p50 0.01 ms, p90
0.01 ms, p99 0.02 ms over 23,779 turns. That is not a bug, it is scope.

`PrefixStampede::head_key` hashes `model + system + tools` and returns `None`
unless one of `system`/`tools` carries a `cache_control` marker. So the gate
protects the *opening* head against a cold fan-out. The waste above is on the
message prefix — these turns read 77k-212k tokens each, so their head was
warm and their system+tools were already cached. Different write, different
key, gate silent.

## The measurement

Two questions, in order. The second only matters if the first says yes.

1. **Is the burst causal, or is it load coinciding with depth?** The eight
   turns are deep (prev_fp 87k-267k) as well as concurrent, and deep turns
   write more whatever the load. Compare unearned-per-turn at matched depth
   across concurrency: bucket every turn by `prev_fp` decile, split by
   `stage_timings.inflight`, and look for a concurrency effect inside a
   depth bucket. `inflight` p50 is 2 and max 29, so the contrast exists.
2. **If concurrency does drive it, what is the shared object?** The burst
   turns sit on 5 *different* conversation keys, so they are not siblings of
   one stream. Either the provider degrades under our aggregate rate, or
   these keys share a prefix the current keying does not see. Join
   `turn_cache_fingerprint.prefix_ladder` / `tail_ladder` across the burst's
   request ids and check for overlap. Overlap means a wider gate has
   something to hold; no overlap means the cause is provider-side and the
   only lever is sending less.

Do not model this offline. `bench/cachesim.py` has no concurrency model, and
the last time a TTL question went to it the sign inverted on live traffic.

## What is already ruled out

**Delaying turns to dodge the commit race.** Measured over 23,217
consecutive turn pairs: unearned tokens do not sit at short gaps.

| gap | turns | unearned |
|---|---|---|
| 0-3s | 3,581 | 85,784 |
| 3-5s | 7,094 | 210,272 |
| 5-10s | 6,788 | 1,166,705 |
| 10-60s | 5,028 | 3,172,572 |
| >60s | 726 | 1,714,692 |

A delay-to-5s policy would hold 10,675 turns for 4.7 hours of added wait over
six days and touch 296,056 unearned tokens — 6%. Delay-to-10s costs 25.3
hours for 32%. Both lose, and "touch" is not "recover": the write is partial,
not absent.

The commit race is 45% of recache *events* but only 15% of recache *tokens*
(187,152 of 1,253,244 over the week). It explains why recaches happen. It
does not explain where the tokens go.

## The free option, stated so it is not skipped

Running fewer Claude Code sessions against the same repo costs nothing and
needs no code. Any proposal here has to beat that, and a gate that adds
latency to every fan-out in order to recover a burst that only happens when
three sessions overlap may not.

## Findings 2026-09-21 — the gate path is dead; sending less is the only lever

Both questions ran the same day the file was written.

### 1. Concurrency is real and small. Depth does the work.

23,217 turns with both a depth (previous footprint) and an `inflight`
reading. Mean unearned tokens per turn:

| depth (prev footprint) | inflight 1-2 | inflight 3-5 | inflight 6+ |
|---|---|---|---|
| <61,800 | 63 (n=1973) | 51 (n=1200) | 96 (n=696) |
| 61,800-82,293 | 50 (n=2047) | 41 (n=1240) | 112 (n=583) |
| 82,293-103,754 | 72 (n=1968) | 44 (n=1263) | 276 (n=638) |
| 103,754-132,807 | 247 (n=2134) | 329 (n=1163) | 201 (n=573) |
| 132,807-177,458 | 426 (n=2241) | 252 (n=1099) | 676 (n=529) |
| >=177,458 | 654 (n=2399) | 880 (n=1128) | 962 (n=343) |

Holding depth fixed and pricing the excess against inflight 1-2:

- inflight 6+: **401,095 tokens over six days, 6.3% of unearned**
- inflight 3-5: 97,105, and the sign flips across buckets — noise
- depth, at constant inflight 1-2: 63 → 654, a **10× swing**

So the burst is mostly deep turns that happened to be concurrent. Concurrency
adds a real but minor surcharge, and it only shows up at inflight 6+, which
is 3,362 of 23,217 turns.

### 2. The keys do share a head. The head is not what gets rewritten.

Across the 204 burst turns, **200 share one `system_digest` and one
`tools_digest`** over four separate conversation keys — the fan-out shape the
gate was built for. But their `prefix_ladder` rungs diverge per conversation
(`1:daf3` / `1:947c` / `1:0697`), so only the head is common.

That head is warm all day and already cached, which is why the gate is
silent: it holds followers under a **cold** key, and `LeaderWarm` lets
everything else straight through. Over six days it fired **once** —
`stampede_follower_released`, `waited_ms=94`, `release=leader_warm`.

The unearned writes are on per-conversation message prefixes, and nothing
shares those. **A wider gate would have nothing to hold.** Widening it to
warm heads would park followers behind an object that is already cached at
read price, buying delay and no tokens.

### 3. One proxy per session does not help. Do not build it.

The obvious next proposal, answered on the same data.

**The proxy is not the bottleneck.** Its own work gets faster as load rises
while upstream slows, which is the opposite of local contention:

| proxy inflight | n | pre_forward p50 | upstream p50 | upstream p90 |
|---|---|---|---|---|
| 1-2 | 13,023 | 62.0 ms | 1,421 ms | 2,336 ms |
| 3-5 | 7,279 | 59.9 ms | 1,489 ms | 2,558 ms |
| 6+ | 3,477 | 56.4 ms | 1,839 ms | 3,029 ms |

Pre-forward would climb under a queue or a lock. It falls. Upstream climbs
29% at p50 and 30% at p90. Splitting one proxy into N sends the same bodies
at the same instants under the same account, so the provider's view is
byte-identical and the surcharge rides along unchanged.

**The load that costs is cross-conversation, not within-conversation.** Proxy
inflight split by whether the overlap is a sibling turn of the same
conversation, mean unearned tokens per turn inside depth quartiles:

| depth q | same=0 | same=1 | same>=2 |
|---|---|---|---|
| q0 | 72 (n=5665) | 177 (n=129) | 144 (n=10) |
| q1 | 70 (n=5659) | 115 (n=142) | 80 (n=3) |
| q2 | 285 (n=5659) | 116 (n=136) | 181 (n=9) |
| q3 | 676 (n=5709) | 155 (n=94) | 402 (n=2) |

| depth q | cross 0-1 | cross 2-4 | cross 5+ |
|---|---|---|---|
| q0 | 72 | 57 | 116 |
| q1 | 52 | 33 | 211 |
| q2 | 265 | 302 | 294 |
| q3 | 616 | 683 | 943 |

Same-conversation overlap lands on about 2% of turns and does not raise
unearned — in the deepest quartile it is 155 against 676. Cross-conversation
load rises with unearned in every quartile. A conversation is not racing
itself; it reads a partial prefix while other conversations are in flight.
That is aggregate rate against the provider, which no client-side topology
changes.

**And splitting costs a mechanism that works.** 21 of 290 first turns adopted
a donor prefix from another session, which is only possible because one proxy
holds every session's prefix store. Give each session its own and those turns
start cold. The saving is not cleanly priceable here — adopted turns are
deeper than average, so their 2.2× read/write ratio against 1.4× for the rest
is partly selection — but the mechanism disappears outright. The stampede
gate is cross-session by construction too, though it fired once in six days,
so losing that costs nothing.

**Magnitude, so nobody reopens this for the wrong reason.** The entire
concurrency surcharge is 401,095 tokens over six days: 6.3% of unearned
writes and **0.86% of all cache creation**. Depth outweighs it tenfold. It is
worth knowing and not worth an architecture change.

### Verdict

Reject the gate extension and the proxy-per-session topology. The remaining
mechanism is provider-side: under our aggregate rate the provider returns a
partial read of the conversation's own previous write
(`provider_partial_of_previous_write`), which is the same staleness lane
`recache-residual-triage.md` lands in, and nothing local reaches it.

What is left is the free option: fewer concurrent sessions against the same
repo, worth about 400,000 tokens per six days. No code, and nothing else
measured here beats it.
