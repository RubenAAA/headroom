# Implemented: stream-aware conversation matching

- **Status:** fixed 2026-08-09 (settled item 11; invalidated item 21's headline)
- **Source:** `docs/notes/proxy-experiments-2026-08.md` (§11, §17/17a)
- **Summary:** `conversation_key` merged interleaved streams (message counts
  running backwards on 79% of events, 68% of booked tokens) — part of "waste"
  was cross-charged prefixes. `usage_observer` now tracks several streams per
  key (`match_stream` by count only, so prefix edits still report), plus
  decline reasons on replay skips (which exposed the `Expected` bucket as 98%
  real busts hiding below the 3-message drift window — corrected the other way).
  Both corrections required before any ratio means anything.


## Detail

*moved from `docs/notes/recache-classification.md`*

## Measured and uniform, so not discriminating

Prefix replay's stable window ends exactly one message short of the end
(`total − stable == 1`) on all 20 events. That is by design — the newest
message is new — and it holds on clean turns too, so it separates nothing on
its own. Recorded here so it is not mistaken for a finding.

Unexplained turns are *shorter* conversations than average (median 67
messages against 308) with higher output (691 tokens against 273). Neither
has an explanation yet.


## Item 11 finding

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### Item 11 — SETTLED: the key merges streams (2026-08-09)

**Reading 2. The waste is an accounting artefact, and items 3, 3a and 3c shrink
with it.** Nineteen recache events across eight keys, read off the live proxy
at 09:04–09:12Z with two Claude Code sessions running.

The field that decided it was not the one built for the job. `prefix_body` was
the designed comparator; `prefix_stable_msgs` — added so a reader could tell
whether two `stable` values covered the same span — is what proved the case,
because it counts messages and **a conversation only ever grows**. Three of the
five keys with more than one event have a count that runs *backwards*:

```
conv 135358e7efd5   msgs 17 → 16 → 35 → 28      4 events, 1 body,  36,888 tok
conv af7a42fd7eb2   msgs 12 → 10 → 24 → 18      4 events, 2 bodies, 46,489 tok
conv c0ec5505341b   msgs 12 → 16 → 27 → 24      4 events, 3 bodies, 170,754 tok
```

No single conversation can shrink by a message and then jump by nineteen. Read
`af7a42fd7eb2` as two series and it resolves at once — 12, 24, 38, 47, 60 under
one `prefix_body`, and 10, 18, 28, 34, 44 under another. Two streams, each
growing normally, interleaved under one key. That is also **item 5's
flip-flop**: "two prefixes, never a third" is two streams, not gradual drift.

`135358e7efd5` is the harder case and the reason the body hash alone could not
settle this: both its streams carry an *identical* first-8 fingerprint, so only
the count separates them. A subagent that inherits its parent's context shares
the opener by construction.

**Scale.** Over the full 17-minute window, 48 events across 12 keys: **38 of
them (79%), carrying 926,101 of 1,357,739 booked tokens (68%), sit on keys
whose message count goes backwards.** It reaches the waste-counted class too —
`c0ec5505341b` is three-quarters `drift` kind — so this is not confined to the
`expected` events already excluded from the totals.

**Fixed.** `usage_observer` now tracks several streams per key and classifies
each turn against the one it continues (`match_stream`), picking the longest
tracked stream no longer than the turn in hand. Matching deliberately uses the
count and nothing else: also requiring the early-message fingerprint to agree
would blind the watchdog to an *edit* inside the cached prefix, which moves
those bytes while leaving the count alone — the same reason `system` is already
kept out of `conversation_key`. `an_edit_inside_the_prefix_is_still_reported_as_a_bust`
pins that, and the two live sequences above are replayed as tests.

**Expect the reported waste to fall.** That is the point. Re-derive item 3's
1.3x spend-to-save ratio on the new numbers before quoting it.


## Item 11a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### Item 11a — explained, not a collision

The "pinned value crossing two distinct keys" has a duller cause than a hash
collision. All four early events carried the same `prefix_head`
(`553665a182ec00dc`) — the same model, system and tools block — so
`expected_cache_read` measured the same cached block in each, landing at 41,023
and 41,020 on different conversations. Two conversations sharing a system
prompt and a tool list will agree on that number without sharing anything else.
A second head (`ba02a1f22f73`) shows up on the other session, so the field does
discriminate.


## Deciding-test design

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### The deciding test, as built (2026-08-09)

`cache_recache_observed` carries three new fields next to `conversation_key`:

| field | covers |
| --- | --- |
| `prefix_head` | `model` + `system` + `tools` — the block that *does* cache |
| `prefix_body` | first 8 messages, **fixed depth** |
| `prefix_stable` + `prefix_stable_msgs` | all but the live tail, and its depth |

**How to read it.** Take two alternating turns under one `conversation_key`:

- same `prefix_head`, **different** `prefix_body` → the streams genuinely
  diverge past the tools block. **Reading 1, real thrash, real money** — item
  3's waste stands and the fix is transform determinism under concurrency.
- same `prefix_head`, **same** `prefix_body` → identical cacheable bytes under
  one key, so the key merged two streams upstream treats separately.
  **Reading 2, artefact** — item 3's totals shrink and the fix is a finer key.

**Why fixed depth.** The obvious design — hash everything but the live tail —
decides nothing: that region grows by one message per turn, so two turns of one
conversation never agree and the field can only ever print "different". A test
caught this before it shipped. `prefix_stable` is still emitted with its depth
for the equal-length pairs item 11 mostly has, but `prefix_body` is the
comparator. It is empty below 8 messages, which means "not comparable yet",
never "no difference".

Depth is measured from the opener because that is where a merged key hides:
`conversation_key` is `(model, first message)`, so two subagents merged by it
share message 0 by construction and must be told apart by what follows.

**Cost.** The fingerprint samples rather than serialises — per text fragment,
the exact byte length plus the leading 64 bytes, hashed in place. A full
re-serialise of a 1.4 MB body would have cost more than the whole optimisation
stage it sits in (`opt_ms` median 11ms).

**Answered on the second run, under two concurrent sessions** — see the settled
section above. Worth recording what the design got wrong: `prefix_body` was
built as the comparator and `prefix_stable_msgs` was added as a footnote, and
it was the footnote that carried the proof. The body hash is ambiguous on its
own, because two streams forked from a shared opener agree on it. A cheap
field that cannot be argued with beat a carefully reasoned one.


## Item 17 + 17a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 17 — NEW: turns that skip prefix replay carry nearly all the waste

Not in the original notes; found on 2026-08-09 by joining
`cache_recache_observed` to `prefix_replay_applied` on `request_id`.

| prefix replayed | share of requests | waste booked | per event |
| --- | --- | --- | --- |
| **false** | **19%** | **10.45M tokens** | ~80,500 |
| true | 81% | 0.32M tokens | ~2,300 |

A turn that replays its cached prefix wastes almost nothing. A turn that does
not wastes 35 times more. Whatever is worth fixing about cache spend is in that
19%.

`replayed_prefix` is a boolean, and `overlay_cached_prefix` declines for **five
different reasons** that need opposite responses. A client that rewrote its own
early messages is not our doing and there is nothing to fix; a turn *shorter*
than the stored prefix means two streams are sharing one session slot, which is
item 11 costing real tokens rather than merely mis-reporting them. The log could
not tell them apart.

`overlay_cached_prefix_reported` now returns the reason and the proxy logs
`prefix_replay_not_replayed` with it, plus the three message counts. Each reason
has a unit test, including the shorter-than-stored case.

**17a — FIXED: the "expected" bucket was mostly real busts.** The reason field
immediately exposed a defect in the classification itself. `event_kind` is
derived from `drift_dims`, which covers `system`, `tools` and the first three
messages only. A prefix that diverges deeper is invisible to it, so the event
falls through to `Expected` — "no cause found" — which this document then writes
off as a session reset (subagent close, `/clear`) and **excludes from waste
totals**. Joining the two events over the 2026-08-08/09 logs:

```
recache events classified "expected"        161
  ... where prefix replay was declined      102  (63% of events)
  ... tokens in those                       8,387,833 of 8,524,807  (98%)
```

Those are not session resets. They are prefix busts whose cause sat below the
drift window. `note_replay_skip` now carries the reason onto the pending turn
and a declined replay counts as a named cause, so these classify as `Drift` and
the event carries `replay_skipped=<reason>`.

**This moves tokens INTO the waste column**, the opposite direction to item 11's
fix, and both corrections are needed before item 3's ratio means anything: 11
removed waste that was double-attributed across merged streams, 17a adds waste
that was dismissed as benign. Do not quote either total until a clean window has
run with both live. A companion test pins the other half of the rule — no drift
dims *and* no declined replay still classifies `Expected`, so the two buckets
keep meaning different things.

**Still to measure: which reason dominates.** Read the
reason histogram off a run of the new binary before choosing a fix; the
candidates differ by an order of magnitude in both cost and difficulty. Three
hypotheses were tested and dropped getting here, so do not skip the measurement:
sessions holding several conversation keys (16 sessions, none did), tool pruning
varying per request (stable per tool-set size), and drift invalidating the
stored prefix (`invalidate()` is never called from the proxy).


## Item 11 raw

*moved from `docs/notes/proxy-experiments-2026-08.md`*

## 11. Two concurrent streams share one `conversation_key`, and one never caches

The clearest single finding of the run. Between 21:25 and 21:30 the
conversation key `f673924de6d343d5` produced 19 recache events totalling
**498,367 wasted tokens**. A second key, `b910081e3d2d617f`, produced 18
events totalling 758,731 over the same minutes. Six conversations accounted
for all 41 events; two of them carried 92% of the waste.

Inside `f673924de6d343d5` the events strictly alternate. Odd turns waste
almost nothing; even turns waste nearly the whole prefix:

```
21:26:58  actual_cache_read=13907   wasted=57304   msgs=55
21:27:00  actual_cache_read=71211   wasted=   71   msgs=53
21:27:33  actual_cache_read=13907   wasted=69893   msgs=71
21:27:39  actual_cache_read=83800   wasted=  245   msgs=71
21:28:07  actual_cache_read=13907   wasted=74414   msgs=83
21:28:13  actual_cache_read=88321   wasted=  242   msgs=83
21:28:43  actual_cache_read=13907   wasted=80160   msgs=99
21:28:43  actual_cache_read=94067   wasted= 1866   msgs=97
21:29:16  actual_cache_read=13907   wasted=85490   msgs=111
21:29:21  actual_cache_read=99397   wasted=  560   msgs=109
21:29:50  actual_cache_read=13907   wasted=89134   msgs=121
21:30:07  actual_cache_read=103041  wasted=  814   msgs=119
```

`actual_cache_read` on the bad turns is **exactly 13,907 every time**, while
the conversation grows from 55 to 121 messages. A cache read that does not
grow with the conversation means that stream matches only the leading block —
system plus tools — and nothing after it. The good turns track the
conversation's real size, so the cache itself is working.

Both streams are `claude-sonnet-5`, both dispatch through the anthropic
live zone, both start within about two seconds of each other, and their
message counts differ by 0 or 2. So it is one logical conversation being
driven by two concurrent request streams — parallel subagents, most likely —
that the proxy hashes to a single `conversation_key`.

Two readings, and they need different fixes:

1. **Real thrash.** The two streams genuinely send different bodies past the
   tools block, so each one's prefix misses. That is real money, roughly
   90K tokens per turn on this conversation alone, and it points at
   non-deterministic transform output for the same conversation under
   concurrency. This is the concrete case behind item 3b.
2. **Phantom waste.** `conversation_key` is too coarse and merges two
   genuinely separate prefixes, so every alternation *looks* like drift and
   the waste is an accounting artefact.

Distinguishing them is the first job here, and it is cheap: log the prefix
hash alongside `conversation_key` on both streams and see whether the two
streams disagree about the body or merely about the key. Note that
`wasted_tokens` here feeds item 3's "waste exceeds savings" conclusion, so
reading 2 would soften item 3 considerably. Do not act on item 3 before
settling this.


## Item 11a

*moved from `docs/notes/proxy-experiments-2026-08.md`*

### 11a. Second observation window — the pinned value crosses keys

A later cluster (2026-08-08 21:45-21:53Z, 18 events, 792,392 nominal wasted
tokens across 8 keys) reproduces the alternation exactly. Key
`eaf0d2bf34c6a53c`:

```
21:51:37  acr=22234  expected=57928  wasted=35694
21:51:39  acr=57928  expected=61259  wasted= 3331
21:52:10  acr=22234  expected=80416  wasted=58182
21:52:11  acr=80416  expected=82803  wasted= 2259
21:52:44  acr=22234  expected=88212  wasted=65978
21:52:51  acr=88212  expected=91799  wasted= 3455
```

The odd turns pin at **22234** while `expected` climbs 57928 → 88212; the even
turns read back exactly what the previous turn expected. Same shape as the
13907 run above, different constant.

**The new fact: 22234 is also the pinned value under key `8d49a3ca5a4befbb`**
(21:47:54, wasted 81185). One constant, two supposedly distinct conversations.

This is evidence *against* the pure-artefact reading in 2. If a too-coarse key
were merging two streams, the pinned value would be that shared prefix's size
and would differ between unrelated conversations. A single constant appearing
across separate keys instead suggests a real cache floor — some stream reads
only the leading system+tools block, and that block is the same size for both
conversations because it is the same client.

So the two readings are no longer symmetric. Reading 1 (real thrash) now has
the better fit, and the earlier note that the source "tips this toward the
artefact reading" is too strong — the key *can* collide by design, but that
mechanism does not explain a constant shared across keys.

Still not settled: the prefix-hash check remains the decider. But if this holds,
item 3's waste is real money and must not be discounted.

**Source.** The key comes from `derive_session_key`
(`cache_stabilization/drift_detector.rs:510-548`), called from
`proxy.rs:2393-2422` alongside `compute_structural_hash` and `observe_drift`.
The event itself is emitted by `usage_observer.rs:383-470`, with classification
via `classify_turn` / `TurnClass::Recache` at :140-167.

Reading the source makes reading 2 the more likely of the two. With no
`x-headroom-session-id` header, the key is:

```
auth:<hash(token)>:<conversation_discriminator>
```

and the discriminator is a fingerprint of `(model, first conversation message)`
— documented at :550-567 as *deliberately* excluding the system prompt and
tools, because those are the axes being measured. Two parallel subagents on one
auth token, same model, same opener therefore collide by design. The key cannot
tell them apart, which is precisely the merge reading 2 describes.

That is not proof of reading 2. The mechanism explains how a collision happens
without proving the bodies match past the tools block — the prefix-hash check
above is still what settles it. But it inverts the burden: the collision is
expected behaviour, so treat the waste figure as suspect until shown otherwise.

Worth noting: recache events *do* carry `conversation_key`, but PERF lines do
not. That asymmetry is what made item 1c hard to pin down, and it is a
one-field fix.
